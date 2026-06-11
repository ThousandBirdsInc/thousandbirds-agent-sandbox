SHELL := /bin/sh

.PHONY: \
	setup setup-all setup-images setup-browser-image direct-boot-image \
	shell \
	browser-create browser-headless browser-vnc browser-boot browser-cdp browser-view \
	browser-status browser-stop delete-browser \
	check

setup:
	scripts/setup-local.sh

setup-all:
	scripts/setup-local.sh --images --browser-image

setup-images:
	scripts/setup-local.sh --images

setup-browser-image:
	scripts/setup-local.sh --browser-image

# Repackage an existing agent image as a no-initramfs direct-boot image
# (~29% faster cold boot, ~47% lower idle footprint). Requires a guest kernel
# with virtio_blk + ext4 built in (e.g. libkrunfw's Image.gz):
#   SRC_IMAGE=target/agent-images/debian-bookworm KERNEL=/path/to/Image.gz make direct-boot-image
direct-boot-image:
	scripts/make-direct-boot-image.sh

shell:
	scripts/run-example.sh shell

browser-create:
	scripts/run-example.sh browser-create

browser-headless:
	scripts/run-example.sh browser-headless

browser-vnc:
	scripts/run-example.sh browser-vnc

browser-boot:
	cargo run -- start $${BROWSER_ID:-my-browser}
	@echo "VM session is running in the background. Next: make browser-view"

browser-cdp:
	cargo run -- browser start $${BROWSER_ID:-my-browser}

browser-view:
	cargo run -- browser view $${BROWSER_ID:-my-browser}

browser-status:
	scripts/run-example.sh browser-status

browser-stop:
	scripts/run-example.sh browser-stop

delete-browser:
	scripts/run-example.sh delete-browser

check:
	cargo fmt --check
	cargo check
	sh -n scripts/setup-local.sh
	sh -n scripts/run-example.sh
	sh -n scripts/cc-linux.sh
	sh -n scripts/build-base-bundle.sh
	sh -n scripts/build-agent-image.sh
	sh -n scripts/make-direct-boot-image.sh
	bash -n scripts/release-macos.sh
