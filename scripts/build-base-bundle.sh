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
  INITRAMFS=PATH                  Initramfs path override
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

kernel=${KERNEL:-}
initramfs=${INITRAMFS:-}
if [ -n "${BASE_BUNDLE:-}" ]; then
  kernel=${kernel:-"$BASE_BUNDLE/kernel"}
  initramfs=${initramfs:-"$BASE_BUNDLE/initramfs"}
fi

if [ -z "$kernel" ] || [ ! -f "$kernel" ]; then
  echo "KERNEL or BASE_BUNDLE is required and must point to an existing kernel" >&2
  exit 66
fi
if [ -z "$initramfs" ] || [ ! -f "$initramfs" ]; then
  echo "INITRAMFS or BASE_BUNDLE is required and must point to an existing initramfs" >&2
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
    console=ttyS0
    ;;
  *)
    echo "unsupported PLATFORM=$PLATFORM" >&2
    exit 64
    ;;
esac

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

"$LIBKRUN_DIR/examples/os_mode_build_container_rootfs.py" \
  --image "$SOURCE_IMAGE" \
  --output-dir "$build_dir/rootfs" \
  --runtime "$runtime" \
  --platform "$PLATFORM" \
  --size-mb "$SIZE_MB" \
  --init-mode systemd \
  --kernel "$kernel" \
  --kernel-format "$kernel_format" \
  --initramfs "$initramfs" \
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
cp "$initramfs" "$OUTPUT_DIR/initramfs"

cat > "$OUTPUT_DIR/manifest.json" <<EOF
{
  "kind": "libkrun.os-bundle.v1",
  "architecture": "$architecture",
  "kernel": "kernel",
  "initramfs": "initramfs",
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

echo "Built libkrun OS bundle:"
echo "  $OUTPUT_DIR"
echo
echo "Use it with:"
echo "  BASE_BUNDLE='$OUTPUT_DIR' scripts/build-agent-image.sh debian-bookworm"
