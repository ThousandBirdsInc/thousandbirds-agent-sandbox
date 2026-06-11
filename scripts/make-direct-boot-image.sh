#!/bin/sh
# Convert an existing agent image into a "direct-boot" image that mounts
# /dev/vda from the kernel directly, with NO initramfs.
#
# The shipped agent images boot via a ~25 MB initramfs whose only job is to
# load the virtio_blk + ext4 kernel modules, find the root device, and pivot.
# A guest kernel built with CONFIG_VIRTIO_BLK=y + CONFIG_EXT4_FS=y (e.g. the
# libkrunfw 6.12.x kernel) needs none of that: the runner can mount /dev/vda
# directly. Dropping the initramfs removes both the pre-kernel initramfs load
# and the in-guest initramfs phase.
#
# Measured on Apple Silicon (debian-bookworm), median of 3 cold boots:
#   total startup      1007 ms -> 718 ms   (-29%)
#   guest kernel->ready  730 ms -> 500 ms
#   VM physical footprint 412 MiB -> 217 MiB (-47%, vmmap)
#
# The root disk is kernel-agnostic (a Debian ext4 filesystem; nothing in the
# agent workload loads out-of-tree modules), so this is a pure repackage: the
# existing root.raw is reused as-is and only the kernel + manifest change.
#
# Usage:
#   SRC_IMAGE=target/agent-images/debian-bookworm \
#   KERNEL=/path/to/builtin-driver/Image.gz \
#     scripts/make-direct-boot-image.sh
#   # -> target/agent-images/debian-bookworm-direct  (pass this as --image)
#
# Environment:
#   SRC_IMAGE=PATH      Existing agent image directory (required)
#   KERNEL=PATH         Built-in-driver guest kernel (required). On aarch64 a
#                       gzip-compressed Image (Image.gz) works with the default
#                       KERNEL_FORMAT=2 (libkrun's PeGz loader scans for the
#                       gzip magic); a raw uncompressed Image needs
#                       KERNEL_FORMAT=0.
#   KERNEL_FORMAT=N     libkrun kernel format. Default: inherit SRC_IMAGE's.
#   OUTPUT_DIR=PATH     Output image directory.
#                       Default: <SRC_IMAGE>-direct
set -eu

SRC_IMAGE=${SRC_IMAGE:-}
KERNEL=${KERNEL:-}
[ -n "$SRC_IMAGE" ] || { echo "SRC_IMAGE is required (an existing agent image directory)" >&2; exit 64; }
[ -n "$KERNEL" ] || { echo "KERNEL is required (a built-in-driver guest kernel)" >&2; exit 64; }
[ -d "$SRC_IMAGE" ] || { echo "SRC_IMAGE does not exist: $SRC_IMAGE" >&2; exit 66; }
[ -f "$SRC_IMAGE/image.json" ] || { echo "missing $SRC_IMAGE/image.json" >&2; exit 66; }
[ -f "$SRC_IMAGE/root.raw" ] || { echo "missing $SRC_IMAGE/root.raw" >&2; exit 66; }
[ -f "$KERNEL" ] || { echo "KERNEL does not exist: $KERNEL" >&2; exit 66; }

OUTPUT_DIR=${OUTPUT_DIR:-"${SRC_IMAGE%/}-direct"}

# kernel_format defaults to the source image's value (2 on aarch64, which is
# the PeGz loader and accepts a gzip-compressed Image).
src_format=$(KERNEL_FORMAT="${KERNEL_FORMAT:-}" SRC_IMAGE="$SRC_IMAGE" python3 - "$SRC_IMAGE/image.json" <<'PY'
import json, os, sys
with open(sys.argv[1]) as f:
    m = json.load(f)
override = os.environ.get("KERNEL_FORMAT", "")
print(override if override else m.get("kernel_format", 2))
PY
)

echo "==> Building direct-boot image"
echo "    src:    $SRC_IMAGE"
echo "    kernel: $KERNEL (format $src_format)"
echo "    out:    $OUTPUT_DIR"

rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR"

# APFS copy-on-write clone of the root disk when available (instant, shares
# blocks), else a plain copy.
cp -c "$SRC_IMAGE/root.raw" "$OUTPUT_DIR/root.raw" 2>/dev/null \
  || cp "$SRC_IMAGE/root.raw" "$OUTPUT_DIR/root.raw"
cp "$KERNEL" "$OUTPUT_DIR/kernel"

# Rewrite the manifest: point at the new kernel/format and DROP the initramfs
# key. image.rs falls back to a sibling `initramfs` file when the key is
# absent, so we must not write one into the output directory.
SRC_FORMAT="$src_format" python3 - "$SRC_IMAGE/image.json" "$OUTPUT_DIR/image.json" <<'PY'
import json, os, sys
with open(sys.argv[1]) as f:
    m = json.load(f)
m["kernel"] = "kernel"
m["kernel_format"] = int(os.environ["SRC_FORMAT"])
m.pop("initramfs", None)
with open(sys.argv[2], "w") as f:
    json.dump(m, f, indent=2)
    f.write("\n")
PY

echo "==> Done. Boot it with:"
echo "    tb-sandbox shell --image $OUTPUT_DIR --workspace ."
