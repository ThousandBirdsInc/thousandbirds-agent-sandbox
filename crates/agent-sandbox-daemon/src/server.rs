//! Connection handling and message dispatch for the guest daemon.
//!
//! One daemon connection multiplexes many operations keyed by `id`. A single
//! writer task drains an outbox channel so concurrent processes interleave
//! their frames safely; the read loop dispatches requests and routes control
//! messages (stdin/resize/cancel) to the owning process task.

use std::collections::HashMap;
use std::sync::Arc;

use agent_sandbox_protocol::{
    read_message, write_message, Message, T_CANCEL, T_EXEC, T_FS_DELETE, T_FS_LIST, T_FS_MKDIR,
    T_FS_READ, T_FS_STAT, T_FS_WRITE, T_GIT, T_HEALTH, T_PONG, T_RESIZE, T_STDIN, T_STDIN_CLOSE,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Mutex};

use crate::proc::{self, ExecSpec, ProcControl};

/// Daemon version reported over the health endpoint.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const OUTBOX_CAPACITY: usize = 256;
const CONTROL_CAPACITY: usize = 64;

type Registry = Arc<Mutex<HashMap<u64, mpsc::Sender<ProcControl>>>>;

/// Serve a single bidirectional connection until EOF or a transport error.
pub async fn handle_connection<S>(stream: S)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (outbox, mut outbox_rx) = mpsc::channel::<Message>(OUTBOX_CAPACITY);
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = outbox_rx.recv().await {
            if write_message(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let msg = match read_message(&mut reader).await {
            Ok(Some(msg)) => msg,
            Ok(None) | Err(_) => break,
        };
        dispatch(msg, &outbox, &registry).await;
    }

    // Connection gone: cancel everything still running, then let the writer
    // drain and exit once all outbox senders drop.
    for (_, control) in registry.lock().await.drain() {
        let _ = control.try_send(ProcControl::Cancel);
    }
    drop(outbox);
    let _ = writer_task.await;
}

async fn dispatch(msg: Message, outbox: &mpsc::Sender<Message>, registry: &Registry) {
    let id = msg.id();
    match msg.t() {
        T_HEALTH => {
            let _ = outbox
                .send(Message::new(T_PONG, id).with_str("version", VERSION))
                .await;
        }
        T_EXEC => start_exec(msg, outbox.clone(), registry.clone()).await,
        T_STDIN => {
            if let Some(bytes) = msg.bytes_field("data") {
                forward(registry, id, ProcControl::Stdin(bytes)).await;
            }
        }
        T_STDIN_CLOSE => forward(registry, id, ProcControl::StdinClose).await,
        T_RESIZE => {
            let rows = msg.u64_field("rows").unwrap_or(0) as u16;
            let cols = msg.u64_field("cols").unwrap_or(0) as u16;
            forward(registry, id, ProcControl::Resize { rows, cols }).await;
        }
        T_CANCEL => forward(registry, id, ProcControl::Cancel).await,
        T_FS_READ | T_FS_WRITE | T_FS_STAT | T_FS_LIST | T_FS_MKDIR | T_FS_DELETE => {
            let outbox = outbox.clone();
            tokio::spawn(async move {
                if let Ok(reply) =
                    tokio::task::spawn_blocking(move || crate::fsops::handle(&msg)).await
                {
                    let _ = outbox.send(reply).await;
                }
            });
        }
        T_GIT => {
            let outbox = outbox.clone();
            tokio::spawn(async move {
                let reply = crate::gitops::handle(&msg).await;
                let _ = outbox.send(reply).await;
            });
        }
        other => {
            let _ = outbox
                .send(
                    Message::new(agent_sandbox_protocol::T_ERROR, id)
                        .with_str("category", agent_sandbox_protocol::CAT_INVALID_ARGUMENT)
                        .with_str("message", &format!("unknown request type {other}")),
                )
                .await;
        }
    }
}

async fn start_exec(msg: Message, outbox: mpsc::Sender<Message>, registry: Registry) {
    let id = msg.id();
    let spec = ExecSpec {
        argv: msg
            .value_field("argv")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        env: msg
            .value_field("env")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|pair| {
                        let pair = pair.as_array()?;
                        Some((pair.first()?.as_str()?.to_string(), pair.get(1)?.as_str()?.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        cwd: msg.str_field("cwd").map(String::from),
        pty: msg.bool_field("pty"),
        rows: msg.u64_field("rows").unwrap_or(0) as u16,
        cols: msg.u64_field("cols").unwrap_or(0) as u16,
    };

    let (control_tx, control_rx) = mpsc::channel(CONTROL_CAPACITY);
    registry.lock().await.insert(id, control_tx);

    tokio::spawn(async move {
        if spec.pty {
            proc::run_pty(id, spec, outbox, control_rx).await;
        } else {
            proc::run_plain(id, spec, outbox, control_rx).await;
        }
        registry.lock().await.remove(&id);
    });
}

async fn forward(registry: &Registry, id: u64, control: ProcControl) {
    let sender = registry.lock().await.get(&id).cloned();
    if let Some(sender) = sender {
        let _ = sender.send(control).await;
    }
}

/// Accept loop over a Unix socket. Used in tests and for local debugging; the
/// guest uses [`crate::serve_vsock`].
pub async fn serve_unix(listener: tokio::net::UnixListener) -> std::io::Result<()> {
    loop {
        let (stream, _addr) = listener.accept().await?;
        tokio::spawn(handle_connection(stream));
    }
}
