#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
LIBKRUN_DIR=${LIBKRUN_DIR:-"$ROOT_DIR/../thousandbirds-libkrun"}
TB_SANDBOX=${TB_SANDBOX:-"$ROOT_DIR/target/debug/tb-sandbox"}
DEFAULT_IMAGE=${DEFAULT_IMAGE:-"$ROOT_DIR/target/agent-images/debian-bookworm"}
BROWSER_IMAGE=${BROWSER_IMAGE:-"$ROOT_DIR/target/agent-images/debian-bookworm-browser"}
SHELL_ID=${SHELL_ID:-example-shell}
BROWSER_ID=${BROWSER_ID:-my-browser}
MEMORY_MIB=${MEMORY_MIB:-4096}

usage() {
  cat <<EOF
Usage: $0 COMMAND

Commands:
  shell              Run an ephemeral shell sandbox
  browser-create     Create/reuse the browser sandbox
  browser-headless   Print the two commands for CDP browser testing
  browser-vnc        Print the two commands for macOS VNC browser testing
  browser-status     Show browser status in the browser sandbox
  browser-stop       Stop the browser process in the browser sandbox
  delete-browser     Delete the browser sandbox

Environment:
  DEFAULT_IMAGE=PATH       Default agent image
  BROWSER_IMAGE=PATH       Browser agent image
  BROWSER_ID=NAME          Browser sandbox name (default: my-browser)
  MEMORY_MIB=N             Browser sandbox memory (default: 4096)
EOF
}

require_file() {
  if [ ! -f "$1" ]; then
    echo "missing required file: $1" >&2
    echo "run: make setup" >&2
    exit 66
  fi
}

require_image() {
  if [ ! -f "$1/image.json" ]; then
    echo "missing image: $1" >&2
    echo "run: make setup-images" >&2
    exit 66
  fi
}

run_tb() {
  require_file "$TB_SANDBOX"
  if [ -d "$LIBKRUN_DIR/target/release" ]; then
    TB_SANDBOX_LIBKRUN_DIR=${TB_SANDBOX_LIBKRUN_DIR:-"$LIBKRUN_DIR/target/release"} \
      "$TB_SANDBOX" "$@"
  else
    "$TB_SANDBOX" "$@"
  fi
}

sandbox_exists() {
  run_tb list | awk '{print $1}' | grep -Fx "$1" >/dev/null 2>&1
}

browser_create() {
  require_image "$BROWSER_IMAGE"
  if sandbox_exists "$BROWSER_ID"; then
    echo "browser sandbox already exists: $BROWSER_ID"
    return 0
  fi
  run_tb create \
    --image "$BROWSER_IMAGE" \
    --workspace "$ROOT_DIR" \
    --name "$BROWSER_ID" \
    --memory-mib "$MEMORY_MIB"
}

case "${1:-}" in
  shell)
    require_image "$DEFAULT_IMAGE"
    run_tb shell --image "$DEFAULT_IMAGE" --workspace "$ROOT_DIR" --ephemeral
    ;;
  browser-create)
    browser_create
    ;;
  browser-headless)
    browser_create
    cat <<EOF
Run these in two terminals:

  make browser-boot

  make browser-cdp

The boot command exits once the session is running. Keep browser-cdp running
while testing; it holds the forwarded cdp_url open.
EOF
    ;;
  browser-vnc)
    browser_create
    cat <<EOF
Run these in two terminals:

  make browser-boot

  make browser-view

On macOS, make browser-view opens the forwarded vnc:// URL with Screen Sharing.
The boot command exits once the session is running. Keep browser-view running
while viewing the browser; it holds the VNC forward open.
EOF
    ;;
  browser-status)
    run_tb browser status "$BROWSER_ID"
    ;;
  browser-stop)
    run_tb browser stop "$BROWSER_ID"
    ;;
  delete-browser)
    run_tb delete "$BROWSER_ID"
    ;;
  -h|--help|"")
    usage
    ;;
  *)
    usage >&2
    exit 64
    ;;
esac
