# Thousand Birds Agent Sandbox

Fast, isolated Linux microVMs for running coding agents (`claude`, `codex`, or
any shell workload). Each sandbox is a real [libkrun](https://github.com/containers/libkrun)
VM — stronger isolation than a container — that cold-boots in well under a second
and reserves only a few hundred MiB of host memory.

You can use it two ways:

- **As a CLI** — `tb-sandbox` boots, controls, and tears down sandboxes from your
  terminal (`tb-sandbox shell`, `create`, `exec`, `snapshot`, …).
- **As an SDK you integrate** — run `tb-sandbox server` for a localhost HTTP/JSON
  API any language can drive, or embed the Rust crate directly in your app.

Both speak the same [API contract](docs/api-contract.md): sandbox lifecycle,
`exec` with streamed stdout/stderr, filesystem and git operations, port
forwarding, and snapshot/restore.

---

## Contents

- [Install](#install)
- [Get a guest image](#get-a-guest-image)
- [Quickstart](#quickstart)
- [CLI reference](#cli-reference)
- [Integrate it (SDK)](#integrate-it-sdk)
  - [HTTP API server](#http-api-server)
  - [Rust crate](#rust-crate)
  - [Drive a remote server from the CLI](#drive-a-remote-server-from-the-cli)
- [Configuration](#configuration)
- [Feature guides](#feature-guides) — networking, credentials, snapshots, scaling, browser, memory resize, preview
- [Building guest images](#building-guest-images)
- [Performance notes](#performance-notes)
- [Development](#development)

---

## Install

**From source** (macOS or Linux, with a sibling `../thousandbirds-libkrun`
checkout):

```sh
make setup
```

This builds two binaries into `target/{debug,release}`:

- `tb-sandbox` — the CLI / API server.
- `tb-sandbox-runner` — the per-VM launcher. `tb-sandbox` finds it next to its
  own binary, so no `DYLD_*`/`TB_SANDBOX_*` env vars are needed.

**Install onto your `PATH`** (local install, no packaging step):

```sh
cargo install --path .
# -> installs tb-sandbox AND tb-sandbox-runner into ~/.cargo/bin
tb-sandbox --help
```

`cargo install` builds libkrun statically into the runner, so the installed
binaries are self-contained — no `TB_SANDBOX_LIBKRUN_DIR` or `DYLD_*` needed.
On macOS the runner needs the `com.apple.security.hypervisor` entitlement to
talk to Hypervisor.framework; `cargo build`/`cargo install` produce an unsigned
binary, so `tb-sandbox` **ad-hoc self-signs the runner on first boot** (you'll
see a one-line "re-signed … hypervisor entitlement" notice). This only ever
touches Cargo-built/installed binaries (under `target/` or `~/.cargo/bin`),
never a notarized release. Requires the Xcode command-line tools (`codesign`),
which are already present on any machine that can build Rust.

> Note: this is a path install against the sibling `../thousandbirds-libkrun`
> checkout (the libkrun dependency is a local path, not yet published). Keep that
> checkout in place when running `cargo install`.

**Packaged macOS release** (signed + notarized):

```sh
APPLE_ID="you@example.com" APPLE_APP_PASSWORD="xxxx-xxxx-xxxx-xxxx" \
  scripts/release-macos.sh
# -> dist/tb-sandbox-<version>-macos-arm64.{dmg,tar.gz}
```

Unpack it and put `bin/` on your `PATH`:

```sh
tar -xzf tb-sandbox-<version>-macos-arm64.tar.gz
export PATH="$PWD/tb-sandbox-<version>-macos-arm64/bin:$PATH"
tb-sandbox --help
```

The package does **not** include a guest image — that is multi-GB data shipped
separately (see below).

## Get a guest image

A sandbox boots from an **agent image**: a directory with a guest kernel and a
root disk. The package doesn't ship one; build it once (needs Docker or Podman):

```sh
# Build everything: base bundle + the default debian-bookworm image.
BASE_BUNDLE=/path/to/seed/libkrun-os-bundle make setup-all
# -> target/agent-images/debian-bookworm   (this directory is your --image)
```

See [Building guest images](#building-guest-images) for what a base bundle is and
how to build other variants. Images are direct-boot (no initramfs) by default
when a built-in-driver kernel is available, which is what gets you sub-second boots.

## Quickstart

Drop into a shell inside a fresh sandbox:

```sh
# On macOS, gvproxy is auto-selected when installed (gives the guest a network
# route + low-latency port forwards); pass --gvproxy to force it, or set
# TB_SANDBOX_NO_NETWORK=1 to opt out. See Networking.
tb-sandbox shell --image target/agent-images/debian-bookworm --workspace .
```

Your current directory is mounted at `/workspace`. Inside the guest:

```sh
claude        # or: codex, or any command
```

Or run the non-interactive lifecycle:

```sh
tb-sandbox create --image target/agent-images/debian-bookworm --workspace . --name dev
tb-sandbox exec dev -- bash -lc 'uname -a'
tb-sandbox list
tb-sandbox delete dev
```

`make shell` wraps the first command with sensible defaults once `make setup-all`
has built an image.

---

## CLI reference

`tb-sandbox <command>`. Run `tb-sandbox --help` for the full option list.

### Lifecycle

| Command | Description |
|---|---|
| `shell [OPTIONS] [-- CMD...]` | Create + start + attach an interactive guest shell |
| `create [OPTIONS]` | Create a sandbox without starting it |
| `start SANDBOX_ID` | Start (attach to) an existing sandbox |
| `stop SANDBOX_ID` | Stop a running sandbox |
| `exec SANDBOX_ID [--cwd DIR] [--env K=V]... -- CMD...` | Run a command, streaming stdout/stderr |
| `list` | List sandboxes with state and reserved memory |
| `inspect SANDBOX_ID` | Print a sandbox's config + state |
| `resize SANDBOX_ID --memory-mib N` | Grow/shrink guest memory at runtime (see [Memory resize](#resize-memory-at-runtime)) |
| `delete SANDBOX_ID` | Delete a sandbox and its state |

### Snapshots

| Command | Description |
|---|---|
| `snapshot SANDBOX_ID [--name N] [--capture-memory]` | Capture disk (and optionally memory) into a reusable artifact |
| `snapshots [prune [--delete]]` | List snapshots / report (or remove) unusable ones |
| `snapshot-inspect SNAPSHOT_ID` | Print a snapshot manifest |
| `snapshot-delete SNAPSHOT_ID` | Delete a snapshot |
| `restore SNAPSHOT_ID [--name N] [--image-path DIR] [OPTIONS]` | Create a new sandbox from a snapshot (`--image-path` points at a local agent image when restoring on a different host) |
| `fork SANDBOX_ID [--name N] [OPTIONS]` | Snapshot + restore in one step (branch a sandbox) |

### Server & integration

| Command | Description |
|---|---|
| `server [--bind ADDR] [--no-auth] [--token T]` | Run the localhost HTTP API ([SDK](#http-api-server)) |
| `preview [--bind ADDR] [--no-auth] [ID PORT]` | Expose a guest port over HTTP ([Preview](#preview-a-guest-port)) |
| `browser start\|view\|stream\|status\|stop SANDBOX_ID` | Drive Chromium in the sandbox ([Browser](#browser-in-sandbox)) |

### Common options

```text
--image PATH_OR_REF              Agent image directory or reference
--name NAME                      Sandbox name (auto-generated if omitted)
--workspace PATH                 Host dir mounted read-write at /workspace
--cpus N                         vCPUs (default 1)
--memory-mib N                   Initial guest memory (default 2048)
--memory-max-mib N               Resize ceiling; enables `resize` when > --memory-mib
--mount TAG=HOST[:GUEST]         Extra read-write mount
--shared-mount TAG=HOST[:GUEST]  Read-only mount shared across sandboxes
--credentials claude,codex,git   Copy agent credentials into the guest (opt-in)
--gvproxy                        Managed guest networking on macOS (needs Podman)
--passt-socket PATH              Guest networking on Linux (passt)
--egress-allow PATTERN[,...]     Restrict outbound HTTP(S) to listed hosts
--ephemeral                      Delete the sandbox when the VM exits
--initial-command CMD            Run CMD at guest startup
--yes                            Skip confirmation prompts (e.g. SSH-key copy)
```

State lives under `TB_SANDBOX_STATE_DIR` (default platform app-data dir); set it
(and `TB_SANDBOX_CACHE_DIR`) to keep runs self-contained, e.g. for tests.

---

## Integrate it (SDK)

There are three integration paths. Pick by how your app is written and where the
sandboxes run.

### HTTP API server

The language-agnostic path. Start a localhost JSON API over the same runtime:

```sh
tb-sandbox server            # binds 127.0.0.1:8080, prints a bearer token
tb-sandbox server --bind 127.0.0.1:9000 --token "$MY_TOKEN"
```

It's JSON over HTTP/1.1, authenticated with `Authorization: Bearer <token>`
(`GET /health` is open). The token is printed on start and persisted in the state
dir. Full schema in [docs/api-contract.md](docs/api-contract.md); the routes:

```text
GET    /health                              -> {status, version}
GET    /v1/sandboxes                        -> {sandboxes:[{id,state,state_dir}]}
POST   /v1/sandboxes                         (body {image,name?,cpus?,memory_mib?,...}) -> Sandbox
GET    /v1/sandboxes/{id}                   -> Sandbox (+ mounts)
POST   /v1/sandboxes/{id}/start | /stop     -> Sandbox
DELETE /v1/sandboxes/{id}                   -> {}
POST   /v1/sandboxes/{id}/exec               (body {argv,cwd?,env?}) -> NDJSON stream:
         {type:"started",process_id} {type:"stdout",data} {type:"stderr",data} {type:"exit",code,success}
POST   /v1/sandboxes/{id}/cancel             (body {process_id}) -> {}
GET    /v1/sandboxes/{id}/fs?path=          -> {path, content}
PUT    /v1/sandboxes/{id}/fs?path=&mode=     (raw body) -> {path, bytes}
GET    /v1/sandboxes/{id}/git/status?path=  -> {branch, clean, entries[]}
GET    /v1/sandboxes/{id}/git/diff?path=    -> {diff}
POST   /v1/sandboxes/{id}/git/clone          (body {url,path,ref?}) -> {path}
POST   /v1/sandboxes/{id}/ports              (body {guest_port}) -> {host, port, guest_port}
GET    /v1/events                           -> chunked lifecycle/process event stream
```

Errors come back as `{"error":{"category","message"}}` with the HTTP status
derived from the category (`invalid_argument`→400, `not_found`→404,
`resource_exhausted`→429, `failed_precondition`→409, `unavailable`→503,
`internal`/`io`→500).

Example — create a sandbox and run a command:

```sh
TOKEN=...   # from `tb-sandbox server` output
BASE=http://127.0.0.1:8080

ID=$(curl -s -X POST $BASE/v1/sandboxes \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"image":"/path/to/agent-images/debian-bookworm","name":"job-1"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

curl -s -X POST $BASE/v1/sandboxes/$ID/start -H "Authorization: Bearer $TOKEN"

curl -sN -X POST $BASE/v1/sandboxes/$ID/exec \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"argv":["bash","-lc","echo hello from $(uname -n)"]}'
# {"type":"started","process_id":"..."}
# {"type":"stdout","data":"hello from ..."}
# {"type":"exit","code":0,"success":true}
```

> Bind to loopback only unless you understand the exposure: the API can `exec`
> arbitrary commands in any sandbox, protected only by the bearer token.

### Rust crate

Embed the runtime in-process. Add a path/git dependency:

```toml
[dependencies]
thousandbirds-agent-sandbox = { path = "../thousandbirds-agent-sandbox" }
tokio = { version = "1", features = ["full"] }
```

The core surface is the async `SandboxRuntimeSdk` trait, implemented by
`SandboxManagerSdk`:

```rust
use thousandbirds_agent_sandbox::{
    AgentImageRef, GuestCommand, SandboxConfig, SandboxManager, SandboxManagerSdk,
    SandboxRuntimeSdk,
};

#[tokio::main]
async fn main() -> thousandbirds_agent_sandbox::Result<()> {
    let sdk = SandboxManagerSdk::new(SandboxManager::new());

    let config = SandboxConfig::new(AgentImageRef::new("target/agent-images/debian-bookworm"));
    let sandbox = sdk.create_session(config).await?;
    sdk.start_session(&sandbox.id).await?;

    // exec returns a GuestProcess with bounded stdout/stderr channels + an exit
    // oneshot. Drain both channels until they close, then await the exit.
    let mut proc = sdk.exec(&sandbox.id, GuestCommand::new(["bash", "-lc", "uname -a"])).await?;
    while let Some(line) = proc.stdout.recv().await {
        println!("{line}");
    }
    let status = proc.exit.await.unwrap()?;
    println!("exited: {}", status.code.unwrap_or(-1));

    sdk.delete_session(&sandbox.id).await?;
    Ok(())
}
```

Useful re-exports: `SandboxConfig`, `WorkspaceMount`, `NetworkBackend`,
`CredentialProfile`, `SnapshotOptions`/`RestoreOptions`/`ForkOptions`, and
`WarmPool` (a pool of pre-booted sandboxes — see [Scaling](#run-many-sandboxes-scaling)).
The conformance test `sdk::tests::api_contract_conformance_local_process_runtime`
is a working reference for the full surface.

### Drive a remote server from the CLI

The CLI itself becomes a thin client when pointed at a server. Set
`TB_SANDBOX_API_URL` (and a token), and `create`/`list`/`inspect`/`start`/`stop`/
`exec`/`delete` dispatch to that server instead of running in-process:

```sh
export TB_SANDBOX_API_URL=http://127.0.0.1:8080
export TB_SANDBOX_API_TOKEN=...      # or rely on the persisted token in the state dir
tb-sandbox list
tb-sandbox exec job-1 -- bash -lc 'pwd'
```

---

## Configuration

### Resources

- `--cpus N` defaults to **1** for density (vCPUs are host threads). `--cpus 2`
  is the fastest-boot / parallel-build sweet spot.
- `--memory-mib N` defaults to **2048**. Memory is demand-paged, so this is a
  ceiling, not a reservation — an idle VM only resides its working set (see
  [Performance notes](#performance-notes)).
- `--memory-max-mib N` larger than `--memory-mib` enables runtime
  [resize](#resize-memory-at-runtime).

### Mounts

- `--workspace PATH` → `/workspace` (read-write).
- `--mount TAG=HOST[:GUEST]` → extra read-write mount.
- `--shared-mount TAG=HOST[:GUEST]` → read-only mount, safe to share across many
  sandboxes (defaults to `/mnt/shared/<TAG>`).

### Environment variables

```text
TB_SANDBOX_STATE_DIR     Where sandbox state lives (default: platform app-data dir)
TB_SANDBOX_CACHE_DIR     Cache directory
TB_SANDBOX_IMAGE         Default --image when not passed
TB_SANDBOX_RUNNER        Override the tb-sandbox-runner binary
TB_SANDBOX_API_URL       Dispatch CLI commands to this API server
TB_SANDBOX_API_TOKEN     Bearer token for API mode
TB_SANDBOX_MEM_BUDGET_MIB   Host memory budget for admission control (0 disables; default 80% RAM)
TB_SANDBOX_ADMISSION     reject | queue  when the budget is full (default reject)
TB_SANDBOX_GVPROXY       auto | /path/to/gvproxy
TB_SANDBOX_GVPROXY_SOCKET / TB_SANDBOX_PASST_SOCKET   Reuse an existing network socket
```

---

## Feature guides

### Networking and the egress proxy

A sandbox has no network route by default. To give the guest internet access:

- **macOS:** install Podman (`brew install podman`, which ships `gvproxy`) and pass
  `--gvproxy`. `tb-sandbox` starts `gvproxy`, wires a per-sandbox socket, and tears
  it down on exit. If `gvproxy` isn't auto-found, pass `--gvproxy-bin "$(brew --prefix podman)/libexec/podman/gvproxy"`.
- **Linux:** run `passt` and pass `--passt-socket /path/to/passt.sock`.

When a network backend is present, `tb-sandbox` also starts a **host-side HTTP
proxy** and injects `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` into the guest:

- Plain HTTP is proxied directly; HTTPS `CONNECT` is tunneled **byte-for-byte**
  (so WebSockets like Codex's `wss://` keep working) **unless** a header rule
  matches the target host.
- `--proxy-header HOST:HEADER=VALUE` (or `--proxy-header-env HOST:HEADER=ENV`)
  rewrites headers for specific hosts; only matched HTTPS hosts are
  TLS-intercepted, using an **ephemeral CA** whose private key never leaves host
  memory (only the CA cert is mounted into the guest).
- `--egress-allow api.anthropic.com,*.openai.com` restricts outbound HTTP(S) to
  an allowlist (exact host or `*.suffix`). Default: unrestricted.
- `--no-network-proxy` disables the managed proxy; `--no-tls-mitm` keeps the
  proxy but never intercepts TLS.

```sh
ANTHROPIC_API_KEY=sk-ant-... tb-sandbox shell \
  --image target/agent-images/debian-bookworm --workspace . --gvproxy \
  --proxy-header-env api.anthropic.com:authorization=ANTHROPIC_API_KEY \
  --egress-allow api.anthropic.com
```

### Credentials

Credential copying is **opt-in** and minimal (only auth/config files, never cache
or history). Files are staged into sandbox-local state, mounted read-only, and
imported into `/home/agent`.

```sh
tb-sandbox shell --image ... --workspace . --credentials claude,codex
```

- `claude` profile: `~/.claude.json`, `~/.claude/.credentials.json`,
  `~/.config/claude/*`, and the env vars `CLAUDE_CODE_OAUTH_TOKEN`,
  `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`. On macOS it also reads the
  `Claude Code-credentials` Keychain item (the guest is Linux and expects
  `~/.claude/.credentials.json`); macOS may prompt for Keychain access. If that's
  unavailable, run `claude setup-token` and export `CLAUDE_CODE_OAUTH_TOKEN`.
- `codex` profile: `~/.codex/{auth.json,config.toml}`, `~/.config/codex/*`, and
  `OPENAI_API_KEY`.
- `git` profile copies SSH key material and therefore requires `--yes`.
- Custom: `--credential-path HOST:GUEST_RELATIVE` and `--credential-env NAME`.

### Snapshot, restore, and fork

Snapshots are **disk-only by default** (restored sandboxes cold-boot from the
snapshotted disk). The disk clone uses APFS copy-on-write on macOS (~5 ms).
Snapshots of a *running* sandbox are cooperatively quiesced (the in-guest daemon
flushes/freezes the rootfs before the clone). With a runner that supports it,
`--capture-memory` produces a **Full** snapshot that also captures guest memory +
vCPU state.

```sh
tb-sandbox snapshot dev --name pre-build              # disk-only
tb-sandbox snapshot dev --name warm --capture-memory  # full (memory + disk)
tb-sandbox restore pre-build --name fresh --workspace .
tb-sandbox fork dev --name sibling                    # branch in one step
```

`restore`/`fork` inherit the source's cpus, memory, mounts, network, and proxy
config unless overridden. Writes to a restored/forked disk never leak back to the
parent or snapshot.

A manifest records the agent image as an **absolute path** from the host that
took the snapshot. When restoring on a *different* host where the image lives
elsewhere, pass `--image-path DIR` to point at the local image (it must be the
same guest architecture as the snapshotted rootfs).

#### Resume a snapshot on a remote GCP node

A **disk-only** snapshot is just `root.raw` + `manifest.json` — no CPU, GIC, or
hypervisor state — so it carries nothing macOS/HVF-specific and `restore`
cold-boots it anywhere. This is the **reliable** way to move a sandbox from a Mac
to the cloud. (A `--capture-memory` **Full** snapshot embeds live aarch64
vCPU/GIC/device state captured through Hypervisor.framework; it is host-locked
and **cannot** be resumed on a GCP KVM node.)

Two requirements for a clean cold boot remotely:

- **Matching guest arch.** A snapshot built on Apple Silicon is aarch64, so it
  can only boot on a GCP **Arm** node — Axion (C4A) or Tau T2A (Ampere Altra),
  not the default x86_64 machine types — running an `aarch64-linux` build of the
  runner.
- **A local agent image** of that same arch on the GCP node, passed via
  `--image-path`.

[`scripts/release-to-gcp.sh`](scripts/release-to-gcp.sh) wraps the whole flow and
refuses to ship a Full snapshot:

```sh
# On macOS — take a quiesced disk-only snapshot and upload it to GCS:
scripts/release-to-gcp.sh push my-sbx gs://tb-snaps/agents --name nightly

# On a GCP Arm node — download and cold-boot from the local image:
scripts/release-to-gcp.sh resume gs://tb-snaps/agents/nightly \
    --image-path /opt/tb-sandbox/images/debian --name resumed-1
```

Equivalently by hand: `gcloud storage cp -r <snapshot-dir> gs://…`, copy it into
`<state>/snapshots/<id>` on the node, then
`tb-sandbox restore <id> --image-path /opt/tb-sandbox/images/debian`. Live
process state is not preserved (cold boot); the quiesced/frozen rootfs comes up
clean.

### Run many sandboxes (scaling)

- **Memory admission control.** Each running VM reserves its `--memory-mib`
  against a host budget (`TB_SANDBOX_MEM_BUDGET_MIB`, default 80% of RAM). Starts
  beyond the budget are refused, or queued with `TB_SANDBOX_ADMISSION=queue`.
  Reservations free automatically on exit (self-reaping if a VM crashes).
- **Shared read-only mounts** (`--shared-mount`) + **virtio-fs DAX**
  (`--virtiofs-dax-size 1G`) let many sandboxes share host page-cache pages for
  common toolchains/caches instead of each copying them in.
- **Free-page reporting** returns a guest's freed pages to the host (Linux/KVM;
  limited on macOS/HVF — see
  [docs/vm-density-and-resource-sharing-design.md](docs/vm-density-and-resource-sharing-design.md)).
- **Warm pool** (Rust SDK): keep pre-booted sandboxes ready so new sessions skip
  cold boot.

```rust
let pool = WarmPool::new(sdk.clone(), template_config, 4);
pool.prewarm().await?;            // boot 4 up front
let id = pool.acquire().await?;   // hand one out; pool refills in the background
```

### Browser-in-sandbox

Agents can drive a real Chromium inside the sandbox over the Chrome DevTools
Protocol (CDP), view it over VNC, or watch it over an efficient **DOM-diff
stream**. Build the browser image once (`make setup-browser-image`), then:

```sh
make browser-headless && make browser-boot && make browser-cdp   # CDP automation
make browser-vnc && make browser-boot && make browser-view       # headed, opens VNC (password: sandbox)
tb-sandbox browser stream SANDBOX_ID                             # DOM-diff stream, opens a viewer URL
```

`browser stream` injects [rrweb](https://github.com/rrweb-io/rrweb) over CDP and
ships only DOM mutations (not pixels) to a host-served viewer page, with a CDP
`Input.*` back-channel for clicks/keys/scroll. It is interactive, runs headless
(no Xvfb/x11vnc), and uses far less bandwidth than VNC for normal pages. Canvas/
WebGL/`<video>` and cross-origin iframes are the lossy cases — see
[`docs/browser-stream-design.md`](docs/browser-stream-design.md).

`browser status`/`stop` are one-shot calls that don't hold the forward open. The
browser inherits the sandbox's network config and egress rules. See
`tb-sandbox browser --help` for ports and profile options.

### Resize memory at runtime

Memory (not CPU) can be grown/shrunk live via the guest balloon, if you booted
with headroom (`--memory-max-mib` > `--memory-mib`):

```sh
tb-sandbox shell --image ... --memory-mib 2048 --memory-max-mib 8192   # boots with an 8 GiB ceiling
tb-sandbox resize my-sandbox --memory-mib 6144                          # adjust live
```

The ceiling is fixed at boot; you can move usable memory within
`[--memory-min-mib, --memory-max-mib]`. Booting at a high ceiling is cheap
(memory is demand-paged), but pick a ceiling you expect to need.

### Preview a guest port

`tb-sandbox preview` exposes a guest HTTP port to the host through a small proxy,
routed by a `{port}-{sandbox_id}.localhost` host header so one listener can serve
many sandboxes. See `tb-sandbox preview --help`.

---

## Building guest images

An **agent image** is built in two stages, both needing Docker or Podman:

1. A **base bundle** (`scripts/build-base-bundle.sh`) — a reusable
   `libkrun-os-bundle` (guest kernel + Debian systemd root disk).
2. An **agent image** (`scripts/build-agent-image.sh`) — installs the toolchain
   (node, git, ripgrep, `claude`, `codex`, …) on top of a base bundle.

```sh
BASE_BUNDLE=/path/to/seed/libkrun-os-bundle scripts/build-base-bundle.sh
BASE_BUNDLE=target/base-bundles/debian-systemd/libkrun-os-bundle \
  scripts/build-agent-image.sh debian-bookworm
# -> target/agent-images/debian-bookworm   (pass this as --image)
```

`make setup-images` / `make setup-all` wrap this. Variants:
`debian-bookworm-rust`, `debian-bookworm-go`, `ubuntu-noble`,
`debian-bookworm-browser`.

**Direct boot is the default.** When a built-in-driver kernel is available (a
`libkrunfw` checkout, auto-located, or `BUILTIN_KERNEL=`), the build produces a
no-initramfs image that cold-boots ~29% faster with ~47% lower idle footprint.
Force it either way with `NO_INITRAMFS=1` / `NO_INITRAMFS=0`. To convert an
existing image without a rebuild:

```sh
SRC_IMAGE=target/agent-images/debian-bookworm \
KERNEL=/path/to/builtin-driver/Image.gz \
  scripts/make-direct-boot-image.sh           # or: make direct-boot-image
```

See each script's `--help` and the design docs for the full option set.

## Performance notes

Cold-boot, Apple Silicon, `debian-bookworm`, median of 3 (measured with
`scripts/profile-sandbox.sh`):

```text
                       with initramfs   direct-boot
  total startup          1007 ms          718 ms      (-29%)
  guest kernel->ready      730 ms          500 ms
  pre-kernel VMM init      280 ms          231 ms
  idle physical footprint  412 MiB         217 MiB     (-47%, vmmap)
```

- **Direct boot is the biggest lever** — a guest kernel with `virtio_blk`/`ext4`
  built in mounts `/dev/vda` directly and drops the ~25 MB initramfs and its boot
  phase. It's the default for new images (see above).
- **`--memory-mib` is a ceiling, not a reservation.** Guest RAM is anonymous and
  demand-paged, so idle footprint tracks the working set, not the cap — lowering
  it doesn't shrink an idle VM (and risks OOM). True post-spike reclaim needs the
  balloon (works on Linux/KVM; limited on macOS/HVF).
- `--cpus 2` shaves ~60 ms off guest boot; `--cpus 1` (default) favors density.

`scripts/profile-sandbox.sh --image <dir>` prints the full per-phase breakdown;
`ITERATIONS=N` and `RESOURCE_CSV=path` add aggregate stats and per-sample
resource rows.

## Development

```sh
cargo fmt --check
cargo test --all --locked
make check          # fmt + cargo check + script syntax checks
```

Design docs live in [`docs/`](docs/): the [API contract](docs/api-contract.md),
the [wrapper design](docs/agent-sandbox-wrapper-design.md), and the
[VM density / resource-sharing design](docs/vm-density-and-resource-sharing-design.md).
