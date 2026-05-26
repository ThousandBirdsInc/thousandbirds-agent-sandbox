#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
LIBKRUN_DIR=${LIBKRUN_DIR:-"$ROOT_DIR/../thousandbirds-libkrun"}
VARIANT=${1:-debian-bookworm}
RUNTIME=${RUNTIME:-auto}
PLATFORM=${PLATFORM:-linux/arm64}
SIZE_MB=${SIZE_MB:-8192}
OUTPUT_ROOT=${OUTPUT_ROOT:-"$ROOT_DIR/target/agent-images"}
IMAGE_PREFIX=${IMAGE_PREFIX:-tb-agent-sandbox}

usage() {
  cat <<EOF
Usage: $0 [debian-bookworm|debian-bookworm-rust|debian-bookworm-go|ubuntu-noble]

Environment:
  RUNTIME=docker|podman|auto      Container runtime for build/export (default: auto)
  PULL=1                          Pull Dockerfile base images during container build
  PLATFORM=linux/arm64|linux/amd64 Build platform (default: linux/arm64)
  SIZE_MB=N                       Root disk size in MiB (default: 8192)
  OUTPUT_ROOT=PATH                Bundle output root (default: target/agent-images)
  BASE_BUNDLE=PATH                Existing libkrun bundle with kernel/initramfs
  KERNEL=PATH                     Kernel path override
  INITRAMFS=PATH                  Initramfs path override
  NO_INITRAMFS=1                  Build a direct-boot image with no initramfs
                                  (needs a guest kernel with built-in
                                  CONFIG_VIRTIO_BLK + CONFIG_EXT4_FS; see
                                  design doc #E). Removes the initramfs load
                                  from the boot path.
  LIBKRUN_DIR=PATH                Sibling libkrun checkout
EOF
}

case "$VARIANT" in
  debian-bookworm|debian-bookworm-rust|debian-bookworm-go|ubuntu-noble) ;;
  -h|--help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 64
    ;;
esac

containerfile="$ROOT_DIR/images/$VARIANT.Containerfile"
tag="$IMAGE_PREFIX:$VARIANT"
output_dir="$OUTPUT_ROOT/$VARIANT"

pick_runtime() {
  case "$RUNTIME" in
    docker|podman)
      command -v "$RUNTIME" >/dev/null 2>&1 || {
        echo "$RUNTIME is not installed or not on PATH" >&2
        exit 69
      }
      printf "%s\n" "$RUNTIME"
      ;;
    auto)
      if command -v docker >/dev/null 2>&1; then
        printf "docker\n"
      elif command -v podman >/dev/null 2>&1; then
        printf "podman\n"
      else
        echo "Neither docker nor podman is installed or on PATH" >&2
        exit 69
      fi
      ;;
    *)
      echo "Invalid RUNTIME=$RUNTIME" >&2
      exit 64
      ;;
  esac
}

runtime=$(pick_runtime)

case "$PLATFORM" in
  linux/arm64)
    kernel_format=2
    architecture=aarch64
    console=ttyAMA0
    ;;
  linux/amd64)
    kernel_format=1
    architecture=x86_64
    console=ttyS0
    ;;
  *)
    echo "unsupported PLATFORM=$PLATFORM" >&2
    exit 64
    ;;
esac

build_image() {
  containerfile=$1
  tag_name=$2
  allow_pull=${3:-1}
  if [ "${PULL:-0}" = "1" ] && [ "$allow_pull" = "1" ]; then
    "$runtime" build \
      --pull \
      --platform "$PLATFORM" \
      -f "$containerfile" \
      -t "$tag_name" \
      "$ROOT_DIR"
  else
    "$runtime" build \
      --platform "$PLATFORM" \
      -f "$containerfile" \
      -t "$tag_name" \
      "$ROOT_DIR"
  fi
}

if [ "$VARIANT" = "debian-bookworm-rust" ] || [ "$VARIANT" = "debian-bookworm-go" ]; then
  build_image "$ROOT_DIR/images/debian-bookworm.Containerfile" "$IMAGE_PREFIX:debian-bookworm"
  build_pull=0
else
  build_pull=1
fi

build_image "$containerfile" "$tag" "$build_pull"

overlay_dir=$(mktemp -d "${TMPDIR:-/tmp}/tb-agent-overlay.XXXXXX")
overlay_tar="$overlay_dir/overlay.tar"
cleanup() {
  rm -rf "$overlay_dir"
}
trap cleanup EXIT INT TERM

mkdir -p \
  "$overlay_dir/root/usr/local/bin" \
  "$overlay_dir/root/etc/systemd/system" \
  "$overlay_dir/root/etc/systemd/system/multi-user.target.wants"
cp "$ROOT_DIR/guest/agent-sandbox-import-credentials" "$overlay_dir/root/usr/local/bin/"
cp "$ROOT_DIR/guest/agent-sandbox-shell" "$overlay_dir/root/usr/local/bin/"
sed "s|@SERIAL_TTY@|/dev/$console|g" \
  "$ROOT_DIR/guest/agent-sandbox-shell.service" \
  > "$overlay_dir/root/etc/systemd/system/agent-sandbox-shell.service"
ln -sf ../agent-sandbox-shell.service \
  "$overlay_dir/root/etc/systemd/system/multi-user.target.wants/agent-sandbox-shell.service"
# Mask the deprecated systemd-udev-settle.service. On a virtio guest with
# devtmpfs, devices appear synchronously, so the settle barrier only adds
# startup latency on the critical path to sysinit.target.
ln -sf /dev/null \
  "$overlay_dir/root/etc/systemd/system/systemd-udev-settle.service"
chmod 0755 \
  "$overlay_dir/root/usr/local/bin/agent-sandbox-import-credentials" \
  "$overlay_dir/root/usr/local/bin/agent-sandbox-shell"
chmod 0644 "$overlay_dir/root/etc/systemd/system/agent-sandbox-shell.service"
(cd "$overlay_dir/root" && tar -cpf "$overlay_tar" .)

kernel=${KERNEL:-}
initramfs=${INITRAMFS:-}
no_initramfs=${NO_INITRAMFS:-}
if [ -n "${BASE_BUNDLE:-}" ]; then
  kernel=${kernel:-"$BASE_BUNDLE/kernel"}
  if [ -z "$no_initramfs" ]; then
    initramfs=${initramfs:-"$BASE_BUNDLE/initramfs"}
  fi
fi

if [ -z "$kernel" ] || [ ! -f "$kernel" ]; then
  echo "KERNEL or BASE_BUNDLE is required and must point to an existing kernel" >&2
  exit 66
fi
if [ -n "$no_initramfs" ]; then
  initramfs=""
elif [ -z "$initramfs" ] || [ ! -f "$initramfs" ]; then
  echo "INITRAMFS or BASE_BUNDLE is required and must point to an existing initramfs" >&2
  echo "(or set NO_INITRAMFS=1 to build a direct-boot image)" >&2
  exit 66
fi

rm -rf "$output_dir"
mkdir -p "$output_dir"

# --initramfs is optional in the rootfs tool; omit it for a direct-boot image.
if [ -n "$initramfs" ]; then
  set -- --initramfs "$initramfs"
else
  set --
fi

"$LIBKRUN_DIR/examples/os_mode_build_container_rootfs.py" \
  --image "$tag" \
  --output-dir "$output_dir" \
  --runtime "$runtime" \
  --platform "$PLATFORM" \
  --size-mb "$SIZE_MB" \
  --init-mode systemd \
  --overlay-tar "$overlay_tar" \
  --kernel "$kernel" \
  --kernel-format "$kernel_format" \
  "$@" \
  --root-device /dev/vda \
  --root-fstype ext4 \
  --root-options rw \
  --no-smoke-poweroff-after-ready \
  --smoke-timeout 180

cp "$kernel" "$output_dir/kernel"
# Direct-boot images omit the initramfs line entirely.
if [ -n "$initramfs" ]; then
  cp "$initramfs" "$output_dir/initramfs"
  initramfs_json='  "initramfs": "initramfs",'
else
  initramfs_json=''
fi

cat > "$output_dir/image.json" <<EOF
{
  "architecture": "$architecture",
  "kernel": "kernel",
$initramfs_json
  "root_disk": "root.raw",
  "kernel_format": $kernel_format,
  "kernel_cmdline": "rw",
  "root_device": "/dev/vda",
  "root_fstype": "ext4",
  "root_options": "rw",
  "init": "/sbin/init",
  "console": "$console",
  "guest_user": "agent",
  "installed_tools": ["git", "ripgrep", "node", "npm", "claude", "codex"]
}
EOF

echo "Built $VARIANT image bundle:"
echo "  $output_dir"
echo
echo "Run it with:"
echo "  cargo run -- shell --image '$output_dir' --workspace ."
