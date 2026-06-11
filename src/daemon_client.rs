//! Host-side client for the in-guest `agent-sandbox-daemon`.
//!
//! The daemon listens on an `AF_VSOCK` port inside the guest; libkrun bridges
//! that port to a per-sandbox `control.vsock` `AF_UNIX` socket on the host
//! (`krun_add_vsock_port2(.., listen=true)`). This client connects to that Unix
//! socket and speaks the multiplexed protocol from
//! [`agent_sandbox_protocol`], demultiplexing replies and process output by
//! correlation id.
//!
//! It produces the same [`GuestProcess`] shape the file-relay transport does —
//! byte chunks from the daemon are reassembled into lines here — so the SDK and
//! its callers are unchanged regardless of which transport served the exec.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agent_sandbox_protocol::{
    read_message, write_message, Message, CAT_FAILED_PRECONDITION, CAT_INVALID_ARGUMENT,
    CAT_IO, CAT_NOT_FOUND, T_CANCEL, T_ERROR, T_EXEC, T_EXIT, T_FS_READ, T_FS_WRITE, T_GIT,
    T_HEALTH, T_RESIZE, T_STDERR, T_STDIN, T_STDIN_CLOSE, T_STDOUT,
};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::error::{Error, Result};
use crate::sdk::{GuestCommand, GuestExitStatus, GuestProcess};

const STREAM_CHANNEL_CAPACITY: usize = 64;
const OUTBOX_CAPACITY: usize = 256;

/// A live connection to a guest daemon. Cheap to clone — clones share the
/// underlying connection.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    outbox: mpsc::Sender<Message>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    next_id: Arc<AtomicU64>,
}

#[derive(Debug)]
enum Pending {
    /// A one-shot request/response (health, fs, git).
    Reply(oneshot::Sender<Message>),
    /// A running process: routes stdout/stderr/exit until it exits.
    Exec(ExecRoute),
}

#[derive(Debug)]
struct ExecRoute {
    stdout: LineSink,
    stderr: LineSink,
    exit: Option<oneshot::Sender<Result<GuestExitStatus>>>,
}

/// Buffers byte chunks and emits complete lines (without the trailing newline)
/// to match the line-oriented [`GuestProcess`] channels.
#[derive(Debug)]
struct LineSink {
    tx: mpsc::Sender<String>,
    buf: Vec<u8>,
}

impl LineSink {
    fn new(tx: mpsc::Sender<String>) -> Self {
        Self { tx, buf: Vec::new() }
    }

    async fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if self.tx.send(text).await.is_err() {
                self.buf.clear();
                return;
            }
        }
    }

    async fn flush(&mut self) {
        if !self.buf.is_empty() {
            let text = String::from_utf8_lossy(&self.buf).into_owned();
            let _ = self.tx.send(text).await;
            self.buf.clear();
        }
    }
}

impl DaemonClient {
    /// Connect to a guest daemon over its bridged `control.vsock` Unix socket.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|err| Error::io_path(socket_path, err))?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (outbox, mut outbox_rx) = mpsc::channel::<Message>(OUTBOX_CAPACITY);
        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));

        // Writer task: serialize all outgoing frames.
        tokio::spawn(async move {
            while let Some(msg) = outbox_rx.recv().await {
                if write_message(&mut writer, &msg).await.is_err() {
                    break;
                }
            }
        });

        // Reader task: demultiplex by id.
        let pending_reader = pending.clone();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = read_message(&mut reader).await {
                route_incoming(&pending_reader, msg).await;
            }
            // Connection dropped: fail any in-flight processes/requests.
            let mut guard = pending_reader.lock().await;
            for (_, p) in guard.drain() {
                if let Pending::Exec(mut route) = p {
                    if let Some(tx) = route.exit.take() {
                        let _ = tx.send(Err(Error::Internal(
                            "guest daemon connection closed".to_string(),
                        )));
                    }
                }
            }
        });

        Ok(Self {
            outbox,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Health/version handshake. Used to decide whether the daemon transport is
    /// available before committing an exec to it.
    pub async fn health(&self) -> Result<String> {
        let reply = self.request(Message::new(T_HEALTH, self.alloc_id())).await?;
        Ok(reply.str_field("version").unwrap_or_default().to_string())
    }

    /// Start a process. Returns the daemon's numeric id (for cancel/stdin/
    /// resize) and the streaming [`GuestProcess`].
    pub async fn exec(&self, command: &GuestCommand, pty: PtyOptions) -> Result<(u64, GuestProcess)> {
        let id = self.alloc_id();
        let (stdout_tx, stdout_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let (stderr_tx, stderr_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let (exit_tx, exit_rx) = oneshot::channel();
        self.pending.lock().await.insert(
            id,
            Pending::Exec(ExecRoute {
                stdout: LineSink::new(stdout_tx),
                stderr: LineSink::new(stderr_tx),
                exit: Some(exit_tx),
            }),
        );

        let argv = serde_json::Value::Array(
            command.argv.iter().map(|a| serde_json::Value::String(a.clone())).collect(),
        );
        let env = serde_json::Value::Array(
            command
                .env
                .iter()
                .map(|(k, v)| serde_json::json!([k, v]))
                .collect(),
        );
        let mut msg = Message::new(T_EXEC, id)
            .with_value("argv", argv)
            .with_value("env", env)
            .with_opt_str("cwd", command.cwd.as_deref());
        if pty.enabled {
            msg = msg
                .with_bool("pty", true)
                .with_u64("rows", pty.rows as u64)
                .with_u64("cols", pty.cols as u64);
        }
        self.send(msg).await?;

        Ok((
            id,
            GuestProcess {
                id: id.to_string(),
                stdout: stdout_rx,
                stderr: stderr_rx,
                exit: exit_rx,
            },
        ))
    }

    /// Cancel a running process by daemon id (kills its process group).
    pub async fn cancel(&self, id: u64) -> Result<()> {
        self.send(Message::new(T_CANCEL, id)).await
    }

    /// Write bytes to a process's stdin (or PTY).
    pub async fn stdin(&self, id: u64, data: &[u8]) -> Result<()> {
        self.send(Message::new(T_STDIN, id).with_bytes("data", data)).await
    }

    /// Signal EOF on a process's stdin.
    pub async fn stdin_close(&self, id: u64) -> Result<()> {
        self.send(Message::new(T_STDIN_CLOSE, id)).await
    }

    /// Resize a PTY session.
    pub async fn resize(&self, id: u64, rows: u16, cols: u16) -> Result<()> {
        self.send(
            Message::new(T_RESIZE, id)
                .with_u64("rows", rows as u64)
                .with_u64("cols", cols as u64),
        )
        .await
    }

    /// Read a file's contents from the guest.
    pub async fn fs_read(&self, path: &str) -> Result<Vec<u8>> {
        let reply = self
            .request(Message::new(T_FS_READ, self.alloc_id()).with_str("path", path))
            .await?;
        Ok(reply.bytes_field("data").unwrap_or_default())
    }

    /// Write a file in the guest (`mode` = overwrite|append|create).
    pub async fn fs_write(&self, path: &str, data: &[u8], mode: &str) -> Result<u64> {
        let reply = self
            .request(
                Message::new(T_FS_WRITE, self.alloc_id())
                    .with_str("path", path)
                    .with_str("mode", mode)
                    .with_bytes("data", data),
            )
            .await?;
        Ok(reply.u64_field("bytes").unwrap_or(0))
    }

    /// Run `git -C path <argv...>` in the guest; returns (exit_code, stdout,
    /// stderr).
    pub async fn git(&self, path: &str, argv: &[String]) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        let argv = serde_json::Value::Array(
            argv.iter().map(|a| serde_json::Value::String(a.clone())).collect(),
        );
        let reply = self
            .request(
                Message::new(T_GIT, self.alloc_id())
                    .with_str("path", path)
                    .with_value("argv", argv),
            )
            .await?;
        Ok((
            reply.i64_field("code").unwrap_or(-1) as i32,
            reply.bytes_field("stdout").unwrap_or_default(),
            reply.bytes_field("stderr").unwrap_or_default(),
        ))
    }

    async fn request(&self, msg: Message) -> Result<Message> {
        let id = msg.id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, Pending::Reply(tx));
        self.send(msg).await?;
        let reply = rx
            .await
            .map_err(|_| Error::Internal("guest daemon closed before replying".to_string()))?;
        if reply.t() == T_ERROR {
            return Err(category_to_error(
                reply.str_field("category").unwrap_or(CAT_IO),
                reply.str_field("message").unwrap_or("daemon error"),
            ));
        }
        Ok(reply)
    }

    async fn send(&self, msg: Message) -> Result<()> {
        self.outbox
            .send(msg)
            .await
            .map_err(|_| Error::Internal("guest daemon connection is closed".to_string()))
    }
}

/// PTY options for [`DaemonClient::exec`].
#[derive(Debug, Clone, Copy)]
pub struct PtyOptions {
    pub enabled: bool,
    pub rows: u16,
    pub cols: u16,
}

impl PtyOptions {
    pub fn off() -> Self {
        Self { enabled: false, rows: 0, cols: 0 }
    }

    pub fn on(rows: u16, cols: u16) -> Self {
        Self { enabled: true, rows, cols }
    }
}

async fn route_incoming(pending: &Arc<Mutex<HashMap<u64, Pending>>>, msg: Message) {
    let id = msg.id();
    let mut guard = pending.lock().await;
    let mut is_reply = false;
    let mut finished = false;
    match guard.get_mut(&id) {
        None => return, // stray/late frame for a completed operation
        Some(Pending::Reply(_)) => is_reply = true,
        Some(Pending::Exec(route)) => match msg.t() {
            T_STDOUT => {
                if let Some(data) = msg.bytes_field("data") {
                    route.stdout.push(&data).await;
                }
            }
            T_STDERR => {
                if let Some(data) = msg.bytes_field("data") {
                    route.stderr.push(&data).await;
                }
            }
            T_EXIT => {
                route.stdout.flush().await;
                route.stderr.flush().await;
                let status = if let Some(err) = msg.str_field("error") {
                    Err(Error::Internal(err.to_string()))
                } else {
                    Ok(GuestExitStatus {
                        code: msg.i64_field("code").map(|c| c as i32),
                        success: msg.bool_field("success"),
                    })
                };
                if let Some(tx) = route.exit.take() {
                    let _ = tx.send(status);
                }
                finished = true;
            }
            _ => {} // `started` and anything else: ignore
        },
    }
    // The mutable borrow above ends here; now it is safe to remove the entry.
    if is_reply {
        if let Some(Pending::Reply(tx)) = guard.remove(&id) {
            let _ = tx.send(msg);
        }
    } else if finished {
        guard.remove(&id);
    }
}

fn category_to_error(category: &str, message: &str) -> Error {
    match category {
        CAT_NOT_FOUND => Error::NotFound(message.to_string()),
        CAT_INVALID_ARGUMENT => Error::InvalidArgument(message.to_string()),
        CAT_FAILED_PRECONDITION => Error::InvalidArgument(message.to_string()),
        _ => Error::Internal(message.to_string()),
    }
}
