#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
LIBKRUN_DIR=${LIBKRUN_DIR:-"$ROOT_DIR/../thousandbirds-libkrun"}
RUNTIME=${RUNTIME:-auto}
PLATFORM=${PLATFORM:-linux/arm64}
SIZE_MB=${SIZE_MB:-4096}
OUTPUT_DIR=${OUTPUT_DIR:-"$ROOT_DIR/target/base-bundles/debian-systemd/libkrun-os-bundle"}
SOURCE_IMAGE=${SOURCE_IMAGE:-tb-agent-sandbox-base:debian-systemd}
SOURCE_CONTAINERFILE=${SOURCE_CONTAINERFILE:-"$ROOT_DIR/images/base-debian-systemd.Containerfile"}

usage() {
  cat <<EOF
Usage: $0

Build a reusable libkrun-os-bundle directory for agent image builds.

Environment:
  BASE_BUNDLE=PATH                Existing bundle to reuse kernel/initramfs from
  KERNEL=PATH                     Kernel path override
  BUILTIN_KERNEL=PATH             Built-in-driver kernel for a direct-boot bundle
  INITRAMFS=PATH                  Initramfs path override
  KERNEL_FORMAT=N                 libkrun kernel format (default by platform)
  NO_INITRAMFS=1|0                Direct boot (1), legacy initramfs (0), or auto
  LIBKRUNFW_DIR=PATH              libkrunfw checkout to auto-locate Image.gz
                                  (a direct-boot bundle is the default when one
                                  is found; see README Startup Performance Notes)
  SOURCE_IMAGE=IMAGE              Base container image (default: tb-agent-sandbox-base:debian-systemd)
  SOURCE_CONTAINERFILE=PATH       Dockerfile/Containerfile to build SOURCE_IMAGE from
  SKIP_SOURCE_BUILD=1             Use SOURCE_IMAGE as-is instead of building it first
  RUNTIME=docker|podman|auto      Container runtime (default: auto)
  PULL=1                          Pull Dockerfile base images while building SOURCE_IMAGE
  PLATFORM=linux/arm64|linux/amd64 Build platform (default: linux/arm64)
  SIZE_MB=N                       Root disk size in MiB (default: 4096)
  OUTPUT_DIR=PATH                 Bundle output directory
  LIBKRUN_DIR=PATH                Sibling libkrun checkout

Examples:
  BASE_BUNDLE=../thousandbirds-libkrun/os_mode_artifacts/.../libkrun-os-bundle \\
    scripts/build-base-bundle.sh

  KERNEL=/path/to/kernel INITRAMFS=/path/to/initramfs \\
    scripts/build-base-bundle.sh
EOF
}

case "${1:-}" in
  -h|--help)
    usage
    exit 0
    ;;
  "")
    ;;
  *)
    usage >&2
    exit 64
    ;;
esac

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

# --- Kernel + initramfs / direct-boot resolution ---------------------------
#
# A guest kernel with virtio_blk + ext4 built in (e.g. libkrunfw's Image.gz)
# mounts /dev/vda directly, so the bundle needs no initramfs. That "direct-boot"
# path is ~29% faster to cold-boot and ~47% smaller in idle footprint than the
# initramfs path (see README "Startup Performance Notes"), so it is the default
# whenever such a kernel can be found.
#
#   NO_INITRAMFS=1   require direct boot (fail if no built-in-driver kernel)
#   NO_INITRAMFS=0   force the legacy initramfs bundle
#   (unset)          auto: direct-boot if a built-in-driver kernel is located,
#                    else fall back to the seed's initramfs kernel
#   BUILTIN_KERNEL=PATH / KERNEL=PATH   pin the kernel explicitly
#   LIBKRUNFW_DIR=PATH                  libkrunfw checkout to auto-locate Image.gz
no_initramfs=${NO_INITRAMFS:-}
kernel=${KERNEL:-${BUILTIN_KERNEL:-}}
initramfs=${INITRAMFS:-}

# Auto-locate a built-in-driver kernel from a libkrunfw checkout. arm64 only —
# the verified direct-boot path; amd64 stays on the initramfs path unless a
# kernel is pinned explicitly.
autolocate_builtin_kernel() {
  [ "$PLATFORM" = "linux/arm64" ] || return 1
  for dir in \
    "${LIBKRUNFW_DIR:-}" \
    "$ROOT_DIR/../libkrunfw" \
    "$LIBKRUN_DIR/../libkrunfw"; do
    [ -n "$dir" ] || continue
    cand=$(ls -1 "$dir"/linux-*/arch/arm64/boot/Image.gz 2>/dev/null | head -1 || true)
    if [ -n "$cand" ] && [ -f "$cand" ]; then
      printf "%s\n" "$cand"
      return 0
    fi
  done
  return 1
}

if [ -z "$kernel" ] && [ "$no_initramfs" != "0" ]; then
  if builtin_kernel=$(autolocate_builtin_kernel); then
    kernel=$builtin_kernel
    no_initramfs=1
    echo "==> Using built-in-driver kernel for direct boot: $kernel"
  fi
fi

if [ -n "${BASE_BUNDLE:-}" ]; then
  kernel=${kernel:-"$BASE_BUNDLE/kernel"}
  if [ "$no_initramfs" != "1" ]; then
    initramfs=${initramfs:-"$BASE_BUNDLE/initramfs"}
  fi
fi

if [ -z "$kernel" ] || [ ! -f "$kernel" ]; then
  echo "KERNEL or BASE_BUNDLE is required and must point to an existing kernel" >&2
  exit 66
fi
if [ "$no_initramfs" = "1" ]; then
  initramfs=""
elif [ -z "$initramfs" ] || [ ! -f "$initramfs" ]; then
  echo "INITRAMFS or BASE_BUNDLE is required and must point to an existing initramfs" >&2
  echo "(or set NO_INITRAMFS=1 to build a direct-boot bundle with a built-in-driver kernel)" >&2
  exit 66
fi

case "$PLATFORM" in
  linux/arm64)
    kernel_format=2
    architecture=aarch64
    console=ttyAMA0
    ;;
  linux/amd64)
    kernel_format=1
    architecture=x86_64
    # libkrun on x86_64 exposes a virtio-console (hvc0), not an 8250 ttyS0; the
    # console name also picks the guest agent-shell's serial tty (/dev/$console).
    console=hvc0
    ;;
  *)
    echo "unsupported PLATFORM=$PLATFORM" >&2
    exit 64
    ;;
esac

# A gzip-compressed arm64 Image (Image.gz) loads with format 2 (libkrun's PeGz
# loader scans for the gzip magic); override for other kernel encodings.
kernel_format=${KERNEL_FORMAT:-$kernel_format}

runtime=$(pick_runtime)

if [ "${SKIP_SOURCE_BUILD:-0}" != "1" ]; then
  if [ ! -f "$SOURCE_CONTAINERFILE" ]; then
    echo "SOURCE_CONTAINERFILE does not exist: $SOURCE_CONTAINERFILE" >&2
    exit 66
  fi
  if [ "${PULL:-0}" = "1" ]; then
    "$runtime" build \
      --pull \
      --platform "$PLATFORM" \
      -f "$SOURCE_CONTAINERFILE" \
      -t "$SOURCE_IMAGE" \
      "$ROOT_DIR"
  else
    "$runtime" build \
      --platform "$PLATFORM" \
      -f "$SOURCE_CONTAINERFILE" \
      -t "$SOURCE_IMAGE" \
      "$ROOT_DIR"
  fi
fi

build_dir=$(mktemp -d "${TMPDIR:-/tmp}/tb-base-bundle.XXXXXX")
cleanup() {
  rm -rf "$build_dir"
}
trap cleanup EXIT INT TERM

# --initramfs is optional in the rootfs tool; omit it for a direct-boot bundle.
if [ -n "$initramfs" ]; then
  set -- --initramfs "$initramfs"
else
  set --
fi

"$LIBKRUN_DIR/examples/os_mode_build_container_rootfs.py" \
  --image "$SOURCE_IMAGE" \
  --output-dir "$build_dir/rootfs" \
  --runtime "$runtime" \
  --platform "$PLATFORM" \
  --size-mb "$SIZE_MB" \
  --init-mode systemd \
  --kernel "$kernel" \
  --kernel-format "$kernel_format" \
  "$@" \
  --root-device /dev/vda \
  --root-fstype ext4 \
  --root-options rw \
  --no-smoke-poweroff-after-ready \
  --smoke-timeout 180

rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR"
cp "$build_dir/rootfs/root.raw" "$OUTPUT_DIR/root.raw"
cp "$build_dir/rootfs/manifest.json" "$OUTPUT_DIR/source-manifest.json"
cp "$kernel" "$OUTPUT_DIR/kernel"

# Direct-boot bundles omit the initramfs file and its manifest key entirely.
if [ -n "$initramfs" ]; then
  cp "$initramfs" "$OUTPUT_DIR/initramfs"
  initramfs_json='
  "initramfs": "initramfs",'
else
  initramfs_json=''
fi

cat > "$OUTPUT_DIR/manifest.json" <<EOF
{
  "kind": "libkrun.os-bundle.v1",
  "architecture": "$architecture",
  "kernel": "kernel",$initramfs_json
  "root_disk": "root.raw",
  "kernel_format": $kernel_format,
  "root_device": "/dev/vda",
  "root_fstype": "ext4",
  "root_options": "rw",
  "init": "/sbin/init",
  "console": "$console",
  "source_image": "$SOURCE_IMAGE"
}
EOF

echo "Built libkrun OS bundle ($([ -n "$initramfs" ] && echo "with initramfs" || echo "direct-boot, no initramfs")):"
echo "  $OUTPUT_DIR"
echo
echo "Use it with:"
echo "  BASE_BUNDLE='$OUTPUT_DIR' scripts/build-agent-image.sh debian-bookworm"
