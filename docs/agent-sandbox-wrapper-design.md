# Agent Sandbox Wrapper Design

## Purpose

`thousandbirds-agent-sandbox` is a small wrapper around
`../thousandbirds-libkrun` for creating disposable Linux sandboxes that can run
interactive coding agents such as Claude Code and Codex. The wrapper should hide
the low-level libkrun boot and device setup behind a narrow API that is centered
on:

- creating a named sandbox from a prepared agent image,
- mounting one or more host workspaces into the guest,
- optionally copying selected host credentials into the sandbox,
- booting the sandbox into an interactive shell,
- cleaning up disposable runtime state.

The first implementation should optimize for a reliable local developer
experience on macOS/ARM64 with libkrun HVF, while keeping the API portable enough
to support Linux/KVM later.

## Non-Goals

- Building a general-purpose VM manager.
- Supporting arbitrary ISO installers, GRUB, cloud images, or firmware boot.
- Automatically discovering and copying every possible host secret.
- Providing strong isolation from host resources exposed to the VMM process.
  libkrun's security model treats the guest and VMM as the same host security
  context unless the wrapper runs the VMM inside additional host isolation.
- Implementing a full terminal multiplexer or remote IDE.

## libkrun Mode Choice

The wrapper should use libkrun OS mode for the primary agent sandbox path.

Reasons:

- Claude Code and Codex expect a normal Linux userspace with Node.js, Git, shell
  tooling, package managers, and persistent home-directory state.
- OS mode boots a prepared guest-owned block root with `/sbin/init` as PID 1,
  matching the existing `examples/os_mode_agent_demo.md` direction in
  `../thousandbirds-libkrun`.
- Workload mode is simpler for launching one process, but it relies on
  libkrun's workload handoff and a virtio-fs root. That is a poorer fit for
  durable agent homes, package installation, and service-like behavior.

The design should still leave room for a later workload-mode backend for fast
single-command execution, but it should not be part of the first public API.

## Product Shape

The repository should expose both:

- a Rust library crate, for embedding sandbox creation in other tools;
- a CLI binary, for local usage and testing.

The first implementation should orchestrate the existing
`../thousandbirds-libkrun/examples/os_mode` launcher instead of linking libkrun
directly. That keeps this wrapper small and makes libkrun-specific boot details
stay in the libkrun repository. If the wrapper needs a lower-level boot knob,
add it to `examples/os_mode` first and pass it through from here.

Proposed package layout:

```text
Cargo.toml
src/
  lib.rs
  error.rs
  config.rs
  credentials.rs
  image.rs
  libkrun.rs
  runtime.rs
  cli.rs
  main.rs
docs/
  agent-sandbox-wrapper-design.md
guest/
  agent-sandbox-import-credentials
  agent-sandbox-shell
  agent-sandbox-shell.service
images/
  debian-bookworm.Containerfile
  debian-bookworm-rust.Containerfile
  debian-bookworm-go.Containerfile
  ubuntu-noble.Containerfile
scripts/
  build-agent-image.sh
```

Agent images should be built on demand with `scripts/build-agent-image.sh`.
The default variant should stay small and broadly useful; heavier language
toolchains should live in optional variants so they are not present in every
sandbox by default.

## Core Concepts

### Agent Image

An agent image is a prepared OS-mode bundle or local image directory containing:

- guest kernel,
- optional initramfs,
- base raw root disk,
- metadata describing architecture, kernel format, root filesystem, default
  guest user, console device, and installed agent tools.

The wrapper should not build images in the first implementation. It should
consume images built by `../thousandbirds-libkrun` tooling.

Expected first image contract:

- architecture: `aarch64` on Apple Silicon,
- init: `/sbin/init`,
- root device: `/dev/vda`,
- filesystem: `ext4`,
- console: `ttyAMA0`,
- default guest user: `agent`,
- installed tools: `git`, `ripgrep`, `node`, `npm`, `claude`, and `codex`.

### Sandbox

A sandbox is one launchable runtime instance created from an agent image. It has:

- a stable sandbox id or name,
- a per-sandbox writable root disk clone,
- a generated staging directory for copied credentials,
- configured virtio-fs workspace mounts,
- runtime metadata recording the image and host paths used.

By default, sandboxes should be persistent until deleted. A `--ephemeral` flag
can create a sandbox that is removed after the VM exits.

### Workspace Mount

Host workspaces should be mounted into the guest with virtio-fs. The default
mount convention should be:

```text
host path:  /path/to/project
tag:        workspace
guest path: /workspace
```

The guest image should include a small mount helper or systemd unit that mounts
known virtio-fs tags before the login shell starts. The wrapper should avoid
mounting arbitrary host directories unless the user explicitly asks for them.

### Interactive Shell

The primary user experience should be:

1. prepare or reuse a sandbox,
2. boot the guest,
3. attach the host terminal to an interactive shell as the `agent` user,
4. let the user invoke `claude`, `codex`, or ordinary shell commands manually.

This keeps the wrapper focused on creating the sandbox boundary and avoids
baking one agent-specific control path into the launcher. Agent-specific CLI
commands can still exist later as shortcuts, but they should be thin aliases
over "boot and attach to shell, then run an initial command."

The shell should start in the mounted workspace when one is provided:

```text
/workspace
```

The guest prompt should make it clear that the user is inside the sandbox and
which workspace is mounted. For example:

```text
[sandbox:<name> /workspace]$
```

### Credential Profile

Credential copying must be explicit, profile-based, and inspectable. A
credential profile is a list of host paths and environment variables copied into
a staging directory before boot and then installed into the guest user's home.

Initial built-in profiles:

- `claude`: Claude Code authentication and configuration paths.
- `codex`: Codex authentication and configuration paths.
- `git`: optional Git identity, SSH keys, and known hosts.

The exact host paths should be configurable because agent credential layouts can
change. Built-in defaults should be treated as convenience defaults, not a
security boundary or compatibility guarantee.

Credential copying should support:

- `--credentials none`, the default for safest behavior;
- `--credentials claude`;
- `--credentials codex`;
- `--credentials claude,codex`;
- `--credential-path HOST_PATH:GUEST_RELATIVE_PATH`;
- `--credential-env NAME`.

The wrapper should never print credential file contents.

## Credential Handling Design

Credentials should not be mounted directly from the host by default. Instead,
the wrapper should copy the selected files into a sandbox-local staging
directory under the wrapper state root, then expose that staging directory to
the guest read-only for first-boot import.

Recommended flow:

1. Resolve the selected credential profile.
2. Expand host paths without following unexpected symlink escapes unless
   explicitly allowed.
3. Copy files into:

   ```text
   $STATE_DIR/sandboxes/<id>/credentials-staging/
   ```

4. Preserve directory structure, file modes where practical, and regular files
   only. Refuse sockets, devices, FIFOs, and world-writable paths.
5. Generate a manifest:

   ```json
   {
     "version": 1,
     "entries": [
       {
         "source_label": "~/.claude",
         "staged_path": "claude/.claude",
         "guest_path": "/home/agent/.claude",
         "mode": "copy"
       }
     ],
     "env": {
       "ANTHROPIC_AUTH_TOKEN": "present"
     }
   }
   ```

6. Mount the staging directory into the guest as read-only virtio-fs, for
   example at `/run/agent-sandbox/credentials`.
7. Run a guest-side import command as root before starting the login shell.
   The import should copy files into `/home/agent`, set ownership to
   `agent:agent`, and apply restrictive permissions.
8. Unmount or leave the read-only staging mount according to a
   `retain_credential_mount` option. The default should be to leave only the
   copied guest files.

This design intentionally creates a copy inside the sandbox root disk. That
matches the user request to copy credentials into the resulting sandbox and
allows login state to persist with the sandbox. It also means deleting a sandbox
must delete its writable root clone and credential staging directory.

## Default Credential Profiles

These defaults should be encoded in data structures, not scattered through CLI
code.

```text
claude:
  candidate host paths:
    ~/.claude
    ~/.config/claude
  guest target:
    /home/agent

codex:
  candidate host paths:
    ~/.codex
    ~/.config/codex
  guest target:
    /home/agent

git:
  candidate host paths:
    ~/.gitconfig
    ~/.ssh/config
    ~/.ssh/known_hosts
  optional candidate host paths:
    ~/.ssh/id_ed25519
    ~/.ssh/id_rsa
  guest target:
    /home/agent
```

The CLI should show the resolved paths and ask for explicit confirmation before
copying private key material unless `--yes` is supplied.

## State Directories

Default state root:

- macOS: `~/Library/Application Support/thousandbirds/agent-sandbox`
- Linux: `${XDG_STATE_HOME:-~/.local/state}/thousandbirds/agent-sandbox`

Default cache root:

- macOS: `~/Library/Caches/thousandbirds/agent-sandbox`
- Linux: `${XDG_CACHE_HOME:-~/.cache}/thousandbirds/agent-sandbox`

Sandbox directory:

```text
$STATE_DIR/sandboxes/<id>/
  sandbox.json
  root.raw
  credentials-staging/
  logs/
```

Image cache directory:

```text
$CACHE_DIR/images/<image-id>/
  image.json
  kernel
  initramfs
  root.raw
```

On APFS, root disks should be created with a clone operation when possible.
Fallback should be an ordinary file copy with clear logging.

## Public Rust API

The library API should be small and declarative. It is also the production
integration surface for other Thousandbirds services; the CLI must remain a thin
consumer of this SDK rather than the only supported automation interface.

```rust
pub struct SandboxConfig {
    pub name: Option<String>,
    pub image: AgentImageRef,
    pub cpus: u8,
    pub memory_mib: u32,
    // Runtime memory-resize headroom. When memory_max_mib > memory_mib, the VM
    // boots at memory_max_mib (the ceiling) with the virtio-balloon inflated so
    // the guest sees memory_mib, and usable memory can later be moved within
    // [memory_min_mib, memory_max_mib] via SandboxManager::resize_memory.
    pub memory_max_mib: u32,
    pub memory_min_mib: u32,
    pub workspace_mounts: Vec<WorkspaceMount>,
    pub credential_profiles: Vec<CredentialProfile>,
    pub extra_credentials: Vec<CredentialSpec>,
    pub shell: ShellConfig,
    pub ephemeral: bool,
}

pub struct Sandbox {
    pub id: SandboxId,
    pub state_dir: PathBuf,
}

pub struct SandboxManager {
    pub fn create(&self, config: SandboxConfig) -> Result<Sandbox>;
    pub fn shell(&self, id: &SandboxId) -> Result<ExitStatus>;
    pub fn run_shell(&self, config: SandboxConfig) -> Result<ExitStatus>;
    pub fn delete(&self, id: &SandboxId) -> Result<()>;
    pub fn list(&self) -> Result<Vec<SandboxSummary>>;
    // Live-adjust a running sandbox's usable memory via virtio-balloon. Only
    // valid when the sandbox was created with resize headroom.
    pub fn resize_memory(&self, id: &SandboxId, target_mib: u32) -> Result<String>;
}
```

`SandboxManager::run_shell` should be the main high-level helper used by the
CLI. It creates or reuses a sandbox, prepares credentials, launches libkrun,
attaches the terminal to the guest shell, and returns the VM exit status.

### SDK Runtime Surface

App-builder generation needs an async, non-interactive SDK path that can be
retried and reopened by a host-local node agent. This should live in this crate
before app-builder treats VM-backed generation as production-ready.

Required additions:

- Reopenable sessions: `SandboxManager::reopen(id)` loads durable state for a
  sandbox created by an earlier process on the same host.
- Detached start: `start_session(id)` boots the VM without attaching an
  interactive terminal to the caller.
- Non-interactive exec: `exec(id, GuestCommand)` runs a command with explicit
  argv/env/cwd, streams stdout/stderr, and returns structured exit status.
- Cancellation: `cancel_process(id, process_id)` requests termination of a
  guest process without deleting the whole sandbox.
- Port forwarding: `forward_port(id, guest_port)` returns a host endpoint for
  services such as `chidori serve`.
- Mount inspection: `inspect_mounts(id)` returns tag, host path, guest path, and
  read-only status for every mounted workspace.
- Test backend: a fake or local-process backend exercises the same async SDK
  contract without booting libkrun, so downstream services can test leases,
  retries, and cancellation deterministically.

Sketch:

```rust
pub struct VmSession {
    pub id: SandboxId,
    pub state_dir: PathBuf,
}

pub struct GuestCommand {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
}

pub struct GuestProcess {
    pub id: String,
    pub stdout: tokio::sync::mpsc::Receiver<String>,
    pub stderr: tokio::sync::mpsc::Receiver<String>,
    pub exit: tokio::sync::oneshot::Receiver<Result<GuestExitStatus>>,
}

pub struct PortForward {
    pub host: String,
    pub port: u16,
}

pub struct WorkspaceMountInfo {
    pub tag: String,
    pub host_path: PathBuf,
    pub guest_path: String,
    pub read_only: bool,
}
```

Initial implementation status: the crate now exports `SandboxRuntimeSdk`,
`SandboxManagerSdk`, `GuestCommand`, `GuestProcess`, `GuestExitStatus`,
`PortForward`, `WorkspaceMountInfo`, and `LocalProcessSandboxRuntime`. The local
backend streams stdout/stderr, returns structured exit status, supports
cancellation, and preserves mount metadata for contract tests. `start_session`
starts a detached libkrun launch, records the host launcher PID in
`session.json`, writes stdout/stderr to session log files, treats an
already-running session as idempotent, and boots an in-guest command server over
a writable `agent-control` virtio-fs mount. `SandboxManagerSdk::exec` detects an
active command server, writes a request script into
`runtime-control/requests`, asynchronously tails stdout/stderr from
`runtime-control/responses`, returns structured exit status, and requests
cancellation by writing a cancel marker. When no session is active, `exec`
retains the legacy one-shot `INITIAL_COMMAND` launch path with piped
stdout/stderr. Persisted process metadata supports same-host cancellation
handoff from a replacement SDK instance, and `stop_session`/`delete_session`
terminate persisted active launch processes before cleanup.

Runtime config now includes a `mounts.tsv` file for every declared workspace
mount, and `agent-sandbox-shell` mounts those virtio-fs tags at their configured
guest paths instead of assuming only the default `workspace` tag exists. The
guest shell also mounts the writable `agent-control` tag at
`/run/agent-sandbox/control`, makes that mount writable by the guest agent user,
and the host runtime creates the backing `runtime-control` directory with
guest-writable permissions. `SandboxManagerSdk::forward_port` now starts a
guest-side bridge through the active command queue, binds an ephemeral
`127.0.0.1` port on the host, and asynchronously relays host TCP connections
through per-connection files under `agent-control`. The bridge makes those
connection files guest-writable and retries guest-port connection during service
startup so host health checks can race safely with a newly-started service.
Richer service-discovery and explicit forward lifecycle management are still
pending.

Verification note: after rebuilding `target/agent-images/debian-bookworm`, a
real libkrun VM smoke boot with separate `generation` and `meta-agent` mounts
printed `vm-mount-smoke-ok`, confirming the rebuilt guest image sees declared
mounts at `/workspace/generation` and `/workspace/meta-agent`.
Unit coverage now includes fake-`os_mode` tests for both the legacy one-shot SDK
exec path, the active-session command queue path, and active-session port
forwarding. App-builder also has an ignored real-libkrun SDK generation smoke
that boots the rebuilt image, executes Chidori through the active session
command queue, and commits output from the mounted disk workspace.

## CLI Design

Initial commands:

```text
tb-sandbox shell [OPTIONS] --workspace PATH
tb-sandbox create [OPTIONS]
tb-sandbox start SANDBOX_ID
tb-sandbox list
tb-sandbox delete SANDBOX_ID
tb-sandbox inspect SANDBOX_ID
tb-sandbox resize SANDBOX_ID --memory-mib N
```

Common options:

```text
--image PATH_OR_REF
--name NAME
--cpus N
--memory-mib N
--memory-max-mib N
--memory-min-mib N
--workspace PATH
--mount TAG=HOST_PATH[:GUEST_PATH]
--credentials none|claude|codex|git|PROFILE[,PROFILE...]
--credential-path HOST_PATH:GUEST_RELATIVE_PATH
--credential-env NAME
--ephemeral
--initial-command COMMAND
--yes
--log-level error|warn|info|debug|trace
```

Examples:

```sh
tb-sandbox shell \
  --image ~/Library/Caches/libkrun/os-bundles/debian-agent \
  --workspace .
```

Inside the guest shell:

```sh
claude
```

```sh
tb-sandbox shell \
  --workspace . \
  --credentials claude,codex
```

Inside the guest shell:

```sh
codex
```

An optional initial command can be provided for convenience, but it should still
land the user in the same booted sandbox shell model:

```sh
tb-sandbox shell \
  --workspace . \
  --credentials claude \
  --initial-command claude
```

## libkrun Launch Sequence

The wrapper should centralize libkrun launcher command construction in
`src/libkrun.rs`.

Expected OS-mode launch sequence:

1. Resolve `../thousandbirds-libkrun/examples/os_mode`, or use
   `TB_SANDBOX_OS_MODE`.
2. Create or reuse a per-sandbox writable root disk clone.
3. Generate a read-only runtime config virtio-fs mount.
4. Generate a read-only credential staging virtio-fs mount when credentials are
   selected.
5. Launch `os_mode` with kernel, initramfs, root disk, root device, root
   filesystem, CPU count, memory size, serial console, workspace virtio-fs
   mounts, `agent-config`, and optional `agent-credentials`.
6. Append `systemd.unit=agent-sandbox-shell.service` to the kernel command line
   unless the image already supplies it.

`examples/os_mode` remains the code that calls libkrun APIs such as
`krun_set_os_mode`, `krun_set_kernel`, `krun_add_disk3`,
`krun_set_os_root`, `krun_add_serial_console_default`, and
`krun_start_enter`.

Because `krun_start_enter` enters the VM run loop inside the launcher, the first
implementation can run one sandbox per wrapper process. Multi-sandbox management
can be handled by spawning multiple wrapper processes later.

The host terminal should be connected to the guest console or another
interactive control channel in raw mode so line editing, full-screen terminal
programs, and Ctrl-C behavior work as expected inside the sandbox shell.

## Guest-Side Contract

The prepared guest image should provide:

- user `agent`,
- `/usr/local/bin/agent-sandbox-import-credentials`,
- `/usr/local/bin/agent-sandbox-shell`,
- systemd unit or init hook that mounts declared virtio-fs tags,
- serial console or control channel for interactive shell access,
- Claude Code and Codex installed for the `agent` user or available on `PATH`.

`agent-sandbox-shell` should:

1. import credentials when `/run/agent-sandbox/credentials/manifest.json`
   exists,
2. mount every entry in `/run/agent-sandbox/config/mounts.tsv` at its declared
   guest path, falling back to the legacy `workspace` tag when no mount manifest
   exists,
3. switch to the `agent` user,
4. `cd` into the mounted workspace,
5. optionally run an initial command,
6. exec the configured login shell.

The host wrapper should pass shell startup settings through a small config file
mounted by virtio-fs. A mounted config file is preferred because it avoids shell
quoting issues and leaves room for future settings such as environment variables
or terminal size.

The first implementation should attach directly to a serial-backed shell. A
future control channel can add structured exit status, resize events, and
reattach support without changing the high-level CLI contract.

## Runtime Memory Resize

A sandbox created with resize headroom (`--memory-max-mib` greater than
`--memory-mib`) can have its guest memory adjusted live via the guest
`virtio-balloon` device. This is memory only; there is no runtime vCPU resize,
because the underlying VMM has no CPU hotplug.

Design:

- The VM boots with `memory_max_mib` of RAM as a fixed ceiling. The balloon
  starts inflated by `memory_max_mib - memory_mib` MiB so the guest's usable
  memory equals `memory_mib`. The boot ceiling cannot be exceeded at runtime —
  growing past it would need memory hotplug, which is not implemented.
- `examples/os_mode` is launched with `--balloon-initial-mib`,
  `--balloon-min-mib`, and `--control-socket <state>/sandboxes/<id>/control.sock`
  (a path under the sandbox state dir). The libkrun launcher serves a small
  line-based control protocol on that Unix socket and drives the balloon.
- `SandboxManager::resize_memory` (CLI `tb-sandbox resize`) is a separate
  short-lived host process that connects to the control socket and sends a
  `target-mib N` command, clamped to `[memory_min_mib, memory_max_mib]`. This
  fits the existing model where each CLI verb is its own invocation: `start`
  runs the blocking launcher, `resize` talks to the socket the launcher serves.
- `memory_max_mib` and `memory_min_mib` are persisted in `sandbox.json` so a
  later `start`/`resize` of the same sandbox keeps the configured range.

This control channel is host-to-VMM (over the balloon), and is distinct from any
future host-to-guest channel for terminal resize or reattach.

## Security Considerations

- Credential copying is opt-in.
- The wrapper should show exactly which paths will be copied before copying.
- Private key files require confirmation unless `--yes` is supplied.
- Credential staging is sandbox-specific and deleted with the sandbox.
- Staged credentials should be readable only by the host user running the
  wrapper.
- Workspace mounts expose host files to the guest through the VMM. Users should
  mount only the workspace they intend the agent to modify.
- Base images must never be attached read-write. Always launch from a per-sandbox
  root clone or copy.
- Non-raw disk image probing should be avoided. The image manifest should state
  the disk format explicitly.

## Error Handling

Errors should include actionable context:

- missing libkrun build or missing dynamic library,
- host architecture does not match image architecture,
- image manifest missing required files,
- root clone failed,
- selected credential path does not exist,
- selected credential path is an unsupported file type,
- guest import failed,
- libkrun API call failed with function name and errno-like code.

## Testing Strategy

Unit tests:

- credential profile path resolution,
- manifest generation,
- state directory selection,
- image manifest parsing,
- CLI argument parsing,
- libkrun error mapping.

Integration tests without booting a VM:

- create sandbox state from a fake image,
- copy sample credentials with mode preservation,
- refuse unsupported credential file types,
- generate expected libkrun launch config.

Optional host integration tests:

- boot a minimal prepared agent image,
- mount a temporary workspace,
- copy fake Claude/Codex credential directories,
- verify the guest sees credentials owned by `agent`,
- attach to an interactive shell and run `pwd`,
- run `claude --version` and `codex --version` from that shell when installed.

## Implementation Milestones

1. Rust crate scaffold, CLI skeleton, error type, and config structs.
2. Image manifest loading and sandbox state directory creation.
3. Credential profile resolution, copy logic, and manifest generation.
4. Root disk clone/copy creation with APFS clone support on macOS.
5. `examples/os_mode` command construction for the libkrun OS-mode launch.
6. `tb-sandbox shell` against a local prepared image.
7. Optional `--initial-command` support for shell startup convenience.
8. Cleanup, inspect, and list commands.
9. SDK runtime surface: reopenable sessions, detached start, non-interactive
   exec streams, cancellation, port forwarding, mount inspection, and a test
   backend.
10. Refactor `tb-sandbox` commands to consume the SDK surface rather than
   owning separate orchestration logic.

## Decisions From Earlier Open Questions

- Agent images are built from this repository's `images/` and `guest/` inputs
  via `scripts/build-agent-image.sh`, while the low-level rootfs builder remains
  in `../thousandbirds-libkrun`.
- Built-in Codex credential discovery is intentionally small:
  `~/.codex/auth.json`, `~/.codex/config.toml`, and the matching
  `~/.config/codex/*` paths, plus portable auth through `OPENAI_API_KEY`.
- Credentials are imported on every boot from the staged manifest. The staging
  directory is sandbox-local and does not reread host credentials after create,
  snapshot, restore, or fork.
- The current structured control path is the `agent-control` virtio-fs mount
  used by `SandboxManagerSdk`; the planned vsock transport below is the next
  transport upgrade, not a public API shape change.
- Networking requires an explicit guest network backend (`--passt-socket`,
  `--gvproxy`, or `--gvproxy-socket`). The host HTTP proxy can be enabled by
  default, but without a backend the guest has no route to it.

## Planned: vsock Host↔Guest Control Channel

This is a **design, not yet implemented**. It replaces the current
filesystem-over-virtiofs IPC used by `SandboxManagerSdk` (the in-guest shell
"command server" polling `runtime-control/requests`, the host tailing
`runtime-control/responses`, and the port forwarder relaying bytes through
append-only files). That transport works but has structural costs: per-hop poll
latency (the guest loop, the host output tail, and the forward relay all sleep
on fixed intervals), unbounded relay-file growth for long-lived forwards, and a
text/line-oriented response format. A real bidirectional channel removes all of
these.

### Why vsock is the right primitive

The `../thousandbirds-libkrun` fork already ships everything needed; no libkrun
core changes are required (only the `examples/os_mode` launcher and the guest
image change):

- `krun_add_vsock(ctx, 0)` — create an explicit vsock device. The header
  (`include/libkrun.h`) notes OS mode *requires* an explicit device; implicit
  vsock is disabled for OS-mode boots.
- `krun_add_vsock_port(ctx, port, unix_socket_path)` /
  `krun_add_vsock_port2(ctx, port, unix_socket_path, listen)` — map a guest
  vsock port to a host `AF_UNIX` socket. Guest userspace connecting to
  `VMADDR_CID_HOST:<port>` reaches the host Unix socket (and vice versa with
  `listen`). This *does* reach guest userspace, unlike the balloon
  `--control-socket`, which is host↔VMM only.

A worked precedent exists in
`../thousandbirds-libkrun/tests/test_cases/src/test_vsock_guest_connect.rs`.

### Launcher changes (`examples/os_mode.c`)

Purely additive, modeled on the existing `--control-socket` handling:

1. Add a `--vsock-port PORT=SOCKET_PATH` option to the `getopt_long` table and
   the `struct cmdline` (it already carries `control_socket`, `passt_socket`,
   etc.).
2. After `krun_set_os_mode(...)`, if any vsock port was requested, call
   `krun_add_vsock(ctx_id, 0)` once, then `krun_add_vsock_port(ctx_id, port,
   socket_path)` per mapping.

Per the repo's own guidance ("if the wrapper needs a lower-level boot knob, add
it to `examples/os_mode` first"), this is the sanctioned place for the change.

### Host side (this crate)

- `runtime.rs` (`launch_piped`): when the SDK requests it, allocate a
  per-sandbox host socket under the state dir (e.g.
  `<state>/sandboxes/<id>/control.vsock`) and pass
  `--vsock-port <AGENT_CONTROL_PORT>=<that path>` to the launcher. This is
  parallel to the existing `--control-socket` balloon wiring.
- `sdk.rs`: add a vsock-backed transport behind the existing
  `SandboxRuntimeSdk` trait (the trait is already the seam — see
  `SandboxManagerSdk` vs `LocalProcessSandboxRuntime`). Connect to the Unix
  socket, then speak a small length-prefixed protocol instead of writing request
  files and tailing response files. Keep the filesystem transport as a fallback
  / for images whose guest agent predates the vsock server.

### Guest side (agent image)

The current in-guest server is the POSIX-shell loop in
`guest_command_server_script()`. Replace it (for vsock-capable images) with a
small long-running agent that:

- listens on the agreed vsock port (`AF_VSOCK`, `VMADDR_CID_ANY`);
- accepts framed `Exec { argv, env, cwd }` requests, runs each in its own
  process group (`setsid`, as the current server now does), and streams
  `Stdout`/`Stderr`/`Exit { code }` frames;
- handles `Cancel { request_id }` by signalling the process group;
- multiplexes port forwards as bidirectional byte streams over the same channel
  (one logical stream per connection), eliminating the per-connection relay
  files and their unbounded growth.

This lives in the guest image (alongside `agent-sandbox-shell`) and requires an
image rebuild.

### Protocol sketch

Length-prefixed frames over the single vsock connection, each tagged with a
stream id for multiplexing:

```text
Open(stream_id, Exec { argv: [String], env: [(String,String)], cwd: Option<String> })
Open(stream_id, Forward { guest_port: u16 })
Data(stream_id, fd: Stdout|Stderr|ForwardBytes, bytes)
Cancel(stream_id)
Close(stream_id, exit_code: Option<i32>)
```

Structured paths travel as real fields, so host↔guest path translation (see the
SDK's `translate_guest_paths_to_host`) stays out of any in-band text rewriting.

### Migration & compatibility

- Gate on guest-agent capability: the SDK negotiates vsock on connect and falls
  back to the filesystem transport if the guest does not answer, so older images
  keep working.
- No public API change: `GuestCommand`, `GuestProcess`, `PortForward`, and the
  trait methods are unchanged; only the transport behind them differs.

### Verification (requires hardware — cannot be done with the fake `os_mode`)

The fake-`os_mode` unit tests cannot exercise a real vsock device. End-to-end
verification needs a guest-image rebuild and a real boot:

1. Rebuild `target/agent-images/debian-bookworm` with the vsock guest agent.
2. Build the patched `examples/os_mode` in the libkrun fork.
3. Run the `#[ignore]`d real-VM smoke tests in `app-agent-builder`
   (`APP_AGENT_RUN_VM_TESTS=1`, `APP_AGENT_SANDBOX_IMAGE=...`):
   `real_vm_sandbox_sdk_generation_smoke`,
   `real_vm_sandbox_sdk_forwarded_serve_generation_smoke`,
   `real_vm_sandbox_sdk_forwarded_serve_cancel_smoke`.
4. Confirm exec streaming, structured exit status, cancellation of a subprocess
   tree, and a forwarded `chidori serve` round-trip — all over vsock.

### Open questions for the channel

- Single multiplexed connection vs one vsock port per concern (control vs each
  forward)? Multiplexing keeps the launcher wiring to one `--vsock-port`.
- Frame encoding: hand-rolled length-prefixed frames vs a small dependency. The
  crate is currently near-zero-dependency by choice.
- Does the guest agent run as PID 1 helper under `agent-sandbox-shell`, or as a
  systemd unit started before the login shell?
