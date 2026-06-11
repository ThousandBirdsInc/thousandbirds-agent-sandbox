#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
LIBKRUN_DIR=${LIBKRUN_DIR:-"$ROOT_DIR/../thousandbirds-libkrun"}

case "$(uname -s)" in
  Darwin)
    arch=$(uname -m)
    case "$arch" in
      arm64) triplet=aarch64-linux-gnu ;;
      *) triplet="${arch}-linux-gnu" ;;
    esac
    gcc_version=${GCC_VERSION:-12}
    sysroot=${SYSROOT_LINUX:-"$LIBKRUN_DIR/linux-sysroot"}
    gcc_lib_dir="$sysroot/usr/lib/gcc/$triplet/$gcc_version"
    exec /usr/bin/clang \
      -target "$triplet" \
      -fuse-ld=lld \
      -Wl,-strip-debug \
      --sysroot "$sysroot" \
      -B"$gcc_lib_dir" \
      -L"$gcc_lib_dir" \
      -Wno-c23-extensions \
      "$@"
    ;;
  *)
    exec "${CC:-cc}" "$@"
    ;;
esac
