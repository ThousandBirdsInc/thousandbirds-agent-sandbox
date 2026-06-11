//! Localhost HTTP API server (Daytona gap plan, Phase 2).
//!
//! This exposes the Phase 0 [`crate::sdk::SandboxRuntimeSdk`] surface over a
//! small, dependency-free HTTP/1.1 server built directly on Tokio. It is the
//! network "remote control contract" that the CLI (in API mode), generated
//! SDKs, and future runner/control-plane services share.
//!
//! Design notes:
//!
//! - Transport-neutral: every operation maps onto the same `SandboxRuntimeSdk`
//!   trait the in-process Rust SDK uses, so the HTTP server and the direct SDK
//!   cannot drift apart. The conformance suite runs against both.
//! - JSON is hand-rolled to match the repo's no-serde house style (see
//!   `image::json_*` and `credentials::escape_json`). Inbound bodies use the
//!   small recursive parser in this module; responses are built with
//!   `escape_json`.
//! - Auth is a static bearer token. `Auth::None` disables it (tests / opt-out).
//! - Lifecycle and process transitions are emitted as structured JSON events on
//!   a broadcast channel, consumable via `GET /v1/events` (SSE-style) or
//!   [`ApiServer::subscribe`].
//!
//! The server is single-node and localhost-first by default; binding is the
//! caller's choice via the [`tokio::net::TcpListener`] passed to [`ApiServer::serve`].

use crate::config::{AgentImageRef, NetworkBackend, SandboxConfig};
use crate::credentials::escape_json;
use crate::error::{Error, Result};
use crate::runtime::{SandboxId, WorkspaceMountInfo};
use crate::sdk::{GuestCommand, SandboxRuntimeSdk};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// Version reported by `GET /health`, sourced from the crate version.
pub const API_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How the server authenticates inbound requests.
#[derive(Debug, Clone)]
pub enum Auth {
    /// No authentication (local opt-out and test topology).
    None,
    /// Require `Authorization: Bearer <token>` on every non-health request.
    Bearer(String),
}

/// A localhost API server wrapping any [`SandboxRuntimeSdk`] implementation.
pub struct ApiServer<R> {
    state: Arc<ServerState<R>>,
}

struct ServerState<R> {
    runtime: Arc<R>,
    auth: Auth,
    events: broadcast::Sender<String>,
}

impl<R> Clone for ApiServer<R> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl<R> ApiServer<R>
where
    R: SandboxRuntimeSdk + Send + Sync + 'static,
{
    pub fn new(runtime: Arc<R>, auth: Auth) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            state: Arc::new(ServerState {
                runtime,
                auth,
                events,
            }),
        }
    }

    /// Subscribe to the structured event stream (lifecycle + process events).
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.state.events.subscribe()
    }

    /// Serve until the listener errors. Each connection handles one request and
    /// closes (`Connection: close`); this keeps the hand-rolled transport simple
    /// while remaining correct for CLI/SDK clients.
    pub async fn serve(self, listener: TcpListener) -> Result<()> {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(err) => return Err(Error::io(err)),
            };
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                if let Err(err) = handle_connection(stream, state).await {
                    // A broken client pipe is routine; log at a low volume.
                    if err.kind() != std::io::ErrorKind::BrokenPipe {
                        eprintln!("tb-sandbox api: connection error: {err}");
                    }
                }
            });
        }
    }
}

fn emit_event<R>(state: &ServerState<R>, event: &str) {
    // A send error just means no subscribers; that is fine.
    let _ = state.events.send(event.to_string());
}

// ---------------------------------------------------------------------------
// HTTP transport
// ---------------------------------------------------------------------------

struct RawRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn handle_connection<R>(stream: TcpStream, state: Arc<ServerState<R>>) -> std::io::Result<()>
where
    R: SandboxRuntimeSdk + Send + Sync + 'static,
{
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;
    let Some(request) = read_request(&mut reader).await? else {
        return Ok(());
    };
    dispatch(request, &state, &mut writer).await
}

async fn read_request<R>(reader: &mut BufReader<R>) -> std::io::Result<Option<RawRequest>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if key == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(key, value);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }

    let (path, query) = split_target(&target);
    Ok(Some(RawRequest {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let mut query = HashMap::new();
    let (path, qs) = match target.split_once('?') {
        Some((path, qs)) => (path, qs),
        None => (target, ""),
    };
    for pair in qs.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(key), percent_decode(value));
    }
    (path.to_string(), query)
}

/// Minimal percent-decoding for query parameters (covers `%XX` and `+`).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

async fn dispatch<R>(
    request: RawRequest,
    state: &ServerState<R>,
    writer: &mut OwnedWriteHalf,
) -> std::io::Result<()>
where
    R: SandboxRuntimeSdk + Send + Sync + 'static,
{
    let path = request.path.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    // Health is unauthenticated so liveness checks need no credentials.
    if request.method == "GET" && matches!(path, "/health" | "/v1/health" | "") {
        let body = format!(
            "{{\"status\":\"ok\",\"version\":\"{}\"}}",
            escape_json(API_VERSION)
        );
        return write_json(writer, 200, "OK", &body).await;
    }

    if !authorized(&request, &state.auth) {
        let body = error_body("unauthenticated", "missing or invalid API token");
        return write_json(writer, 401, "Unauthorized", &body).await;
    }

    // /v1/events: structured event stream (SSE-style chunked text).
    if request.method == "GET" && (path == "/v1/events" || path == "/events") {
        return stream_events(state, writer).await;
    }

    // All remaining routes live under /v1/sandboxes.
    let rest = match segments.as_slice() {
        ["v1", "sandboxes", rest @ ..] => rest,
        _ => {
            let body = error_body("not_found", "unknown route");
            return write_json(writer, 404, "Not Found", &body).await;
        }
    };

    match (request.method.as_str(), rest) {
        ("GET", []) => respond(writer, list_sandboxes(state).await).await,
        ("POST", []) => respond(writer, create_sandbox(state, &request.body).await).await,
        ("GET", [id]) => respond(writer, inspect_sandbox(state, id).await).await,
        ("DELETE", [id]) => respond(writer, delete_sandbox(state, id).await).await,
        ("POST", [id, "start"]) => respond(writer, start_sandbox(state, id).await).await,
        ("POST", [id, "stop"]) => respond(writer, stop_sandbox(state, id).await).await,
        ("POST", [id, "cancel"]) => {
            respond(writer, cancel_process(state, id, &request.body).await).await
        }
        ("POST", [id, "ports"]) => {
            respond(writer, forward_port(state, id, &request.body).await).await
        }
        ("GET", [id, "git", "status"]) => {
            respond(writer, git_status(state, id, &request.query).await).await
        }
        ("GET", [id, "git", "diff"]) => {
            respond(writer, git_diff(state, id, &request.query).await).await
        }
        ("POST", [id, "git", "clone"]) => {
            respond(writer, git_clone(state, id, &request.body).await).await
        }
        ("GET", [id, "fs"]) => respond(writer, read_file(state, id, &request.query).await).await,
        ("PUT", [id, "fs"]) => {
            respond(
                writer,
                write_file(state, id, &request.query, &request.body).await,
            )
            .await
        }
        ("POST", [id, "exec"]) => exec_stream(state, id, &request.body, writer).await,
        _ => {
            let body = error_body("not_found", "unknown route");
            write_json(writer, 404, "Not Found", &body).await
        }
    }
}

fn authorized(request: &RawRequest, auth: &Auth) -> bool {
    match auth {
        Auth::None => true,
        Auth::Bearer(token) => request
            .headers
            .get("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(|presented| presented.trim() == token)
            .unwrap_or(false),
    }
}

/// Write the success/error JSON body produced by a handler. Handlers return a
/// `Result<String>` where the `String` is the JSON success body.
async fn respond(writer: &mut OwnedWriteHalf, outcome: Result<String>) -> std::io::Result<()> {
    match outcome {
        Ok(body) => write_json(writer, 200, "OK", &body).await,
        Err(err) => {
            let (code, reason, category) = error_status(&err);
            let body = error_body(category, &err.to_string());
            write_json(writer, code, reason, &body).await
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_sandboxes<R>(state: &ServerState<R>) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let ids = state.runtime.list_sessions().await?;
    let mut entries = Vec::with_capacity(ids.len());
    for id in ids {
        let sandbox = state.runtime.reopen_session(&id).await?;
        let running = state.runtime.session_running(&id).await?;
        entries.push(sandbox_json(
            id.as_str(),
            &sandbox.state_dir.display().to_string(),
            running,
            None,
        ));
    }
    Ok(format!("{{\"sandboxes\":[{}]}}", entries.join(",")))
}

async fn create_sandbox<R>(state: &ServerState<R>, body: &[u8]) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let json = parse_body(body)?;
    let config = config_from_json(&json)?;
    let sandbox = state.runtime.create_session(config).await?;
    let mounts = state.runtime.inspect_mounts(&sandbox.id).await?;
    let running = state.runtime.session_running(&sandbox.id).await?;
    emit_event(
        state,
        &lifecycle_event("created", sandbox.id.as_str(), running),
    );
    Ok(sandbox_json(
        sandbox.id.as_str(),
        &sandbox.state_dir.display().to_string(),
        running,
        Some(&mounts),
    ))
}

async fn inspect_sandbox<R>(state: &ServerState<R>, id: &str) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    let sandbox = state.runtime.reopen_session(&id).await?;
    let mounts = state.runtime.inspect_mounts(&id).await?;
    let running = state.runtime.session_running(&id).await?;
    Ok(sandbox_json(
        id.as_str(),
        &sandbox.state_dir.display().to_string(),
        running,
        Some(&mounts),
    ))
}

async fn start_sandbox<R>(state: &ServerState<R>, id: &str) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    state.runtime.start_session(&id).await?;
    let sandbox = state.runtime.reopen_session(&id).await?;
    let running = state.runtime.session_running(&id).await?;
    emit_event(state, &lifecycle_event("started", id.as_str(), running));
    Ok(sandbox_json(
        id.as_str(),
        &sandbox.state_dir.display().to_string(),
        running,
        None,
    ))
}

async fn stop_sandbox<R>(state: &ServerState<R>, id: &str) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    state.runtime.stop_session(&id).await?;
    let running = state.runtime.session_running(&id).await?;
    emit_event(state, &lifecycle_event("stopped", id.as_str(), running));
    let sandbox = state.runtime.reopen_session(&id).await?;
    Ok(sandbox_json(
        id.as_str(),
        &sandbox.state_dir.display().to_string(),
        running,
        None,
    ))
}

async fn delete_sandbox<R>(state: &ServerState<R>, id: &str) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    state.runtime.delete_session(&id).await?;
    emit_event(state, &lifecycle_event("deleted", id.as_str(), false));
    Ok("{}".to_string())
}

async fn cancel_process<R>(state: &ServerState<R>, id: &str, body: &[u8]) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    let json = parse_body(body)?;
    let process_id = json
        .get("process_id")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::InvalidArgument("cancel requires a process_id".to_string()))?;
    state.runtime.cancel_process(&id, process_id).await?;
    emit_event(
        state,
        &format!(
            "{{\"type\":\"process\",\"event\":\"cancelled\",\"sandbox\":\"{}\",\"process_id\":\"{}\"}}",
            escape_json(id.as_str()),
            escape_json(process_id)
        ),
    );
    Ok("{}".to_string())
}

async fn forward_port<R>(state: &ServerState<R>, id: &str, body: &[u8]) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    let json = parse_body(body)?;
    let guest_port = json
        .get("guest_port")
        .and_then(Json::as_u64)
        .ok_or_else(|| Error::InvalidArgument("forward requires a guest_port".to_string()))?;
    if guest_port == 0 || guest_port > u16::MAX as u64 {
        return Err(Error::InvalidArgument(
            "guest_port must be in 1..=65535".to_string(),
        ));
    }
    let forward = state.runtime.forward_port(&id, guest_port as u16).await?;
    Ok(format!(
        "{{\"host\":\"{}\",\"port\":{},\"guest_port\":{}}}",
        escape_json(&forward.host),
        forward.port,
        guest_port
    ))
}

async fn read_file<R>(
    state: &ServerState<R>,
    id: &str,
    query: &HashMap<String, String>,
) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    let path = query
        .get("path")
        .ok_or_else(|| Error::InvalidArgument("fs read requires a ?path= parameter".to_string()))?;
    // Read through exec to stay on the transport-neutral surface. This is
    // line-oriented (suitable for text); binary fidelity will land with the
    // guest daemon's first-class file operations.
    let command = GuestCommand::new(["/bin/sh", "-c", "cat -- \"$0\"", path]);
    let (stdout, _stderr, status) = run_to_completion(state, &id, command).await?;
    if !status.success {
        return Err(Error::NotFound(format!("file {path}")));
    }
    let content = stdout.join("\n");
    Ok(format!(
        "{{\"path\":\"{}\",\"content\":\"{}\"}}",
        escape_json(path),
        escape_json(&content)
    ))
}

async fn write_file<R>(
    state: &ServerState<R>,
    id: &str,
    query: &HashMap<String, String>,
    body: &[u8],
) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    let path = query.get("path").ok_or_else(|| {
        Error::InvalidArgument("fs write requires a ?path= parameter".to_string())
    })?;
    let content = String::from_utf8_lossy(body).into_owned();
    let mode = query.get("mode").map(String::as_str).unwrap_or("overwrite");
    let redirect = match mode {
        "append" => ">>",
        "overwrite" | "create" => ">",
        other => {
            return Err(Error::InvalidArgument(format!(
                "unsupported write mode: {other}"
            )))
        }
    };
    // Pass content and path as separate argv tokens so neither needs in-shell
    // quoting; `printf '%s'` writes the bytes verbatim with no trailing newline.
    let script = format!("printf '%s' \"$0\" {redirect} \"$1\"");
    let command = GuestCommand::new(["/bin/sh", "-c", &script, &content, path]);
    let (_stdout, stderr, status) = run_to_completion(state, &id, command).await?;
    if !status.success {
        return Err(Error::Internal(format!(
            "fs write failed: {}",
            stderr.join("\n")
        )));
    }
    Ok(format!(
        "{{\"path\":\"{}\",\"bytes\":{}}}",
        escape_json(path),
        body.len()
    ))
}

/// Run a command to completion, collecting stdout/stderr lines and the exit
/// status. Used by the (line-oriented) filesystem helpers.
async fn run_to_completion<R>(
    state: &ServerState<R>,
    id: &SandboxId,
    command: GuestCommand,
) -> Result<(Vec<String>, Vec<String>, crate::sdk::GuestExitStatus)>
where
    R: SandboxRuntimeSdk,
{
    let mut process = state.runtime.exec(id, command).await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_done = false;
    let mut stderr_done = false;
    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            line = process.stdout.recv(), if !stdout_done => match line {
                Some(line) => stdout.push(line),
                None => stdout_done = true,
            },
            line = process.stderr.recv(), if !stderr_done => match line {
                Some(line) => stderr.push(line),
                None => stderr_done = true,
            },
        }
    }
    let status = process
        .exit
        .await
        .map_err(|_| Error::Internal("process exit channel closed".to_string()))??;
    Ok((stdout, stderr, status))
}

async fn git_status<R>(
    state: &ServerState<R>,
    id: &str,
    query: &HashMap<String, String>,
) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    let path = query.get("path").ok_or_else(|| {
        Error::InvalidArgument("git status requires a ?path= parameter".to_string())
    })?;
    let command = GuestCommand::new(["git", "-C", path, "status", "--porcelain=v1", "--branch"]);
    let (stdout, stderr, status) = run_to_completion(state, &id, command).await?;
    if !status.success {
        return Err(Error::InvalidArgument(format!(
            "git status failed: {}",
            stderr.join("\n")
        )));
    }
    let mut branch = String::new();
    let mut entries = Vec::new();
    for line in &stdout {
        if let Some(rest) = line.strip_prefix("## ") {
            branch = rest.split("...").next().unwrap_or(rest).trim().to_string();
        } else if line.len() >= 3 {
            let (code, file) = line.split_at(2);
            entries.push(format!(
                "{{\"status\":\"{}\",\"path\":\"{}\"}}",
                escape_json(code.trim()),
                escape_json(file.trim())
            ));
        }
    }
    Ok(format!(
        "{{\"branch\":\"{}\",\"clean\":{},\"entries\":[{}]}}",
        escape_json(&branch),
        entries.is_empty(),
        entries.join(",")
    ))
}

async fn git_diff<R>(
    state: &ServerState<R>,
    id: &str,
    query: &HashMap<String, String>,
) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    let path = query.get("path").ok_or_else(|| {
        Error::InvalidArgument("git diff requires a ?path= parameter".to_string())
    })?;
    let mut argv = vec![
        "git".to_string(),
        "-C".to_string(),
        path.to_string(),
        "diff".to_string(),
    ];
    if let Some(base) = query.get("base") {
        argv.push(base.to_string());
    }
    let command = GuestCommand::new(argv);
    let (stdout, stderr, status) = run_to_completion(state, &id, command).await?;
    if !status.success {
        return Err(Error::InvalidArgument(format!(
            "git diff failed: {}",
            stderr.join("\n")
        )));
    }
    Ok(format!(
        "{{\"diff\":\"{}\"}}",
        escape_json(&stdout.join("\n"))
    ))
}

async fn git_clone<R>(state: &ServerState<R>, id: &str, body: &[u8]) -> Result<String>
where
    R: SandboxRuntimeSdk,
{
    let id = SandboxId::new(id)?;
    let json = parse_body(body)?;
    let url = json
        .get("url")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::InvalidArgument("git clone requires a url".to_string()))?;
    let path = json
        .get("path")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::InvalidArgument("git clone requires a path".to_string()))?;
    let command = GuestCommand::new(["git", "clone", url, path]);
    let (_stdout, stderr, status) = run_to_completion(state, &id, command).await?;
    if !status.success {
        return Err(Error::Internal(format!(
            "git clone failed: {}",
            stderr.join("\n")
        )));
    }
    if let Some(reference) = json.get("ref").and_then(Json::as_str) {
        let checkout = GuestCommand::new(["git", "-C", path, "checkout", reference]);
        let (_out, err, status) = run_to_completion(state, &id, checkout).await?;
        if !status.success {
            return Err(Error::InvalidArgument(format!(
                "git checkout {reference} failed: {}",
                err.join("\n")
            )));
        }
    }
    Ok(format!("{{\"path\":\"{}\"}}", escape_json(path)))
}

/// Stream a process's stdout/stderr and exit as newline-delimited JSON events
/// over a chunked HTTP response.
async fn exec_stream<R>(
    state: &ServerState<R>,
    id: &str,
    body: &[u8],
    writer: &mut OwnedWriteHalf,
) -> std::io::Result<()>
where
    R: SandboxRuntimeSdk,
{
    let prepared = (|| {
        let id = SandboxId::new(id)?;
        let json = parse_body(body)?;
        let command = command_from_json(&json)?;
        Ok::<_, Error>((id, command))
    })();
    let (id, command) = match prepared {
        Ok(prepared) => prepared,
        Err(err) => return respond(writer, Err(err)).await,
    };

    let mut process = match state.runtime.exec(&id, command).await {
        Ok(process) => process,
        Err(err) => return respond(writer, Err(err)).await,
    };

    emit_event(
        state,
        &format!(
            "{{\"type\":\"process\",\"event\":\"started\",\"sandbox\":\"{}\",\"process_id\":\"{}\"}}",
            escape_json(id.as_str()),
            escape_json(&process.id)
        ),
    );

    write_stream_headers(writer).await?;
    let started = format!(
        "{{\"type\":\"started\",\"process_id\":\"{}\"}}\n",
        escape_json(&process.id)
    );
    write_chunk(writer, started.as_bytes()).await?;

    let mut stdout_done = false;
    let mut stderr_done = false;
    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            line = process.stdout.recv(), if !stdout_done => match line {
                Some(line) => {
                    let event = format!("{{\"type\":\"stdout\",\"data\":\"{}\"}}\n", escape_json(&line));
                    write_chunk(writer, event.as_bytes()).await?;
                }
                None => stdout_done = true,
            },
            line = process.stderr.recv(), if !stderr_done => match line {
                Some(line) => {
                    let event = format!("{{\"type\":\"stderr\",\"data\":\"{}\"}}\n", escape_json(&line));
                    write_chunk(writer, event.as_bytes()).await?;
                }
                None => stderr_done = true,
            },
        }
    }

    let exit_event = match process.exit.await {
        Ok(Ok(status)) => {
            let code = status
                .code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "null".to_string());
            emit_event(
                state,
                &format!(
                    "{{\"type\":\"process\",\"event\":\"exited\",\"sandbox\":\"{}\",\"process_id\":\"{}\",\"success\":{}}}",
                    escape_json(id.as_str()),
                    escape_json(&process.id),
                    status.success
                ),
            );
            format!(
                "{{\"type\":\"exit\",\"code\":{},\"success\":{}}}\n",
                code, status.success
            )
        }
        Ok(Err(err)) => format!(
            "{{\"type\":\"error\",\"message\":\"{}\"}}\n",
            escape_json(&err.to_string())
        ),
        Err(_) => "{\"type\":\"error\",\"message\":\"process exit channel closed\"}\n".to_string(),
    };
    write_chunk(writer, exit_event.as_bytes()).await?;
    write_last_chunk(writer).await
}

async fn stream_events<R>(
    state: &ServerState<R>,
    writer: &mut OwnedWriteHalf,
) -> std::io::Result<()> {
    let mut rx = state.events.subscribe();
    write_stream_headers(writer).await?;
    loop {
        match rx.recv().await {
            Ok(event) => {
                let line = format!("{event}\n");
                if write_chunk(writer, line.as_bytes()).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    write_last_chunk(writer).await
}

// ---------------------------------------------------------------------------
// Response writers
// ---------------------------------------------------------------------------

async fn write_json(
    writer: &mut OwnedWriteHalf,
    code: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        len = body.len(),
    );
    writer.write_all(response.as_bytes()).await?;
    writer.flush().await
}

async fn write_stream_headers(writer: &mut OwnedWriteHalf) -> std::io::Result<()> {
    let headers = "HTTP/1.1 200 OK\r\n\
         Content-Type: application/x-ndjson\r\n\
         Transfer-Encoding: chunked\r\n\
         Connection: close\r\n\r\n";
    writer.write_all(headers.as_bytes()).await?;
    writer.flush().await
}

async fn write_chunk(writer: &mut OwnedWriteHalf, data: &[u8]) -> std::io::Result<()> {
    let header = format!("{:x}\r\n", data.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(data).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await
}

async fn write_last_chunk(writer: &mut OwnedWriteHalf) -> std::io::Result<()> {
    writer.write_all(b"0\r\n\r\n").await?;
    writer.flush().await
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

fn sandbox_json(
    id: &str,
    state_dir: &str,
    running: bool,
    mounts: Option<&[WorkspaceMountInfo]>,
) -> String {
    let state = if running { "running" } else { "stopped" };
    let mut json = format!(
        "{{\"id\":\"{}\",\"state\":\"{}\",\"state_dir\":\"{}\"",
        escape_json(id),
        state,
        escape_json(state_dir)
    );
    if let Some(mounts) = mounts {
        let entries: Vec<String> = mounts
            .iter()
            .map(|mount| {
                format!(
                    "{{\"tag\":\"{}\",\"host_path\":\"{}\",\"guest_path\":\"{}\",\"read_only\":{}}}",
                    escape_json(&mount.tag),
                    escape_json(&mount.host_path.display().to_string()),
                    escape_json(&mount.guest_path),
                    mount.read_only
                )
            })
            .collect();
        json.push_str(&format!(",\"mounts\":[{}]", entries.join(",")));
    }
    json.push('}');
    json
}

fn lifecycle_event(event: &str, id: &str, running: bool) -> String {
    let state = if running { "running" } else { "stopped" };
    format!(
        "{{\"type\":\"lifecycle\",\"event\":\"{}\",\"sandbox\":\"{}\",\"state\":\"{}\"}}",
        escape_json(event),
        escape_json(id),
        state
    )
}

fn error_body(category: &str, message: &str) -> String {
    format!(
        "{{\"error\":{{\"category\":\"{}\",\"message\":\"{}\"}}}}",
        escape_json(category),
        escape_json(message)
    )
}

/// Map an [`Error`] to (HTTP status, reason phrase, contract error category).
fn error_status(err: &Error) -> (u16, &'static str, &'static str) {
    match err {
        Error::InvalidArgument(_) | Error::InvalidImage(_) | Error::InvalidCredential(_) => {
            (400, "Bad Request", "invalid_argument")
        }
        Error::NotFound(_) => (404, "Not Found", "not_found"),
        Error::ResourceExhausted(_) => (429, "Too Many Requests", "resource_exhausted"),
        Error::ProcessFailed { .. } => (409, "Conflict", "failed_precondition"),
        Error::Io { .. } => (500, "Internal Server Error", "io"),
        Error::Libkrun { .. } | Error::Internal(_) => (500, "Internal Server Error", "internal"),
    }
}

fn parse_body(body: &[u8]) -> Result<Json> {
    if body.is_empty() {
        return Ok(Json::Obj(Vec::new()));
    }
    let text = std::str::from_utf8(body)
        .map_err(|_| Error::InvalidArgument("request body is not valid UTF-8".to_string()))?;
    Json::parse(text)
        .ok_or_else(|| Error::InvalidArgument("request body is not valid JSON".to_string()))
}

fn config_from_json(json: &Json) -> Result<SandboxConfig> {
    let image = json
        .get("image")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::InvalidArgument("create requires an image path".to_string()))?;
    let mut config = SandboxConfig::new(AgentImageRef::new(image));
    if let Some(name) = json.get("name").and_then(Json::as_str) {
        config.name = Some(name.to_string());
    }
    if let Some(cpus) = json.get("cpus").and_then(Json::as_u64) {
        config.cpus = cpus.min(u8::MAX as u64) as u8;
    }
    if let Some(mib) = json.get("memory_mib").and_then(Json::as_u64) {
        config.memory_mib = mib as u32;
    }
    if let Some(mib) = json.get("memory_max_mib").and_then(Json::as_u64) {
        config.memory_max_mib = mib as u32;
    }
    if let Some(mib) = json.get("memory_min_mib").and_then(Json::as_u64) {
        config.memory_min_mib = mib as u32;
    }
    if let Some(backend) = json.get("network_backend").and_then(Json::as_str) {
        config.network_backend = network_backend_from_str(backend)?;
    }
    if let Some(Json::Arr(mounts)) = json.get("mounts") {
        for mount in mounts {
            let tag = mount
                .get("tag")
                .and_then(Json::as_str)
                .ok_or_else(|| Error::InvalidArgument("mount requires a tag".to_string()))?;
            let host_path = mount
                .get("host_path")
                .and_then(Json::as_str)
                .ok_or_else(|| Error::InvalidArgument("mount requires a host_path".to_string()))?;
            let guest_path = mount
                .get("guest_path")
                .and_then(Json::as_str)
                .ok_or_else(|| Error::InvalidArgument("mount requires a guest_path".to_string()))?;
            let read_only = mount
                .get("read_only")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            config.workspace_mounts.push(crate::config::WorkspaceMount {
                tag: tag.to_string(),
                host_path: host_path.into(),
                guest_path: guest_path.to_string(),
                read_only,
            });
        }
    }
    Ok(config)
}

fn network_backend_from_str(value: &str) -> Result<NetworkBackend> {
    match value {
        "none" | "" => Ok(NetworkBackend::None),
        "gvproxy-auto" => Ok(NetworkBackend::GvproxyAuto { executable: None }),
        other => Err(Error::InvalidArgument(format!(
            "network_backend '{other}' is not settable over the API yet; use 'none' or 'gvproxy-auto'"
        ))),
    }
}

fn command_from_json(json: &Json) -> Result<GuestCommand> {
    let argv: Vec<String> = match json.get("argv") {
        Some(Json::Arr(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    Error::InvalidArgument("argv entries must be strings".to_string())
                })
            })
            .collect::<Result<_>>()?,
        _ => {
            return Err(Error::InvalidArgument(
                "exec requires an argv array".to_string(),
            ))
        }
    };
    if argv.is_empty() {
        return Err(Error::InvalidArgument("argv must not be empty".to_string()));
    }
    let mut command = GuestCommand::new(argv);
    if let Some(cwd) = json.get("cwd").and_then(Json::as_str) {
        command = command.cwd(cwd);
    }
    if let Some(Json::Obj(env)) = json.get("env") {
        for (key, value) in env {
            if let Some(value) = value.as_str() {
                command.env.push((key.clone(), value.to_string()));
            }
        }
    }
    Ok(command)
}

// ---------------------------------------------------------------------------
// Minimal JSON value parser (inbound bodies only)
// ---------------------------------------------------------------------------

/// A parsed JSON value. Deliberately tiny — only what request bodies need.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub(crate) fn parse(input: &str) -> Option<Json> {
        let bytes = input.as_bytes();
        let mut pos = 0;
        let value = parse_value(bytes, &mut pos)?;
        skip_ws(bytes, &mut pos);
        if pos == bytes.len() {
            Some(value)
        } else {
            None
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(value) if *value >= 0.0 && value.fract() == 0.0 => Some(*value as u64),
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }
}

fn skip_ws(bytes: &[u8], pos: &mut usize) {
    while *pos < bytes.len() && matches!(bytes[*pos], b' ' | b'\t' | b'\n' | b'\r') {
        *pos += 1;
    }
}

fn parse_value(bytes: &[u8], pos: &mut usize) -> Option<Json> {
    skip_ws(bytes, pos);
    match bytes.get(*pos)? {
        b'{' => parse_object(bytes, pos),
        b'[' => parse_array(bytes, pos),
        b'"' => parse_string(bytes, pos).map(Json::Str),
        b't' => parse_literal(bytes, pos, "true", Json::Bool(true)),
        b'f' => parse_literal(bytes, pos, "false", Json::Bool(false)),
        b'n' => parse_literal(bytes, pos, "null", Json::Null),
        _ => parse_number(bytes, pos),
    }
}

fn parse_literal(bytes: &[u8], pos: &mut usize, literal: &str, value: Json) -> Option<Json> {
    if bytes[*pos..].starts_with(literal.as_bytes()) {
        *pos += literal.len();
        Some(value)
    } else {
        None
    }
}

fn parse_number(bytes: &[u8], pos: &mut usize) -> Option<Json> {
    let start = *pos;
    while *pos < bytes.len()
        && matches!(bytes[*pos], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
    {
        *pos += 1;
    }
    let text = std::str::from_utf8(&bytes[start..*pos]).ok()?;
    text.parse::<f64>().ok().map(Json::Num)
}

fn parse_string(bytes: &[u8], pos: &mut usize) -> Option<String> {
    if bytes.get(*pos)? != &b'"' {
        return None;
    }
    *pos += 1;
    let mut out = String::new();
    while *pos < bytes.len() {
        let ch = bytes[*pos];
        *pos += 1;
        match ch {
            b'"' => return Some(out),
            b'\\' => {
                let esc = bytes.get(*pos)?;
                *pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'u' => {
                        let hex = std::str::from_utf8(bytes.get(*pos..*pos + 4)?).ok()?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        *pos += 4;
                        out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                    }
                    _ => return None,
                }
            }
            other => {
                // Push the raw UTF-8 byte sequence. Multi-byte chars arrive as
                // their constituent bytes; collect them directly.
                out.push(other as char);
            }
        }
    }
    None
}

fn parse_array(bytes: &[u8], pos: &mut usize) -> Option<Json> {
    *pos += 1; // consume '['
    let mut items = Vec::new();
    skip_ws(bytes, pos);
    if bytes.get(*pos) == Some(&b']') {
        *pos += 1;
        return Some(Json::Arr(items));
    }
    loop {
        let value = parse_value(bytes, pos)?;
        items.push(value);
        skip_ws(bytes, pos);
        match bytes.get(*pos)? {
            b',' => {
                *pos += 1;
            }
            b']' => {
                *pos += 1;
                return Some(Json::Arr(items));
            }
            _ => return None,
        }
    }
}

fn parse_object(bytes: &[u8], pos: &mut usize) -> Option<Json> {
    *pos += 1; // consume '{'
    let mut entries = Vec::new();
    skip_ws(bytes, pos);
    if bytes.get(*pos) == Some(&b'}') {
        *pos += 1;
        return Some(Json::Obj(entries));
    }
    loop {
        skip_ws(bytes, pos);
        let key = parse_string(bytes, pos)?;
        skip_ws(bytes, pos);
        if bytes.get(*pos)? != &b':' {
            return None;
        }
        *pos += 1;
        let value = parse_value(bytes, pos)?;
        entries.push((key, value));
        skip_ws(bytes, pos);
        match bytes.get(*pos)? {
            b',' => {
                *pos += 1;
            }
            b'}' => {
                *pos += 1;
                return Some(Json::Obj(entries));
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_parses_nested_objects_and_arrays() {
        let json = Json::parse(
            r#"{"image":"/img","cpus":2,"ok":true,"mounts":[{"tag":"workspace","read_only":false}]}"#,
        )
        .unwrap();
        assert_eq!(json.get("image").and_then(Json::as_str), Some("/img"));
        assert_eq!(json.get("cpus").and_then(Json::as_u64), Some(2));
        assert_eq!(json.get("ok").and_then(Json::as_bool), Some(true));
        let mounts = match json.get("mounts") {
            Some(Json::Arr(items)) => items,
            _ => panic!("mounts should be an array"),
        };
        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].get("tag").and_then(Json::as_str),
            Some("workspace")
        );
        assert_eq!(
            mounts[0].get("read_only").and_then(Json::as_bool),
            Some(false)
        );
    }

    #[test]
    fn json_rejects_trailing_garbage() {
        assert!(Json::parse(r#"{"a":1} oops"#).is_none());
        assert!(Json::parse("not json").is_none());
    }

    #[test]
    fn json_parses_escapes() {
        let json = Json::parse(r#"{"s":"a\nb\t\"c\""}"#).unwrap();
        assert_eq!(json.get("s").and_then(Json::as_str), Some("a\nb\t\"c\""));
    }

    #[test]
    fn config_from_json_builds_mounts() {
        let json = Json::parse(
            r#"{"image":"/img","name":"x","memory_mib":1024,"mounts":[{"tag":"w","host_path":"/h","guest_path":"/g","read_only":true}]}"#,
        )
        .unwrap();
        let config = config_from_json(&json).unwrap();
        assert_eq!(config.name.as_deref(), Some("x"));
        assert_eq!(config.memory_mib, 1024);
        assert_eq!(config.workspace_mounts.len(), 1);
        assert_eq!(config.workspace_mounts[0].guest_path, "/g");
        assert!(config.workspace_mounts[0].read_only);
    }

    #[test]
    fn percent_decode_handles_paths() {
        assert_eq!(percent_decode("%2Ftmp%2Ffile"), "/tmp/file");
        assert_eq!(percent_decode("a+b"), "a b");
    }

    // -- end-to-end HTTP conformance ----------------------------------------

    use crate::client::{ApiClient, ExecEvent};
    use crate::sdk::LocalProcessSandboxRuntime;

    fn unique_root(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tb-api-{label}-{}-{:p}",
            std::process::id(),
            &label as *const _
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Bind a server backed by `LocalProcessSandboxRuntime` on an ephemeral port
    /// and return a token-authenticated client plus the base URL.
    async fn start_server(root: &std::path::Path, token: &str) -> (ApiClient, String) {
        let runtime = LocalProcessSandboxRuntime::new(root);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = ApiServer::new(Arc::new(runtime), Auth::Bearer(token.to_string()));
        tokio::spawn(async move {
            let _ = server.serve(listener).await;
        });
        let url = format!("http://{addr}");
        let client = ApiClient::new(&url, Some(token.to_string())).unwrap();
        (client, url)
    }

    fn exec_body(argv: &[&str], cwd: Option<&str>) -> String {
        let argv_json: Vec<String> = argv
            .iter()
            .map(|arg| format!("\"{}\"", escape_json(arg)))
            .collect();
        let mut body = format!("{{\"argv\":[{}]", argv_json.join(","));
        if let Some(cwd) = cwd {
            body.push_str(&format!(",\"cwd\":\"{}\"", escape_json(cwd)));
        }
        body.push('}');
        body
    }

    #[tokio::test]
    async fn api_contract_conformance_over_http() {
        let root = unique_root("conformance");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let (client, url) = start_server(&root, "secret-token").await;

        // Health is unauthenticated.
        let no_auth = ApiClient::new(&url, None).unwrap();
        let health = no_auth.get("/health").await.unwrap();
        assert_eq!(health.get("status").and_then(Json::as_str), Some("ok"));

        // A bad token is rejected.
        let bad = ApiClient::new(&url, Some("wrong".to_string())).unwrap();
        assert!(bad.get("/v1/sandboxes").await.is_err());

        // create
        let create_body = format!(
            "{{\"image\":\"{img}\",\"name\":\"contract-http\",\"mounts\":[{{\"tag\":\"workspace\",\"host_path\":\"{ws}\",\"guest_path\":\"/workspace\",\"read_only\":false}}]}}",
            img = escape_json(&root.join("image").display().to_string()),
            ws = escape_json(&workspace.display().to_string()),
        );
        let created = client.post("/v1/sandboxes", &create_body).await.unwrap();
        assert_eq!(
            created.get("id").and_then(Json::as_str),
            Some("contract-http")
        );
        assert_eq!(created.get("state").and_then(Json::as_str), Some("running"));

        // start (idempotent for this runtime) + inspect mounts/state
        client
            .post("/v1/sandboxes/contract-http/start", "{}")
            .await
            .unwrap();
        let inspected = client.get("/v1/sandboxes/contract-http").await.unwrap();
        assert_eq!(
            inspected.get("state").and_then(Json::as_str),
            Some("running")
        );
        let mounts = match inspected.get("mounts") {
            Some(Json::Arr(items)) => items,
            _ => panic!("inspect should return mounts"),
        };
        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].get("guest_path").and_then(Json::as_str),
            Some("/workspace")
        );

        // list
        let listed = client.get("/v1/sandboxes").await.unwrap();
        let entries = match listed.get("sandboxes") {
            Some(Json::Arr(items)) => items,
            _ => panic!("list should return sandboxes"),
        };
        assert!(entries
            .iter()
            .any(|e| e.get("id").and_then(Json::as_str) == Some("contract-http")));

        // exec: stream stdout/stderr + write a file + report exit
        let cwd = workspace.display().to_string();
        let body = exec_body(
            &[
                "/bin/sh",
                "-c",
                "printf 'contract-out\\n'; printf 'contract-err\\n' >&2; \
                 printf 'file-ok\\n' > contract.txt; cat contract.txt",
            ],
            Some(&cwd),
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let (code, success) = client
            .exec_stream("contract-http", &body, |event| match event {
                ExecEvent::Stdout(line) => stdout.push(line.clone()),
                ExecEvent::Stderr(line) => stderr.push(line.clone()),
                _ => {}
            })
            .await
            .unwrap();
        assert!(success);
        assert_eq!(code, Some(0));
        assert_eq!(stdout, vec!["contract-out", "file-ok"]);
        assert_eq!(stderr, vec!["contract-err"]);
        assert_eq!(
            std::fs::read_to_string(workspace.join("contract.txt")).unwrap(),
            "file-ok\n"
        );

        // filesystem read/write through the API (text-oriented). The runtime
        // execs without an implicit cwd, so use an absolute path.
        let abs = workspace.join("fs-roundtrip.txt").display().to_string();
        let encoded = abs.replace('/', "%2F");
        client
            .request(
                "PUT",
                &format!("/v1/sandboxes/contract-http/fs?path={encoded}"),
                Some("hello-from-api"),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("fs-roundtrip.txt")).unwrap(),
            "hello-from-api"
        );
        let read = client
            .get(&format!("/v1/sandboxes/contract-http/fs?path={encoded}"))
            .await
            .unwrap();
        assert_eq!(
            read.get("content").and_then(Json::as_str),
            Some("hello-from-api")
        );

        // cancellation kills a running process tree.
        let (pid_tx, mut pid_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_client = client.clone();
        let sleep_body = exec_body(&["/bin/sh", "-c", "sleep 5"], None);
        let exec_task = tokio::spawn(async move {
            cancel_client
                .exec_stream("contract-http", &sleep_body, move |event| {
                    if let ExecEvent::Started { process_id } = event {
                        let _ = pid_tx.send(process_id.clone());
                    }
                })
                .await
        });
        let process_id = pid_rx.recv().await.expect("started event with process id");
        client
            .post(
                "/v1/sandboxes/contract-http/cancel",
                &format!("{{\"process_id\":\"{}\"}}", escape_json(&process_id)),
            )
            .await
            .unwrap();
        let (_code, cancelled_success) = exec_task.await.unwrap().unwrap();
        assert!(!cancelled_success);

        // stop + delete, then the sandbox is gone.
        client
            .post("/v1/sandboxes/contract-http/stop", "{}")
            .await
            .unwrap();
        client.delete("/v1/sandboxes/contract-http").await.unwrap();
        let missing = client.get("/v1/sandboxes/contract-http").await;
        assert!(matches!(missing, Err(crate::error::Error::NotFound(_))));

        let _ = std::fs::remove_dir_all(&root);
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("run git");
        assert!(status.status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn git_status_and_diff_over_http() {
        let root = unique_root("git");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        run_git(&repo, &["add", "a.txt"]);
        run_git(&repo, &["commit", "-q", "-m", "init"]);

        let (client, _url) = start_server(&root, "tok").await;
        let create = format!(
            "{{\"image\":\"{}\",\"name\":\"gitbox\"}}",
            escape_json(&root.join("image").display().to_string())
        );
        client.post("/v1/sandboxes", &create).await.unwrap();

        let repo_q = repo.display().to_string().replace('/', "%2F");

        // Clean tree: no entries.
        let status = client
            .get(&format!("/v1/sandboxes/gitbox/git/status?path={repo_q}"))
            .await
            .unwrap();
        assert_eq!(status.get("clean").and_then(Json::as_bool), Some(true));
        assert!(!status
            .get("branch")
            .and_then(Json::as_str)
            .unwrap_or("")
            .is_empty());

        // Dirty tree: a modified file shows up as an entry, and diff is non-empty.
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        let status = client
            .get(&format!("/v1/sandboxes/gitbox/git/status?path={repo_q}"))
            .await
            .unwrap();
        assert_eq!(status.get("clean").and_then(Json::as_bool), Some(false));
        let entries = match status.get("entries") {
            Some(Json::Arr(items)) => items,
            _ => panic!("status should have entries"),
        };
        assert!(entries
            .iter()
            .any(|e| e.get("path").and_then(Json::as_str) == Some("a.txt")));

        let diff = client
            .get(&format!("/v1/sandboxes/gitbox/git/diff?path={repo_q}"))
            .await
            .unwrap();
        assert!(diff
            .get("diff")
            .and_then(Json::as_str)
            .unwrap_or("")
            .contains("+two"));

        // A non-repo path is rejected.
        let bad = root
            .join("notrepo")
            .display()
            .to_string()
            .replace('/', "%2F");
        std::fs::create_dir_all(root.join("notrepo")).unwrap();
        assert!(client
            .get(&format!("/v1/sandboxes/gitbox/git/status?path={bad}"))
            .await
            .is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
