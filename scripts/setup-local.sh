#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
LIBKRUN_DIR=${LIBKRUN_DIR:-"$ROOT_DIR/../thousandbirds-libkrun"}

usage() {
  cat <<EOF
Usage: $0 [--images] [--browser-image]

Build the Cargo-built host-side pieces needed by tb-sandbox examples.

Environment:
  LIBKRUN_DIR=PATH       Sibling libkrun checkout (default: ../thousandbirds-libkrun)
  LIBKRUNFW_DIR=PATH     libkrunfw checkout. When present, a built-in-driver
                         kernel is used to build a direct-boot (no-initramfs)
                         base bundle and images by default (~29% faster cold
                         boot, ~47% lower idle footprint). Set NO_INITRAMFS=0 to
                         keep the legacy initramfs path.
  BASE_BUNDLE=PATH      Existing libkrun OS bundle used when building images
  SEED_BUNDLE=PATH      Alias for BASE_BUNDLE when bootstrapping image builds

Options:
  --images              Also build the default debian-bookworm image
  --browser-image       Also build the debian-bookworm-browser image
EOF
}

build_default_image=0
build_browser_image=0
while [ $# -gt 0 ]; do
  case "$1" in
    --images)
      build_default_image=1
      shift
      ;;
    --browser-image)
      build_browser_image=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 64
      ;;
  esac
done

if [ ! -d "$LIBKRUN_DIR" ]; then
  echo "missing libkrun checkout: $LIBKRUN_DIR" >&2
  echo "set LIBKRUN_DIR=/path/to/thousandbirds-libkrun" >&2
  exit 66
fi

case "$(uname -s)" in
  Darwin)
    if [ ! -f "$LIBKRUN_DIR/linux-sysroot/.sysroot_ready" ]; then
      echo "==> Preparing libkrun Linux sysroot"
      (cd "$LIBKRUN_DIR" && make linux-sysroot/.sysroot_ready)
    fi
    ;;
esac

echo "==> Building tb-sandbox and tb-sandbox-runner"
(cd "$ROOT_DIR" && cargo build --bins)

case "$(uname -s)" in
  Darwin)
    echo "==> Signing tb-sandbox-runner with hypervisor entitlement"
    codesign --entitlements "$ROOT_DIR/packaging/os_mode.entitlements" --force -s - \
      "$ROOT_DIR/target/debug/tb-sandbox-runner"
    ;;
esac

base_bundle=${BASE_BUNDLE:-${SEED_BUNDLE:-}}
default_base_bundle="$ROOT_DIR/target/base-bundles/debian-systemd/libkrun-os-bundle"

if [ "$build_default_image" -eq 1 ] || [ "$build_browser_image" -eq 1 ]; then
  if [ -n "$base_bundle" ]; then
    if [ ! -d "$default_base_bundle" ]; then
      echo "==> Building local base bundle"
      BASE_BUNDLE="$base_bundle" "$ROOT_DIR/scripts/build-base-bundle.sh"
    fi
  elif [ ! -d "$default_base_bundle" ]; then
    echo "missing base bundle: $default_base_bundle" >&2
    echo "set BASE_BUNDLE=/path/to/libkrun-os-bundle or SEED_BUNDLE=/path/to/libkrun-os-bundle" >&2
    exit 66
  fi
fi

if [ "$build_default_image" -eq 1 ]; then
  echo "==> Building debian-bookworm image"
  BASE_BUNDLE="$default_base_bundle" "$ROOT_DIR/scripts/build-agent-image.sh" debian-bookworm
fi

if [ "$build_browser_image" -eq 1 ]; then
  echo "==> Building debian-bookworm-browser image"
  BASE_BUNDLE="$default_base_bundle" "$ROOT_DIR/scripts/build-agent-image.sh" debian-bookworm-browser
fi

echo
echo "Local setup complete."
echo "Examples:"
echo "  make shell"
echo "  make browser-vnc"
echo "  make browser-headless"
