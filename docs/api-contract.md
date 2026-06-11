# Thousandbirds Agent Sandbox API Contract

## Purpose

This is the Phase 0 product contract for the Daytona gap plan. It defines the
stable runtime surface that the in-process Rust SDK, future localhost API
server, guest daemon transport, and generated SDKs must share.

The contract is intentionally transport-neutral. A direct Rust implementation,
HTTP/gRPC server, or guest-daemon bridge should expose the same resources,
state names, and error categories.

## Resources

### Sandbox

Fields:

- `id`: stable sandbox identifier.
- `state`: `created`, `running`, `stopped`, `deleting`, `error`, or `unknown`.
- `state_dir`: local state directory when the runtime is local.
- `image`: image reference and resolved image path.
- `cpus`, `memory_mib`, `memory_max_mib`, `memory_min_mib`.
- `network_backend`: `none`, `passt`, `gvproxy`, or `gvproxy-auto`.
- `mounts`: declared workspace/shared mounts with tag, host path, guest path,
  and read-only flag.
- `resources`: current reservation and optional live RSS when available.

Operations:

- `create(config) -> Sandbox`
- `start(id) -> Sandbox`
- `stop(id) -> Sandbox`
- `delete(id) -> Empty`
- `inspect(id) -> Sandbox`
- `list() -> [Sandbox]`

### Process

Fields:

- `id`: stable process identifier within a sandbox.
- `argv`, `cwd`, `env`.
- `state`: `starting`, `running`, `exited`, `cancelled`, or `error`.
- `exit`: exit code and success flag when complete.

Operations:

- `process.start(sandbox_id, command) -> Process`
- `process.stdout(process_id) -> stream<String>`
- `process.stderr(process_id) -> stream<String>`
- `process.wait(process_id) -> ExitStatus`
- `process.cancel(process_id) -> Empty`

### Filesystem

All paths are guest paths rooted at allowed mounts. Implementations may translate
paths to host paths only when the runtime executes commands on the host for
tests.

Operations:

- `fs.read(sandbox_id, path) -> bytes`
- `fs.write(sandbox_id, path, bytes, mode=create|overwrite|append) -> Empty`
- `fs.stat(sandbox_id, path) -> FileStat`
- `fs.list(sandbox_id, path) -> [FileStat]`
- `fs.mkdir(sandbox_id, path, parents: bool) -> Empty`
- `fs.delete(sandbox_id, path, recursive: bool) -> Empty`

The current in-process Rust SDK covers this surface through `exec`; the guest
daemon/API server should promote these to first-class operations.

### Git

Operations run inside the sandbox using the guest `git` binary:

- `git.clone(sandbox_id, url, path, ref?) -> Empty`
- `git.status(sandbox_id, path) -> GitStatus`
- `git.diff(sandbox_id, path, base?) -> String`
- `git.commit(sandbox_id, path, message) -> String`
- `git.branch(sandbox_id, path, name) -> Empty`
- `git.checkout(sandbox_id, path, ref) -> Empty`
- `git.pull(sandbox_id, path) -> Empty`
- `git.push(sandbox_id, path, remote?, branch?) -> Empty`

### Terminal

Operations:

- `terminal.open(sandbox_id, cwd?, rows?, cols?) -> TerminalSession`
- `terminal.input(session_id, bytes) -> Empty`
- `terminal.output(session_id) -> stream<bytes>`
- `terminal.resize(session_id, rows, cols) -> Empty`
- `terminal.close(session_id) -> Empty`

### Port Forward

Operations:

- `port.forward(sandbox_id, guest_port, host_port?) -> PortForward`
- `port.list(sandbox_id) -> [PortForward]`
- `port.close(sandbox_id, forward_id) -> Empty`

### Snapshot

Operations:

- `snapshot.create(sandbox_id, options) -> Snapshot`
- `snapshot.list() -> [Snapshot]`
- `snapshot.inspect(id) -> Snapshot`
- `snapshot.restore(id, options) -> Sandbox`
- `snapshot.delete(id) -> Empty`
- `snapshot.prune(delete: bool) -> SnapshotPruneReport`

Snapshot kinds are `disk-only` and `full`.

## Error Categories

Every implementation should map errors to one of:

- `invalid_argument`: malformed input or unsupported option.
- `not_found`: sandbox, snapshot, process, file, or forward does not exist.
- `resource_exhausted`: host admission budget or quota denied the request.
- `failed_precondition`: valid request against the wrong state.
- `unavailable`: runner, guest daemon, or transport is unreachable.
- `internal`: bug or unexpected runtime failure.
- `io`: local filesystem or OS error.

Error payloads should include a human-readable message and, when applicable, the
path, sandbox id, process id, or snapshot id that caused the failure.

## HTTP Transport (Phase 2)

The localhost API server (`tb-sandbox server`, `src/api.rs`) is the first
network transport for this contract. It is JSON over HTTP/1.1, localhost-first,
and authenticated with a bearer token (`Authorization: Bearer <token>`);
`GET /health` is unauthenticated.

Route map (all under `/v1` unless noted):

- `GET /health` -> `{status, version}`
- `GET /sandboxes` -> `{sandboxes: [{id, state, state_dir}]}`
- `POST /sandboxes` (body `{image, name?, cpus?, memory_mib?, memory_max_mib?,
  memory_min_mib?, network_backend?, mounts?}`) -> Sandbox
- `GET /sandboxes/{id}` -> Sandbox with `mounts`
- `POST /sandboxes/{id}/start` | `/stop` -> Sandbox
- `DELETE /sandboxes/{id}` -> `{}`
- `POST /sandboxes/{id}/exec` (body `{argv, cwd?, env?}`) -> chunked
  `application/x-ndjson` stream of events: `{type:"started",process_id}`,
  `{type:"stdout",data}`, `{type:"stderr",data}`, `{type:"exit",code,success}`.
- `POST /sandboxes/{id}/cancel` (body `{process_id}`) -> `{}`
- `GET /sandboxes/{id}/fs?path=` -> `{path, content}` (text-oriented)
- `PUT /sandboxes/{id}/fs?path=&mode=overwrite|append` (raw body) -> `{path, bytes}`
- `GET /sandboxes/{id}/git/status?path=` -> `{branch, clean, entries:[{status,path}]}`
- `GET /sandboxes/{id}/git/diff?path=&base=` -> `{diff}`
- `POST /sandboxes/{id}/git/clone` (body `{url, path, ref?}`) -> `{path}`
- `POST /sandboxes/{id}/ports` (body `{guest_port}`) -> `{host, port, guest_port}`
- `GET /events` -> chunked stream of structured lifecycle/process events.

Errors are returned as `{"error":{"category","message"}}` with the HTTP status
derived from the category (`invalid_argument`->400, `not_found`->404,
`resource_exhausted`->429, `failed_precondition`->409, `unavailable`->503,
`internal`/`io`->500). The CLI's API mode (`src/client.rs`) maps these back into
the same `Error` categories so local and API behavior match.

## Conformance

The first conformance target is the in-process Rust SDK. The test named
`sdk::tests::api_contract_conformance_local_process_runtime` exercises the
minimum Phase 0 surface available today: lifecycle, mount inspection, exec with
stdout/stderr/exit, file read/write through exec, cancellation, and deletion.

The HTTP transport runs the *same* behavioral suite through the real server and
client in `api::tests::api_contract_conformance_over_http`: it boots the server
over an in-process `LocalProcessSandboxRuntime`, then drives create, start,
inspect (mounts + state), list, streaming exec (stdout/stderr/exit + file
write), filesystem read/write, live cancellation, and delete-then-not-found over
HTTP — plus auth rejection. `api::tests::git_status_and_diff_over_http` covers
the Git surface (status/diff over a real repo). Future guest-daemon and runner
transports should run this suite against their transport rather than inventing
parallel expectations.
