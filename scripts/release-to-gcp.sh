#!/usr/bin/env bash
# Ship a disk-only sandbox snapshot from this machine (typically macOS/arm64)
# to a Google Cloud Storage bucket, then cold-boot it on a remote GCP node.
#
# WHY DISK-ONLY: a disk-only snapshot is just `root.raw` + `manifest.json` with
# no CPU/GIC/hypervisor state, so it carries nothing that is HVF- or
# Apple-specific. `restore` cold-boots it from the snapshotted disk. This is the
# only snapshot kind that travels reliably between hosts. A `--capture-memory`
# (Full) snapshot embeds live aarch64 vCPU/GIC/device state captured through
# Hypervisor.framework and CANNOT be resumed on a GCP KVM node — this script
# refuses to push one.
#
# THE ARCH WALL: the snapshotted rootfs holds binaries for the guest arch it was
# built for. A snapshot built on Apple Silicon is aarch64, so it can only
# cold-boot on a GCP **Arm** node (Axion C4A or Tau T2A / Ampere Altra). It will
# NOT boot on the default x86_64 machine types.
#
# Subcommands:
#   push    SANDBOX_ID gs://BUCKET/PREFIX [--name SNAP] [--no-freeze] [--keep-local]
#           Run on the source host. Takes a disk-only snapshot of SANDBOX_ID and
#           uploads its directory to the bucket. Prints the snapshot id.
#
#   resume  gs://BUCKET/PREFIX/SNAP --image-path DIR [--name NEW_ID] [restore opts...]
#           Run on the GCP node. Downloads the snapshot into the local state
#           root and restores it, pointing at the local agent image (which must
#           be the same guest arch as the snapshot). Extra args after
#           --image-path are forwarded to `tb-sandbox restore` verbatim.
#
# Env vars:
#   TB_SANDBOX        Path to the tb-sandbox binary. Default: tb-sandbox on PATH.
#   TB_SANDBOX_STATE_DIR  Overrides the state root (snapshots live under it).
#
# Examples:
#   # On macOS — snapshot and upload:
#   scripts/release-to-gcp.sh push my-sbx gs://tb-snaps/agents --name nightly
#
#   # On a GCP Arm node — download and cold-boot:
#   scripts/release-to-gcp.sh resume gs://tb-snaps/agents/nightly \
#       --image-path /opt/tb-sandbox/images/debian --name resumed-1
set -euo pipefail

TB_SANDBOX="${TB_SANDBOX:-tb-sandbox}"

die() { echo "error: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }

usage() {
  sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//; /^set -euo/d'
  exit "${1:-0}"
}

cmd_push() {
  local sandbox_id="${1:-}"; shift || true
  local dest="${1:-}"; shift || true
  [ -n "$sandbox_id" ] || usage 1
  [ -n "$dest" ] || usage 1
  case "$dest" in
    gs://*) ;;
    *) die "destination must be a gs:// URL, got: $dest" ;;
  esac

  local snap_name="" freeze_flag="" keep_local=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --name)        snap_name="${2:?--name needs a value}"; shift 2 ;;
      --no-freeze)   freeze_flag="--no-freeze"; shift ;;
      --keep-local)  keep_local=1; shift ;;
      *) die "unexpected argument to push: $1" ;;
    esac
  done

  need gcloud

  # Take a DISK-ONLY snapshot (no --capture-memory). `snapshot` prints the
  # snapshot directory path on stdout; the dir's basename is the snapshot id.
  local name_args=()
  [ -n "$snap_name" ] && name_args=(--name "$snap_name")
  echo "==> snapshotting $sandbox_id (disk-only, quiesced)" >&2
  local snap_dir
  snap_dir="$("$TB_SANDBOX" snapshot "$sandbox_id" "${name_args[@]}" ${freeze_flag:+$freeze_flag})"
  [ -d "$snap_dir" ] || die "snapshot did not produce a directory: $snap_dir"
  local snap_id
  snap_id="$(basename "$snap_dir")"

  # Guard: never ship a Full snapshot — its live state is host-locked.
  if grep -q '"kind": "full"' "$snap_dir/manifest.json"; then
    die "snapshot $snap_id is a Full snapshot; live memory state is not portable to GCP. Re-take without --capture-memory."
  fi

  local upload_dest="${dest%/}/$snap_id"
  echo "==> uploading $snap_dir -> $upload_dest" >&2
  gcloud storage cp -r "$snap_dir" "$upload_dest"

  if [ "$keep_local" -eq 0 ]; then
    echo "==> upload complete (local snapshot kept; pass --keep-local to silence)" >&2
  fi
  echo "$snap_id"
}

cmd_resume() {
  local src="${1:-}"; shift || true
  [ -n "$src" ] || usage 1
  case "$src" in
    gs://*) ;;
    *) die "source must be a gs:// URL, got: $src" ;;
  esac

  # Everything from --image-path onward is forwarded to `restore`, but we must
  # see --image-path to fail early with a clear message if it is missing.
  local image_path=""
  local restore_args=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --image-path) image_path="${2:?--image-path needs a value}"; restore_args+=("$1" "$2"); shift 2 ;;
      *) restore_args+=("$1"); shift ;;
    esac
  done
  [ -n "$image_path" ] || die "--image-path DIR is required (the local agent image, same guest arch as the snapshot)"
  [ -d "$image_path" ] || die "agent image directory not found: $image_path"

  need gcloud

  # Resolve the local state root the same way the binary does, then drop the
  # snapshot into <state>/snapshots/<snap_id> so `restore` finds it by id.
  local state_root
  if [ -n "${TB_SANDBOX_STATE_DIR:-}" ]; then
    state_root="$TB_SANDBOX_STATE_DIR"
  else
    # Linux default (GCP nodes): XDG_STATE_HOME or ~/.local/state.
    state_root="${XDG_STATE_HOME:-$HOME/.local/state}/thousandbirds/agent-sandbox"
  fi
  local snap_id
  snap_id="$(basename "${src%/}")"
  local snaps_root="$state_root/snapshots"
  mkdir -p "$snaps_root"
  local local_snap="$snaps_root/$snap_id"

  if [ -d "$local_snap" ]; then
    echo "==> snapshot $snap_id already present at $local_snap (skipping download)" >&2
  else
    echo "==> downloading $src -> $local_snap" >&2
    # `cp -r gs://.../SNAP <dir>` lands the contents under <dir>/SNAP.
    gcloud storage cp -r "${src%/}" "$snaps_root/"
    [ -d "$local_snap" ] || die "download did not produce $local_snap"
  fi

  if grep -q '"kind": "full"' "$local_snap/manifest.json"; then
    die "snapshot $snap_id is a Full snapshot; it cannot be restored on this host's hypervisor."
  fi

  echo "==> restoring $snap_id with image $image_path" >&2
  "$TB_SANDBOX" restore "$snap_id" "${restore_args[@]}"
}

main() {
  local sub="${1:-}"; shift || true
  case "$sub" in
    push)   cmd_push "$@" ;;
    resume) cmd_resume "$@" ;;
    ""|-h|--help|help) usage 0 ;;
    *) die "unknown subcommand: $sub (expected push|resume)" ;;
  esac
}

main "$@"
