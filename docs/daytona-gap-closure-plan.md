# Daytona Gap Closure Plan

## Purpose

Close the product and platform gap between `thousandbirds-agent-sandbox` and
Daytona while keeping our core differentiation: libkrun-backed full Linux VMs
for local and agent workloads, with stronger isolation and macOS/HVF density
work instead of a Docker-first runner.

This plan is intentionally specific. It names the missing capabilities, where
they should land, the order to build them, and the exit criteria for each phase.

## Baseline

What we already have:

- **VM substrate:** full OS-mode libkrun guests launched through
  `../thousandbirds-libkrun/examples/os_mode`.
- **Local lifecycle:** create, start/shell, exec, resize, list, inspect, delete.
- **Guest execution:** SDK process execution and cancellation through a mounted
  runtime-control directory.
- **Port forwarding:** localhost forwards implemented by host and guest relay
  processes.
- **Image builds:** container-derived root disks plus guest shell services.
- **Density primitives:** APFS root clones, single-host memory admission,
  optional runtime balloon resize, idle reclaim, and a basic warm pool.
- **Networking:** passt on Linux, gvproxy on macOS, plus a host HTTP proxy with
  selective TLS interception for header substitution.

What Daytona has that we do not:

- A network API and stable remote control contract.
- Multi-language SDKs backed by that API.
- An in-sandbox daemon with first-class file, Git, process, terminal, log, and
  port/preview APIs.
- A control plane with auth, tenants, quotas, lifecycle state reconciliation,
  runner registration, and scheduling.
- A runner service that polls for jobs and reports health/metrics.
- Snapshot and volume services backed by a registry/object store.
- Preview proxy, SSH gateway, web terminal, dashboard, and observability.
- Operational packaging for self-hosted and managed deployments.

## Strategic Choice

Do not copy Daytona's Docker runner as the primary implementation. Build the
Daytona-like platform surface on top of our VM runtime:

- Containers remain an image-build input, not the sandbox isolation boundary.
- The guest daemon becomes the main execution API, replacing control-directory
  shell polling for production use.
- The local CLI remains a thin client of the same API used by SDKs and services.
- The first control plane is single-node and local-first; multi-runner support
  comes after the daemon and state model are stable.

## Phase 0 - Product Contract

Goal: define the stable surface before expanding implementation.

Status: initial repo-local contract is checked in at
[`api-contract.md`](api-contract.md). The first conformance target is the
in-process Rust SDK, covered by
`sdk::tests::api_contract_conformance_local_process_runtime`. Future daemon and
server transports should run the same behavioral suite rather than defining
parallel semantics.

Completed in Phase 0:

- Checked in the transport-neutral contract at
  [`api-contract.md`](api-contract.md), covering lifecycle, process, filesystem,
  Git, terminal, port forwarding, and snapshots.
- Mapped the current `SandboxRuntimeSdk` semantics to the contract through
  `sdk::tests::api_contract_conformance_local_process_runtime`.
- Captured the shared error categories that CLI, API, and SDK transports should
  use.

Remaining Phase 0 refinements:

- Convert the markdown contract into OpenAPI or protobuf when the localhost API
  server starts.
- Extend conformance coverage for first-class filesystem, Git, terminal, port
  forwarding, logs, and live resource metrics as those transports land.
- Run the same conformance suite against the future HTTP/gRPC API.

Acceptance criteria:

- One checked-in API spec. **Met by `docs/api-contract.md`.**
- One Rust conformance test suite that exercises lifecycle, exec, file
  read/write, and cancellation. **Met by
  `sdk::tests::api_contract_conformance_local_process_runtime`.**
- Existing CLI behavior is representable by the new contract. **Met for the
  current lifecycle, exec, mount, and snapshot surfaces; new daemon/API features
  should extend the same contract instead of creating a second shape.**

## Phase 1 - Guest Daemon

Goal: replace ad hoc request files with a durable in-guest control service.

Status: **implemented** (host stack verified end-to-end on macOS; on-VM boot is
the one remaining manual step, gated on a guest kernel with vsock — same shape
as every other phase's hardware gate).

Architecture: the daemon exposes a single multiplexed connection over
`AF_VSOCK`. libkrun bridges the guest vsock port to a per-sandbox
`control.vsock` `AF_UNIX` socket on the host
(`krun_add_vsock_port2(.., listen=true)`), so the host SDK is just a Unix-socket
client. The server is transport-generic — it takes any `AsyncRead + AsyncWrite`
— so the identical code path runs over `AF_UNIX` in tests and `AF_VSOCK` in the
guest. That makes the full exec/stream/cancel/PTY/fs/git behavior testable
without a VM.

Done:

- **`crates/agent-sandbox-protocol`** (new): the framed wire protocol shared by
  host and guest — `u32`-length-prefixed JSON messages, binary payloads
  base64-encoded, multiplexed by correlation id. Zero VMM dependency so it
  cross-compiles into the static guest binary. Unit-tested (framing, base64,
  message accessors).
- **`crates/agent-sandbox-daemon`** (new): the in-guest server (lib + thin bin).
  Implements process start with streamed stdout/stderr, cancel (kills the whole
  process group via `setsid` + `killpg`), PTY sessions with live resize
  (`TIOCSWINSZ`) and stdin, first-class binary-safe filesystem ops
  (read/write/stat/list/mkdir/delete), git via the guest `git`, and
  health/version. Binds `AF_VSOCK` on Linux (a small `AsyncFd`-wrapped listener)
  or `AF_UNIX` for tests/local debugging. 7 integration tests drive the real
  server over a Unix socket.
- **`DaemonClient`** (`src/daemon_client.rs`): the host transport. Connects to
  `control.vsock`, demultiplexes replies/output by id, and reassembles byte
  chunks into the line-oriented `GuestProcess` the SDK already exposes — so
  callers are unchanged regardless of transport. 6 conformance tests drive the
  real daemon server through the real client.
- **SDK wiring**: `SandboxManagerSdk::exec` prefers the daemon for active
  sessions (connect + health, cached per sandbox) and falls back to the
  request-file command server when it's unreachable; `cancel_process` signals
  daemon-backed processes over vsock. `TB_SANDBOX_NO_DAEMON=1` forces the
  fallback.
- **Launcher plumbing**: `LaunchConfig::vsock_ports` → `--vsock-port PORT=PATH`,
  emitted by `src/runtime.rs` for every session boot (host socket
  `<state>/sandboxes/<id>/control.vsock`). Implemented in both the primary Rust
  `tb-sandbox-runner` (`krun_add_vsock` + `krun_add_vsock_port2(.., listen=true)`)
  and the legacy `examples/os_mode.c`.
- **Guest image**: `guest/agent-sandbox-daemon.service` starts the daemon
  `Before=agent-sandbox-shell.service`; `scripts/build-agent-image.sh`
  cross-compiles the daemon to a static musl binary in a throwaway Rust
  container (or accepts `DAEMON_BIN=`, or `SKIP_DAEMON=1`) and installs it.

Remaining (on-VM, hardware-gated):

- Boot a real guest with a vsock-capable kernel (`CONFIG_VIRTIO_VSOCKETS`) and
  confirm the host `DaemonClient` reaches the in-guest daemon end-to-end.
- **User-identity parity.** The daemon runs as root (systemd) and currently
  execs workloads as root; the legacy file-relay path runs them as the `agent`
  user (the session boots via `su - agent`). Per-exec privilege dropping
  (`setgid`/`setgroups`/`setuid` to `agent`, plus chowning the PTY slave to the
  target uid so an interactive shell keeps tty access) is a guest-only change
  that needs a real boot to verify, so it is deliberately left until the on-VM
  step rather than shipped unverified. Until then, prefer the daemon for
  ephemeral/build execs and keep credentialed agent runs on the file-relay path
  if root-owned workspace writes matter.
- Promote `fs`/`git` API-server endpoints and `forward_port` onto the daemon
  transport (today they still use the file-relay command server, which coexists
  with the daemon); add the PTY/SSH gateway (Phase 4) on top of the daemon's PTY
  sessions.

Acceptance criteria:

- `tb-sandbox exec` uses the daemon by default. **Met** — `exec` tries the
  daemon first for active sessions, falling back only when it's unreachable.
- stdout/stderr are streamed without file polling. **Met** — output flows as
  multiplexed frames over the connection (no response-file tailing).
- cancellation kills the guest process tree, not only the launcher process.
  **Met** — `setsid` + process-group signal; verified by
  `cancel_kills_the_whole_process_tree` (a shell that forks a child sleep).
- PTY resize works for an interactive shell. **Met** — verified by
  `pty_echoes_stdin_and_honors_resize` (`stty size` reflects the live resize).
- Existing exec/forward tests pass against both daemon and fallback modes.
  **Met** — the 99 lib tests (fake-launcher exec/forward) pass via the fallback;
  the daemon path is covered by the protocol/daemon/conformance suites.

## Phase 2 - Local API Server

Goal: expose the sandbox runtime over a real network API while staying
single-node.

Status: **largely implemented.** `tb-sandbox server` (`src/api.rs`) starts a
localhost HTTP/1.1 API over the same `SandboxRuntimeSdk` surface the in-process
SDK uses, and the CLI (`src/client.rs` + `cli.rs`) is now a client of that API
when `TB_SANDBOX_API_URL` is set. The transport is hand-rolled on Tokio to match
the repo's no-dependency house style (same approach as `proxy.rs`).

Done:

- `server` subcommand binds a localhost listener, generates/persists a bearer
  token at `<state_root>/api-token` (or `--no-auth` / `--token`), and serves the
  real `SandboxManagerSdk` runtime.
- Routes implement the Phase 0 contract: lifecycle (`list`/`create`/`inspect`/
  `start`/`stop`/`delete`), streaming `exec` (chunked NDJSON of
  `started`/`stdout`/`stderr`/`exit`), `cancel`, text `fs` read/write, and
  `ports` forward. Errors map to the contract's categories and HTTP status.
- Bearer-token auth on every non-health route; `GET /health` is unauthenticated.
- Structured JSON events for lifecycle transitions and process start/exit on a
  broadcast channel, streamed at `GET /v1/events` and printed by `server`.
- CLI API mode dispatches `create/list/inspect/start/stop/exec/delete` to the
  server (with live exec streaming) while leaving direct local mode unchanged.
- The trait gained `list_sessions` and `session_running` so `list`/`inspect`
  report state across both the real and in-process runtimes.

Remaining:

- Store sandbox metadata in SQLite instead of only per-sandbox JSON files.
  (Currently the server reconstructs running/stopped state from the manager's
  on-disk markers on every request, which already satisfies the restart
  criterion, but a metadata store is still wanted for richer queries.)
- Promote `fs` operations beyond the line-oriented `cat`/`printf` exec bridge to
  first-class binary-safe file ops (lands with the Phase 1 guest daemon).
- HTTP keep-alive (the server currently closes the connection per request).

Git surface: `git status`, `git diff`, and `git clone` are exposed as API
endpoints (executed via the guest `git`) and covered by
`api::tests::git_status_and_diff_over_http`. The remaining contract Git ops
(commit/branch/checkout/pull/push) follow the same exec pattern when needed.

Acceptance criteria:

- `tb-sandbox server` can create/start/exec/delete a sandbox from HTTP clients.
  **Met** (verified by curl + the CLI API-mode smoke path).
- CLI works both in direct local mode and API mode. **Met.**
- Server restart reconstructs running/stopped state from metadata and markers.
  **Met** via the manager's on-disk running markers (SQLite store still TODO).
- API conformance tests pass against the server. **Met** by
  `api::tests::api_contract_conformance_over_http`, which runs the same
  behavioral suite as the in-process SDK conformance test against the real HTTP
  server + client (lifecycle, streaming exec, file round-trip, cancellation,
  not-found).

## Phase 3 - SDKs

Goal: make the platform usable from agent applications without embedding Rust.

Work:

- Generate TypeScript and Python clients from the Phase 0 API spec.
- Provide hand-written ergonomic wrappers for:
  - `sandbox.process.exec(...)`;
  - `sandbox.fs.read/write/list(...)`;
  - `sandbox.git.clone/status/diff(...)`;
  - `sandbox.ports.forward(...)`;
  - `sandbox.terminal.open(...)`.
- Add examples for Claude/Codex-style agent sessions and batch code execution.
- Publish SDK packages only after the API conformance suite is stable.

Acceptance criteria:

- Python and TypeScript examples can create a sandbox, write a file, run code,
  stream output, and delete the sandbox.
- SDK integration tests run against `tb-sandbox server`.
- API breaking changes require spec version updates.

## Phase 4 - Preview Proxy and SSH Gateway

Goal: match Daytona's human/tool access features without weakening the VM model.

Status: **preview proxy implemented; SSH/PTY gateway still TODO.** The reverse
proxy lives in `src/preview.rs` and is exposed as `tb-sandbox preview`.

Done (preview proxy):

- Localhost reverse proxy routing `{guest_port}-{sandbox_id}.localhost`
  (Host header, with an optional `:proxy_port` suffix; sandbox ids may contain
  hyphens) to a host-side forward resolved live through the API server.
- HTTP and WebSocket support via a raw bidirectional splice
  (`tokio::io::copy_bidirectional`), so streaming and upgrades pass through
  without the proxy parsing the application protocol.
- Preview-token auth by default (`Authorization: Bearer` or `?token=` for
  browser links); `--no-auth` opts out.
- `tb-sandbox preview [--bind ADDR] [--no-auth] [ID PORT]` runs the gateway and
  prints a ready-to-open preview URL when an id/port pair is given.
- Routes are resolved per request through the API, so a forward that the API no
  longer serves (stopped/deleted sandbox) returns 404 rather than a stale route.

Remaining:

- SSH gateway support:
  - authenticate to the API;
  - connect to a guest PTY or guest sshd;
  - avoid mounting host SSH credentials into the guest implicitly.
- `tb-sandbox ssh <id>` command. (Best built on the Phase 1 guest daemon's PTY
  sessions rather than the serial console.)

Acceptance criteria:

- A web server running in a guest can be reached through the preview proxy.
  **Met** — verified by `preview::tests::preview_proxy_routes_host_to_forward`,
  which routes a Host-addressed request through the API forward to a real
  backend.
- WebSocket previews work. **Met by construction** (raw bidirectional splice
  carries the Upgrade handshake and frames unchanged).
- SSH or PTY gateway works without exposing the VM serial console directly.
  **TODO** (depends on the guest daemon).
- Preview routes disappear when the sandbox stops. **Met** — each request
  re-resolves the forward through the API and 404s when it is gone.

## Phase 5 - Snapshots and Image Registry

Goal: make creation fast and portable beyond local root-disk directories.

Work:

- Define snapshot types:
  - base image snapshot: immutable root disk, kernel, metadata;
  - runtime snapshot: stopped sandbox root disk plus metadata;
  - warm snapshot: libkrun Full snapshot/restore state for restore-backed
    startup policy.
- Implement local snapshot export/import:
  - compress root disk deltas where possible;
  - include image manifest and guest daemon version;
  - validate architecture, kernel format, root device, and feature flags.
- Add OCI registry compatibility for base images:
  - publish image metadata as an OCI artifact;
  - push/pull root disks and kernel artifacts;
  - cache by content digest.
- Add a snapshot builder service that accepts Dockerfile/container images and
  produces VM-ready base snapshots.

Acceptance criteria:

- A sandbox can be stopped, snapshotted, deleted, restored, and resumed from the
  restored root state.
- Base images can be pulled by digest into the local cache.
- Snapshot metadata records required libkrun/kernel features.
- Image build no longer depends on sibling checkout assumptions in the API path.

## Phase 6 - Volumes

Goal: support persistent data independent of sandbox lifecycle.

Work:

- Add volume objects to metadata: name, size, mount path, access mode.
- Implement local volumes as sparse raw disks or host directories mounted through
  virtiofs, with clear isolation semantics:
  - raw disk for stronger guest-owned persistence;
  - virtiofs for host-inspectable shared data.
- Add mount declarations to sandbox create/start.
- Add backup/restore hooks for future object-store support.
- Enforce read-only vs read-write modes per sandbox.

Acceptance criteria:

- Deleting a sandbox does not delete attached volumes unless explicitly asked.
- Multiple sandboxes can mount read-only shared volumes safely.
- Volume state survives server restart.
- Volume backup/export works for at least one local backend.

## Phase 7 - Runner Service

Goal: separate compute from control and prepare for multi-host scheduling.

Work:

- Add `tb-sandbox-runner` or `tb-sandbox runner`:
  - registers with the API server;
  - reports host architecture, memory, disk, libkrun features, image cache;
  - polls or streams jobs;
  - executes lifecycle, exec, snapshot, and volume operations.
- Move single-host `ResourceLedger` behind a runner resource model.
- Add health checks and heartbeat expiry.
- Support one control plane with many local-network runners before adding cloud
  networking complexity.

Acceptance criteria:

- API can schedule a sandbox onto a registered runner.
- Runner loss marks sandboxes unknown/unreachable, not silently stopped.
- Jobs are idempotent across runner restart.
- Scheduler considers architecture, free memory, disk, and image locality.

## Phase 8 - Control Plane

Goal: add the platform features Daytona has without committing to SaaS-specific
complexity too early.

Work:

- Add tenants/projects:
  - org/project IDs;
  - API keys scoped to projects;
  - per-project quotas for sandboxes, CPU, memory, disk, and runtime duration.
- Add lifecycle policies:
  - auto-stop;
  - auto-delete;
  - max runtime;
  - idle detection.
- Add state reconciliation:
  - desired state vs observed state;
  - background controller loop;
  - retry/backoff;
  - explicit error states.
- Add audit logs and structured operation history.

Acceptance criteria:

- Project-scoped API keys cannot access other projects' sandboxes.
- Quotas are enforced before runner admission.
- Reconciliation recovers from API server restart and runner restart.
- Lifecycle policies are covered by integration tests.

## Phase 9 - Observability and Operations

Goal: make the system debuggable and deployable.

Work:

- Emit OpenTelemetry traces and metrics from API, runner, daemon, and proxy.
- Collect:
  - sandbox start latency;
  - boot phases;
  - memory reservation and RSS;
  - process execution latency;
  - exit codes;
  - proxy request counts;
  - snapshot build/pull latency.
- Add admin commands:
  - runner health;
  - sandbox repair/reconcile;
  - image cache inspect/prune;
  - volume inspect/backup;
  - support bundle collection.
- Add Docker Compose packaging for self-hosted local deployments.
- Add systemd/launchd service examples for API and runner.

Acceptance criteria:

- A failing sandbox start has enough logs/events to identify image, runner,
  libkrun, network, or guest-daemon failure.
- Metrics can be scraped locally.
- A clean machine can run the full API + runner stack from documented commands.

## Phase 10 - Dashboard

Goal: add a human operations surface after the backend contract is stable.

Work:

- Build a minimal dashboard against the same public API:
  - sandbox list/detail;
  - create/start/stop/delete;
  - terminal;
  - process logs;
  - preview links;
  - image/snapshot list;
  - runner health.
- Keep it operational, not marketing-oriented.

Acceptance criteria:

- A user can manage sandboxes without the CLI.
- Dashboard uses only public API endpoints.
- Terminal and log streaming work through the proxy/API stack.

## Cross-Cutting Requirements

Security:

- Keep credentials explicit and profile-based.
- Never mount host credential directories directly by default.
- Treat guest code as untrusted.
- Add an optional outer host isolation layer for the VMM process before claiming
  strong host escape resistance.
- Default network exposure to private/local; require explicit preview sharing.

Compatibility:

- macOS/ARM64 remains the first-class local target.
- Linux/KVM support must use the same API and daemon contract.
- Image manifests must record architecture and required feature flags.

Testing:

- Every phase must add API conformance coverage.
- Keep fake-launcher tests for fast CI.
- Add real-VM smoke tests for daemon, exec, PTY, proxy, and snapshots.
- Add failure tests for guest daemon crash, runner restart, API restart, and
  stale running markers.

## Suggested Build Order

1. Phase 0: API contract and conformance tests. **Initial contract done.**
2. Phase 1: guest daemon. **Done** (host stack verified; on-VM boot pending).
3. Phase 2: local API server.
4. Phase 3: Python and TypeScript SDKs.
5. Phase 4: preview proxy and SSH/PTY gateway.
6. Phase 5: snapshots and OCI-compatible image cache.
7. Phase 6: volumes.
8. Phase 7: runner service.
9. Phase 8: control plane.
10. Phase 9: observability and deployment.
11. Phase 10: dashboard.

This order keeps the core execution contract stable before adding distributed
systems complexity. It also lets the local VM product become useful earlier:
daemon + API + SDKs close the biggest agent-integration gap without waiting for
multi-runner orchestration.

## First Milestone

The first shippable milestone should be:

- guest daemon enabled by default;
- localhost API server;
- CLI using the API optionally;
- Python and TypeScript SDK preview packages;
- conformance suite covering lifecycle, exec, file, Git status, PTY, and port
  forward;
- documented local deployment.

Phase 0's initial contract is done: the repo now has `docs/api-contract.md` and
an in-process Rust SDK conformance test covering lifecycle, exec, file
read/write through exec, cancellation, mount inspection, and deletion. The
remaining milestone work starts at the guest daemon/API server boundary.

That milestone would not match Daytona's full platform, but it would close the
most important developer-facing gap while preserving our libkrun VM advantage.
