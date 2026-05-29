# Thousand Birds Agent Sandbox

```
 cargo run -- shell \
    --image target/agent-images/debian-bookworm \
    --workspace . \
    --credentials claude,codex \
    --gvproxy
```

`thousandbirds-agent-sandbox` creates libkrun-backed Linux sandboxes for
interactive coding agents. The default workflow is to boot a prepared sandbox,
attach your terminal to a guest shell as `agent`, and then run `claude`,
`codex`, or ordinary shell commands from inside the sandbox.

The wrapper uses `../thousandbirds-libkrun/examples/os_mode` for the actual VM
launch. That launcher must include the local `--serial-pty` support used by this
wrapper.

## Build

```sh
cargo build
```

## Full Local Flow

From a clean checkout with `../thousandbirds-libkrun` next to this repository:

```sh
export SEED_BUNDLE="../thousandbirds-libkrun/os_mode_artifacts/debian-recording-cache/libkrun-osmode-debian-systemd-bundle-d711a9d1ad3a73b1/libkrun-os-bundle"

BASE_BUNDLE="$SEED_BUNDLE" \
  scripts/build-base-bundle.sh

BASE_BUNDLE=target/base-bundles/debian-systemd/libkrun-os-bundle \
  scripts/build-agent-image.sh debian-bookworm

cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace .
```

For a fully repo-local smoke run, keep sandbox state under `target/`:

```sh
TB_SANDBOX_STATE_DIR=target/run-state \
TB_SANDBOX_CACHE_DIR=target/run-cache \
  cargo run -- shell \
    --image target/agent-images/debian-bookworm \
    --workspace .
```

Inside the guest shell:

```sh
claude
codex
```

By default the wrapper starts a host-side HTTP proxy, injects
`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` into the guest, and keeps the proxy alive
for the lifetime of the VM when a network backend is configured. Plain HTTP
requests pass through the proxy directly. HTTPS `CONNECT` traffic is tunneled
byte-for-byte unless at least one `--proxy-header` or `--proxy-header-env` rule
matches the target host; only those matched hosts are TLS-intercepted so the
proxy can substitute headers. This keeps WebSocket traffic such as Codex's
`wss://chatgpt.com/backend-api/codex/responses` on a raw tunnel by default.

When TLS interception is needed, the proxy creates an ephemeral CA, mounts only
the CA certificate into the guest, imports it into the guest trust store, and
sets common runtime trust variables including `NODE_EXTRA_CA_CERTS`,
`REQUESTS_CA_BUNDLE`, `SSL_CERT_FILE`, `GIT_SSL_CAINFO`, `NPM_CONFIG_CAFILE`,
and `CARGO_HTTP_CAINFO`.

The guest still needs a virtio-net backend to reach that host proxy. On macOS,
use `gvproxy`, the host-side network proxy from the `gvisor-tap-vsock` project
that Podman ships for VM networking. Install Podman with Homebrew:

```sh
brew install podman
```

That normally installs `gvproxy` at:

```sh
$(brew --prefix podman)/libexec/podman/gvproxy
```

Use `--gvproxy` to let `tb-sandbox` start `gvproxy`, create a per-sandbox socket,
wait for it, and stop it when the VM exits:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --credentials claude,codex \
  --gvproxy
```

If `gvproxy` is not on `PATH` or in the usual Homebrew Podman location, pass it
explicitly:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --credentials claude,codex \
  --gvproxy-bin "$(brew --prefix podman)/libexec/podman/gvproxy"
```

You can still run `gvproxy` yourself when you need to capture packets or reuse
an existing socket. Start it in one terminal and choose a socket path under
`/private/tmp`:

```sh
mkdir -p /private/tmp/tb-sandbox-net

GVPROXY="$(brew --prefix podman)/libexec/podman/gvproxy"
"$GVPROXY" \
  --debug \
  --listen-vfkit unixgram:///private/tmp/tb-sandbox-net/gvproxy.sock
```

Then pass the raw socket path to the sandbox from another terminal:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --credentials claude,codex \
  --gvproxy-socket /private/tmp/tb-sandbox-net/gvproxy.sock
```

The `gvproxy` command takes the socket as a URI:
`unixgram:///private/tmp/tb-sandbox-net/gvproxy.sock`. The sandbox flag takes
only the filesystem path: `/private/tmp/tb-sandbox-net/gvproxy.sock`.

On Linux, use `passt` instead. Install it with your distro package manager, then
start it in one terminal:

```sh
passt -f --socket /tmp/tb-sandbox-passt.sock
```

Then pass that socket path to the sandbox:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --credentials claude,codex \
  --passt-socket /tmp/tb-sandbox-passt.sock
```

You can also set `TB_SANDBOX_GVPROXY=auto`,
`TB_SANDBOX_GVPROXY=/path/to/gvproxy`, `TB_SANDBOX_GVPROXY_SOCKET`, or
`TB_SANDBOX_PASST_SOCKET`.
When a backend is present, the guest startup script brings up the first non-loopback
interface with DHCP before starting the agent shell.

Disable the managed proxy:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --no-network-proxy
```

Disable TLS interception while still routing through the managed proxy. Without
header-substitution rules, HTTPS is already tunneled; this flag also prevents
matched HTTPS rules from being applied:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --no-tls-mitm
```

Add or replace headers for HTTP and intercepted HTTPS requests seen by the proxy:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --proxy-header api.example.test:authorization='Bearer test-token'
```

Use precise host rules when possible. A wildcard rule such as
`*:authorization=...` forces TLS interception for every HTTPS host and may break
protocols the simple header-rewrite proxy does not implement, including
WebSockets and HTTP/2 upgrades.

For secrets, read the replacement value from the host environment at sandbox
start time. The sandbox metadata stores the environment variable name, not the
secret value:

```sh
ANTHROPIC_API_KEY=sk-ant-... \
  cargo run -- shell \
    --image target/agent-images/debian-bookworm \
    --workspace . \
    --proxy-header-env api.anthropic.com:authorization=ANTHROPIC_API_KEY
```

The CA private key is kept in host process memory and is regenerated each time
the proxy starts. It is not mounted into the guest or written to sandbox
metadata.

The shell runs on a PTY-backed serial console. On the host side, `os_mode` puts
your terminal in raw mode and bridges it through a PTY before attaching it to
libkrun. Inside the guest, `agent-sandbox-login-tty` creates a new session,
claims the serial device as the controlling terminal, drops to the `agent` user,
and starts the login shell. The guest startup script also applies an `80x24`
terminal-size default when the serial device reports `0x0`.

If interactive input looks lossy or bash reports `cannot set terminal process
group` or `no job control`, rebuild `../thousandbirds-libkrun/examples/os_mode`
and rebuild the agent image so both halves of the terminal path are present.

## Build A Base OS Bundle

Agent image builds need a reusable `libkrun-os-bundle` containing a compatible
kernel and initramfs. You can create one from this repo with:

```sh
BASE_BUNDLE=/path/to/existing/libkrun-os-bundle \
  scripts/build-base-bundle.sh
```

That command reuses `kernel` and `initramfs` from `BASE_BUNDLE`, builds a fresh
Debian systemd root disk from `images/base-debian-systemd.Containerfile`, and
writes:

```text
target/base-bundles/debian-systemd/libkrun-os-bundle/
```

If you already have kernel artifacts directly, use:

```sh
KERNEL=/path/to/kernel \
INITRAMFS=/path/to/initramfs \
  scripts/build-base-bundle.sh
```

Useful environment overrides:

```text
SOURCE_IMAGE=tb-agent-sandbox-base:debian-systemd
SOURCE_CONTAINERFILE=images/base-debian-systemd.Containerfile
SKIP_SOURCE_BUILD=1
RUNTIME=docker|podman|auto
PULL=1
PLATFORM=linux/arm64|linux/amd64
SIZE_MB=4096
OUTPUT_DIR=target/base-bundles/debian-systemd/libkrun-os-bundle
LIBKRUN_DIR=../thousandbirds-libkrun
```

## Build An Agent Image On Demand

Images are not all installed by default. Build only the variant you need.

Point `BASE_BUNDLE` at the base bundle:

```sh
BASE_BUNDLE=target/base-bundles/debian-systemd/libkrun-os-bundle \
  scripts/build-agent-image.sh debian-bookworm
```

This creates:

```text
target/agent-images/debian-bookworm/
```

The script overwrites the selected output directory on each run so image builds
are repeatable after guest script or service changes.

Optional variants:

```sh
BASE_BUNDLE=target/base-bundles/debian-systemd/libkrun-os-bundle scripts/build-agent-image.sh debian-bookworm-rust
BASE_BUNDLE=target/base-bundles/debian-systemd/libkrun-os-bundle scripts/build-agent-image.sh debian-bookworm-go
BASE_BUNDLE=target/base-bundles/debian-systemd/libkrun-os-bundle scripts/build-agent-image.sh ubuntu-noble
```

Useful environment overrides:

```text
RUNTIME=docker|podman|auto
PULL=1
PLATFORM=linux/arm64|linux/amd64
SIZE_MB=8192
OUTPUT_ROOT=target/agent-images
KERNEL=/path/to/kernel
INITRAMFS=/path/to/initramfs
LIBKRUN_DIR=../thousandbirds-libkrun
```

If you set `KERNEL` and `INITRAMFS` directly, `BASE_BUNDLE` is not required.

## Start A Sandbox Shell

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace .
```

Inside the guest shell:

```sh
claude
codex
```

The default workspace mount is:

```text
host:  .
guest: /workspace
tag:   workspace
```

## Credentials

Credential copying is opt-in. Credentials are copied into sandbox-local staging
state, mounted read-only into the guest, and imported into `/home/agent`.
Built-in agent profiles copy only small auth/config files, not cache, log,
history, plugin, or project state directories.

Claude credentials:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --credentials claude
```

The `claude` profile currently looks for:

```text
~/.claude.json
~/.claude/settings.json
~/.claude/.credentials.json
~/.config/claude/config.json
~/.config/claude/settings.json
```

It also imports these host environment variables when they are set:

```text
CLAUDE_CODE_OAUTH_TOKEN
ANTHROPIC_AUTH_TOKEN
ANTHROPIC_API_KEY
```

Claude Code stores subscription login credentials in the macOS Keychain, but the
sandbox is Linux and Claude Code expects Linux credentials at
`~/.claude/.credentials.json`. On macOS, `--credentials claude` reads the
`Claude Code-credentials` keychain item when no portable Claude auth env var is
already set and stages it as the Linux credentials file. macOS may prompt you to
allow keychain access for the launching terminal.

If keychain access is unavailable, run `claude setup-token` on the host and
export the printed token before launching the sandbox:

```sh
export CLAUDE_CODE_OAUTH_TOKEN=your-token

cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --credentials claude,codex \
  --gvproxy
```

Codex credentials:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --credentials codex
```

The `codex` profile currently looks for:

```text
~/.codex/auth.json
~/.codex/config.toml
~/.config/codex/auth.json
~/.config/codex/config.toml
```

It also imports `OPENAI_API_KEY` from the host environment when set.

Both:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --credentials claude,codex
```

Custom file and environment variable:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --credential-path ~/.config/example/token:.config/example/token \
  --credential-env EXAMPLE_TOKEN
```

Private SSH key material requires `--yes`:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --credentials git \
  --yes
```

## Manage Sandboxes

Create without starting:

```sh
cargo run -- create \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --name my-sandbox
```

Start an existing sandbox:

```sh
cargo run -- start my-sandbox
```

List, inspect, and delete:

```sh
cargo run -- list
cargo run -- inspect my-sandbox
cargo run -- delete my-sandbox
```

`list` reports each sandbox's run state and the host memory it reserves while
running:

```text
my-sandbox	running	2048MiB	/path/to/state/sandboxes/my-sandbox	/path/to/image
other   	stopped	-      	/path/to/state/sandboxes/other      	/path/to/image
```

## Snapshot, Restore, And Fork

A sandbox's root disk can be captured into an immutable artifact and used to
provision new sandboxes. This is **disk-only**: guest memory, vCPU state, and
live processes are not preserved — restored and forked sandboxes cold-boot
from the snapshotted disk. The memory/process side is tracked as Phase 3 of
[docs/closing-the-smolvm-gap-plan.md](docs/closing-the-smolvm-gap-plan.md)
and depends on libkrun HVF capture work.

**Snapshot** captures the disk + sandbox configuration:

```sh
cargo run -- snapshot my-sandbox
cargo run -- snapshot my-sandbox --name pre-build
cargo run -- snapshots                    # list
cargo run -- snapshot-inspect pre-build   # print manifest as JSON
cargo run -- snapshot-delete pre-build
```

The disk clone uses APFS copy-on-write on macOS so snapshot creation is ~5 ms
and the snapshot shares blocks with the source sandbox until either side
writes.

**Snapshots of running sandboxes are cooperatively quiesced.** Before the
clone, the wrapper asks the in-guest `agent-sandbox-quiesce` daemon to flush
the rootfs (`sync`), waits for the ack on the runtime-control mount, then
clones the disk, then sends a resume. This produces a strictly better
artifact than crash-consistent: in-flight writes are flushed before the
clone snapshots them. The manifest records `"quiesced": true` and the
`"quiesce_action"` performed (`sync` in this build; the protocol also
supports `freeze` and will use it when the guest-side daemon learns to drive
`fsfreeze -f /` safely without deadlocking its own polling loop).

Snapshots of stopped sandboxes skip quiesce automatically (nothing to
flush). On a running sandbox:

- `--no-quiesce` skips it (crash-consistent fallback, useful if the in-guest
  daemon is missing or hung).
- `--no-freeze` asks for sync only — the default action requested is
  `freeze`, which the current daemon downgrades to `sync` (the manifest
  records the downgrade).
- `--quiesce-timeout SECS` tunes the per-side timeout (default 10 s).

**Restore** creates a fresh sandbox whose root disk starts from a snapshot:

```sh
cargo run -- restore pre-build --name fresh
cargo run -- restore pre-build --name big --cpus 4 --memory-mib 4096 --memory-max-mib 8192
cargo run -- restore pre-build --workspace . --no-credentials
cargo run -- restore pre-build --ephemeral
```

Unset options inherit from the snapshot manifest (cpus, memory, virtio-fs DAX
size, workspace mounts, network backend, proxy config). `--no-credentials`
skips inheriting the snapshot's bundled credentials-staging; without it,
restore copies the snapshot's credentials into the new sandbox.

**Fork** is snapshot + immediate restore in one step, with no persisted
intermediate snapshot. The fork inherits the parent's configuration
wholesale:

```sh
cargo run -- fork my-sandbox                       # auto-name: my-sandbox-fork-<ts>
cargo run -- fork my-sandbox --name sibling
cargo run -- fork my-sandbox --name sibling --workspace . --no-credentials
```

Use fork to **branch** a configured sandbox into siblings — same image, same
mounts, same credentials, same hardware shape — without writing a checkpoint
to disk. Use snapshot + restore when you want a reusable starting point.

After a fork or restore, write to the new sandbox's `root.raw` does not leak
back to the parent or snapshot. Subsequent `start` / `shell` on the new
sandbox follows the same boot path as a freshly created one.

## Run Many Sandboxes (Density)

Two host-side controls keep many concurrent sandboxes within a machine's
resources. See
[docs/vm-density-and-resource-sharing-design.md](docs/vm-density-and-resource-sharing-design.md)
for the full design.

**Memory admission control.** Each running VM reserves its initial memory
(`--memory-mib`) against a host budget. Starting a VM that would exceed the
budget is refused (or queued), so you can pack sandboxes without swapping the
host. Reservations are released automatically when a VM exits (markers are
self-reaping if a VM crashes).

```sh
# Cap total sandbox memory at 24 GiB; refuse starts beyond it (the default).
TB_SANDBOX_MEM_BUDGET_MIB=24576 cargo run -- start my-sandbox

# Or wait for capacity instead of failing.
TB_SANDBOX_MEM_BUDGET_MIB=24576 TB_SANDBOX_ADMISSION=queue cargo run -- start my-sandbox
```

The budget defaults to 80% of physical RAM; set `TB_SANDBOX_MEM_BUDGET_MIB=0` to
disable admission control entirely.

**Shared read-only mounts.** Point many sandboxes at the same host directory
read-only — toolchains, package caches, model assets — so they share its
contents instead of each baking a private copy into its root disk. Unlike
`--mount`, a `--shared-mount` is read-only in the guest, so one sandbox cannot
mutate another's view.

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --shared-mount toolchains=/opt/host-toolchains \
  --shared-mount npm-cache=/Users/me/.npm:/home/agent/.npm
```

A `--shared-mount` defaults to guest path `/mnt/shared/<TAG>` when no
`:GUEST_PATH` is given.

**Shared host page cache (virtio-fs DAX).** Add `--virtiofs-dax-size SIZE`
(e.g. `--virtiofs-dax-size 1G`) to open a DAX window. Guests then map virtio-fs
files directly from the host page cache instead of copying them into guest RAM,
so multiple sandboxes mounting the same read-only directory share those physical
pages. Requires a guest kernel built with `CONFIG_FUSE_DAX` (see
[the libkrun guest image notes](../thousandbirds-libkrun/design_docs/os_mode_guest_image.md)).

**Automatic memory reclaim (virtio-balloon free-page reporting).** A guest kernel
built with `CONFIG_VIRTIO_BALLOON=y` and `CONFIG_PAGE_REPORTING=y` returns its
freed pages to the host automatically, so a long-lived VM's host memory tracks
its live working set instead of its peak. libkrun already attaches the balloon
and frees reported pages; this needs only the guest kernel options. It is
independent of the opt-in runtime [resize](#resize-memory-at-runtime) path.

**Warm pool (library API).** Embedders running a long-lived `SandboxManagerSdk`
can keep a pool of pre-booted sandboxes so new sessions skip cold boot:

```rust
let pool = WarmPool::new(sdk.clone(), template_config, 4);
pool.prewarm().await?;          // boot 4 sandboxes up front
let id = pool.acquire().await?; // hand one out; pool refills in the background
```

Pooled sandboxes share the template's mounts and are still subject to the memory
budget above.

Use an ephemeral sandbox that is deleted after the VM exits:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --ephemeral
```

## Browser-In-Sandbox

Agents can drive a real Chromium inside the sandbox over the Chrome DevTools
Protocol (CDP). The browser variant of the image bakes in Chromium plus a
display stack (`Xvfb` + `x11vnc` + `fluxbox`) so the same sandbox can serve
both headless CDP automation and headed VNC viewing.

Build the variant once:

```sh
BASE_BUNDLE=target/base-bundles/debian-systemd/libkrun-os-bundle \
  scripts/build-agent-image.sh debian-bookworm-browser
```

Boot a sandbox from it:

```sh
cargo run -- create \
  --image target/agent-images/debian-bookworm-browser \
  --workspace . \
  --name my-browser \
  --memory-mib 4096
```

### Headless: drive Chromium over CDP

Open the browser and forward the CDP port to the host. This command stays in
the foreground holding the forward open; Ctrl-C stops the browser and tears
the forward down.

```sh
cargo run -- start my-browser &       # boot the sandbox in another terminal first
cargo run -- browser start my-browser
# status=running
# mode=headless
# chromium_pid=341
# cdp_port=9222
# cdp_url=http://127.0.0.1:9222
# cdp_url=http://127.0.0.1:54321
# browser: forwarding cdp port; press Ctrl-C to stop browser and tear down forwards
```

The second `cdp_url=` line is the host-side forwarded URL — that is what the
user's agent code connects to. A minimal end-to-end demo, in a separate
terminal:

```sh
# Pretty-print the CDP target list — confirms the browser is reachable.
curl -s http://127.0.0.1:54321/json/version | python3 -m json.tool

# Drive it with the puppeteer Node client (or any CDP-compatible library):
cat > /tmp/cdp-demo.mjs <<'EOF'
import CDP from 'chrome-remote-interface';
const client = await CDP({ port: 54321 });
const { Page, Runtime } = client;
await Page.enable();
await Page.navigate({ url: 'https://example.com' });
await Page.loadEventFired();
const { result } = await Runtime.evaluate({ expression: 'document.title' });
console.log('title:', result.value);
await client.close();
EOF
npm install -g chrome-remote-interface 2>/dev/null
node /tmp/cdp-demo.mjs
# title: Example Domain
```

(Replace `54321` with whatever port the `browser start` output showed.)

When you're done, `Ctrl-C` the `browser start` command. The wrapper runs
`agent-sandbox-browser stop` in the guest and drops the host-side
listener.

### Headed: view the browser over VNC

For visual debugging or demos, start in headed mode. This launches Xvfb +
fluxbox + Chromium and an x11vnc server, then forwards both the CDP port
and the VNC port to the host:

```sh
cargo run -- browser start my-browser --headed
# status=running
# mode=headed
# chromium_pid=341
# cdp_port=9222
# vnc_port=5900
# cdp_url=http://127.0.0.1:54321
# vnc_url=vnc://127.0.0.1:54322
```

On macOS you can open the `vnc://` URL directly — Screen Sharing handles
the `vnc://` scheme by default:

```sh
open vnc://127.0.0.1:54322
```

On Linux, point any VNC client at `127.0.0.1:54322`. `cargo run -- browser
view my-browser` is a convenience wrapper that does headed-start + prints
the same URLs (it does **not** auto-open a viewer; pipe the URL to your
preferred client).

### Inspect or stop without holding the forward

`browser status` and `browser stop` are one-shot calls that don't stay
attached:

```sh
cargo run -- browser status my-browser
# status=running
# mode=headless
# chromium_pid=341
# cdp_port=9222
# cdp_url=http://127.0.0.1:9222

cargo run -- browser stop my-browser
# stopped
```

`status` reports the guest-side view; the `cdp_url` it prints uses the
*guest* port (9222), not a forwarded host port — useful for sanity-checking
that Chromium is alive even when no `browser start` is currently holding a
forward.

### Egress control

The browser inherits whatever network configuration the sandbox was started
with. With `--gvproxy` (or `--passt-socket`) the browser has full internet
access; the host-side proxy's per-host header substitution rules (see the
top of this README) apply to browser traffic too. Browser sessions are
isolated per-VM the same way every other workload is — there is no shared
profile or cookie jar across sandboxes.

## Resize Memory At Runtime

A running sandbox can have its guest memory grown or shrunk live, without a
reboot, using the guest's `virtio-balloon` device. This is **memory only** —
there is no runtime CPU (vCPU) resize.

Resizing is **opt-in** and disabled by default. To enable it, give the sandbox
headroom at create/`shell` time by setting a ceiling (`--memory-max-mib`) larger
than the initial memory (`--memory-mib`):

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --memory-mib 2048 \
  --memory-max-mib 8192
```

This boots the VM with `8192` MiB of RAM (the fixed ceiling) but starts the
balloon inflated so the guest only sees `2048` MiB usable. While the VM runs,
adjust the usable amount with `resize`:

```sh
cargo run -- resize my-sandbox --memory-mib 6144
```

The guest's usable memory changes within seconds. On success the command prints
the VMM's acknowledgement, e.g.:

```text
ok target-mib=6144 reclaimed-pages=524288
```

Optionally set a reclaim floor (default `512` MiB); the resize target is clamped
to `[--memory-min-mib, --memory-max-mib]`:

```sh
cargo run -- shell \
  --image target/agent-images/debian-bookworm \
  --workspace . \
  --memory-mib 2048 --memory-max-mib 8192 --memory-min-mib 1024
```

How it works:

- The wrapper passes the ceiling as the VM's boot RAM and starts the balloon
  inflated by `ceiling - initial` MiB. `resize` connects to a per-sandbox
  control socket at `<state>/sandboxes/<id>/control.sock`, which the in-VM
  control listener serves; it inflates or deflates the balloon to move the
  guest's usable memory to the requested size.
- **The ceiling is fixed at boot.** You can grow usable memory only up to
  `--memory-max-mib` and shrink it down to `--memory-min-mib`. Going above the
  boot ceiling would require memory hotplug, which is not implemented.
- Inflation is cooperative: the guest kernel must have a free-page supply to
  give back to the host, and reclaim is best-effort and asynchronous.

Requirements:

- The guest kernel must include `CONFIG_VIRTIO_BALLOON` (the libkrun guest
  kernel built by this toolchain does).
- The `os_mode` launcher in `../thousandbirds-libkrun` must support the
  `--control-socket` / `--balloon-initial-mib` flags. When the launcher predates
  that support, or when `--memory-max-mib` is left at its default (equal to
  `--memory-mib`), the sandbox boots with fixed memory and `resize` is
  unavailable.

Note the startup trade-off from
[Startup Performance Notes](#startup-performance-notes): booting at a high
ceiling costs extra startup time because the VMM zeroes more guest RAM up front.
Pick a ceiling you actually expect to need.

## Runtime Overrides

Use a specific `os_mode` launcher:

```sh
TB_SANDBOX_OS_MODE=/path/to/os_mode cargo run -- shell --image target/agent-images/debian-bookworm
```

Use disposable state directories for tests:

```sh
TB_SANDBOX_STATE_DIR=/tmp/tb-sandbox-state \
TB_SANDBOX_CACHE_DIR=/tmp/tb-sandbox-cache \
  cargo run -- list
```

## Profile Startup And Resource Use

`scripts/profile-sandbox.sh` is the runnable entrypoint for measuring sandbox
startup time and VM resource utilization. It boots a sandbox, waits for a
guest-side ready marker, samples the VM process (the `os_mode` launcher that
runs libkrun in-process), then tears everything down. It never blocks on
interactive input, so it is safe to run in scripts and CI.

Single run:

```sh
scripts/profile-sandbox.sh --image target/agent-images/debian-bookworm
```

Example output:

```text
[iteration 1]
  host create:               5 ms
  host prelaunch:            5 ms  (config + backend/proxy setup)
  pre-kernel VMM init:     700 ms  (os_mode + libkrun VM create, before kernel)
  guest kernel->ready:     560 ms  (kernel + systemd + shell service + login)
    boot->shell svc:       540 ms  (kernel + systemd to shell service)
    shell-service tail:     20 ms  (mounts + creds + login handoff)
      credential import:    10 ms
  total startup:          1270 ms
  VM peak RSS:           433 MiB
  VM peak CPU:          206 %
```

The host-side phases (`host create`, `host prelaunch`) come from
`TB_SANDBOX_PROFILE=1`, which makes `tb-sandbox` emit
`tb-sandbox-profile <phase>=<unix_millis>` markers; the script sets this
automatically. The pre-kernel/in-guest split is derived from the guest's
`/proc/uptime` at the ready point (injected via `--initial-command`), so it
works on any image. The finer `boot->shell svc` / `shell-service tail` /
`credential import` rows require an image built from the instrumented
`guest/agent-sandbox-shell` in this repo. Set `SYSTEMD_ANALYZE=1` to also dump
`systemd-analyze blame` from the guest on the first iteration.

Run several iterations to get min/median/avg/max, and write per-sample
resource rows to a CSV:

```sh
ITERATIONS=5 RESOURCE_CSV=target/profile-samples.csv \
  scripts/profile-sandbox.sh --image target/agent-images/debian-bookworm
```

Anything after `--` is forwarded verbatim to `tb-sandbox shell`, so you can
profile a configuration with networking and credentials:

```sh
scripts/profile-sandbox.sh \
  --image target/agent-images/debian-bookworm \
  -- --gvproxy --credentials claude
```

Useful environment overrides:

```text
IMAGE=PATH                 Agent image directory (or pass --image)
WORKSPACE=PATH             Host workspace mounted at /workspace (default: repo root)
ITERATIONS=N               Boot/teardown cycles to run (default: 1)
SAMPLE_INTERVAL=SECONDS    Resource sampling interval (default: 0.25)
READY_TIMEOUT=SECONDS      Max wait for the ready marker per iteration (default: 120)
SETTLE_SECONDS=SECONDS     Keep sampling after ready before teardown (default: 3)
PROFILE=debug|release      Cargo profile to build/run (default: release)
TB_SANDBOX_BIN=PATH        Use a prebuilt binary instead of building
RESOURCE_CSV=PATH          Append per-sample rows: iteration,elapsed_ms,rss_kib,cpu_pct
KEEP_LOGS=1                Keep per-iteration logs and print the temp dir path
SYSTEMD_ANALYZE=1          Dump 'systemd-analyze blame' from the guest (iteration 1)
```

## Startup Performance Notes

Measured cold-boot breakdown of `total startup` (Apple Silicon, debian-bookworm
image), in rough order of cost:

```text
pre-kernel VMM init   ~470-770 ms   os_mode + libkrun VM create, load+decompress
                                    kernel/initramfs, virtio setup (before kernel)
guest kernel->ready   ~530-570 ms   of which:
  kernel + initramfs  ~345 ms       module load (virtio_blk), root mount, pivot
  systemd->shell svc  ~195 ms       dev-vda.device (~120 ms) dominates
  shell-service tail   ~20-30 ms    guest/agent-sandbox-shell work
host create/prelaunch  ~5 ms        APFS copy-on-write clone of the root disk
```

Tuned defaults and what moves the numbers (from A/B sweeps):

- **`--memory-mib 2048` is the default and the startup sweet spot.** Raising
  memory *hurts* startup (~+100 ms at 4096 MiB, from zeroing more guest RAM);
  lowering it to 512 MiB does not help. Workspace/credential mount count has no
  measurable effect on startup.
- **`--cpus` defaults to 1** for VM density (vCPUs are host threads). A second
  CPU shaves ~60 ms off guest boot (more parallel systemd) and 4 CPUs does not
  beat 2, so pass `--cpus 2` when you want the fastest boot or run parallel
  builds. See [docs/vm-density-and-resource-sharing-design.md](docs/vm-density-and-resource-sharing-design.md).
- **The host create step is effectively free** because the root disk is cloned
  with an APFS copy-on-write `cp -c`. Cold-cache first boots after an image
  rebuild show higher `pre-kernel VMM init`; it settles once the disk is warm.

What this repo already does for startup:

- `systemd-udev-settle.service` is masked in the image build — it is deprecated
  and sat on the critical path to `sysinit.target`.
- `guest/agent-sandbox-shell` skips credential import (and its `chown`) entirely
  when nothing is staged, and `agent-sandbox-import-credentials` chowns only the
  imported entries rather than the whole agent home.

Biggest remaining levers (both live in `../thousandbirds-libkrun`, not here):

- **Drop the initramfs.** The guest kernel currently loads `virtio_blk` as a
  module from the 25 MB initramfs, so booting without it panics
  (`VFS: Cannot open root device "/dev/vda"`). Rebuilding the libkrun guest
  kernel with `CONFIG_VIRTIO_BLK=y` and `CONFIG_EXT4_FS=y` would let os_mode
  mount `/dev/vda` directly and remove both the initramfs load (pre-kernel) and
  the ~345 ms initramfs phase (in-guest) — the single biggest win.
- **Snapshot/restore.** libkrun has no snapshot API yet
  (`design_docs/snapshot_restore.md` defers it). A restored VM would collapse
  most of pre-kernel + guest boot. A host-side warm pool of pre-booted VMs is
  the only near-term alternative, at ~430 MiB RAM per idle VM.

## Verification

```sh
cargo fmt -- --check
cargo test
sh -n scripts/build-base-bundle.sh
sh -n scripts/build-agent-image.sh
sh -n scripts/profile-sandbox.sh
sh -n guest/agent-sandbox-shell
sh -n guest/agent-sandbox-import-credentials
sh -n guest/agent-sandbox-quiesce
sh -n guest/agent-sandbox-browser
cc -Wall -Wextra -c guest/agent-sandbox-login-tty.c -o /tmp/agent-sandbox-login-tty.o
cargo check
```
