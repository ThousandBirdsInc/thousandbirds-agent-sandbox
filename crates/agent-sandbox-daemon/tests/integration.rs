//! End-to-end tests driving the real daemon server over an `AF_UNIX` socket.
//! This is the same `handle_connection` code path the guest runs over vsock, so
//! these exercise the actual exec/stream/cancel/pty/fs behavior without a VM.

use std::time::Duration;

use agent_sandbox_protocol::{
    read_message, write_message, Message, T_EXEC, T_EXIT, T_FS_READ, T_FS_WRITE, T_HEALTH, T_OK,
    T_PONG, T_RESIZE, T_STDERR, T_STDIN, T_STDOUT,
};
use serde_json::json;
use tokio::net::{UnixListener, UnixStream};

async fn start_server() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tb-daemon-test-{}-{}",
        std::process::id(),
        next_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("daemon.sock");
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let _ = agent_sandbox_daemon::serve_unix(listener).await;
    });
    path
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn exec_msg(id: u64, argv: &[&str]) -> Message {
    Message::new(T_EXEC, id).with_value(
        "argv",
        json!(argv.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
    )
}

#[tokio::test]
async fn health_returns_version() {
    let path = start_server().await;
    let mut conn = UnixStream::connect(&path).await.unwrap();
    write_message(&mut conn, &Message::new(T_HEALTH, 1)).await.unwrap();
    let reply = read_message(&mut conn).await.unwrap().unwrap();
    assert_eq!(reply.t(), T_PONG);
    assert_eq!(reply.id(), 1);
    assert!(reply.str_field("version").is_some());
}

#[tokio::test]
async fn exec_streams_stdout_stderr_and_exit_code() {
    let path = start_server().await;
    let mut conn = UnixStream::connect(&path).await.unwrap();
    let msg = exec_msg(7, &["/bin/sh", "-c", "printf out; printf err 1>&2; exit 3"]);
    write_message(&mut conn, &msg).await.unwrap();

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit_code = loop {
        let m = read_message(&mut conn).await.unwrap().unwrap();
        match m.t() {
            T_STDOUT => stdout.extend(m.bytes_field("data").unwrap()),
            T_STDERR => stderr.extend(m.bytes_field("data").unwrap()),
            T_EXIT => break m.i64_field("code").unwrap(),
            _ => {}
        }
    };
    assert_eq!(stdout, b"out");
    assert_eq!(stderr, b"err");
    assert_eq!(exit_code, 3);
}

#[tokio::test]
async fn cancel_kills_the_whole_process_tree() {
    let path = start_server().await;
    let mut conn = UnixStream::connect(&path).await.unwrap();
    // A shell that spawns a child sleep then waits: cancelling must reach the
    // child via the process group, not just the shell.
    let msg = exec_msg(9, &["/bin/sh", "-c", "sleep 60 & sleep 60"]);
    write_message(&mut conn, &msg).await.unwrap();

    // Wait for `started`, then cancel.
    loop {
        let m = read_message(&mut conn).await.unwrap().unwrap();
        if m.t() == agent_sandbox_protocol::T_STARTED {
            break;
        }
    }
    write_message(&mut conn, &Message::new(agent_sandbox_protocol::T_CANCEL, 9))
        .await
        .unwrap();

    let exit = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let m = read_message(&mut conn).await.unwrap().unwrap();
            if m.t() == T_EXIT {
                return m;
            }
        }
    })
    .await
    .expect("process should exit promptly after cancel");
    assert!(!exit.bool_field("success"));
}

#[tokio::test]
async fn filesystem_write_then_read_round_trips_binary() {
    let path = start_server().await;
    let mut conn = UnixStream::connect(&path).await.unwrap();
    let dir = std::env::temp_dir().join(format!("tb-fs-{}", next_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("data.bin");
    let payload: Vec<u8> = (0u8..=255).collect();

    write_message(
        &mut conn,
        &Message::new(T_FS_WRITE, 1)
            .with_str("path", file.to_str().unwrap())
            .with_bytes("data", &payload),
    )
    .await
    .unwrap();
    let ack = read_message(&mut conn).await.unwrap().unwrap();
    assert_eq!(ack.t(), T_OK);
    assert_eq!(ack.u64_field("bytes"), Some(256));

    write_message(
        &mut conn,
        &Message::new(T_FS_READ, 2).with_str("path", file.to_str().unwrap()),
    )
    .await
    .unwrap();
    let got = read_message(&mut conn).await.unwrap().unwrap();
    assert_eq!(got.t(), T_OK);
    assert_eq!(got.bytes_field("data"), Some(payload));
}

#[tokio::test]
async fn fs_read_missing_file_is_not_found() {
    let path = start_server().await;
    let mut conn = UnixStream::connect(&path).await.unwrap();
    write_message(
        &mut conn,
        &Message::new(T_FS_READ, 1).with_str("path", "/no/such/file/here"),
    )
    .await
    .unwrap();
    let got = read_message(&mut conn).await.unwrap().unwrap();
    assert_eq!(got.t(), agent_sandbox_protocol::T_ERROR);
    assert_eq!(got.str_field("category"), Some(agent_sandbox_protocol::CAT_NOT_FOUND));
}

#[tokio::test]
async fn pty_echoes_stdin_and_honors_resize() {
    let path = start_server().await;
    let mut conn = UnixStream::connect(&path).await.unwrap();
    // A PTY session: resize to 30x100, then report the window size.
    let msg = exec_msg(5, &["/bin/sh", "-c", "sleep 0.3; stty size"])
        .with_bool("pty", true)
        .with_u64("rows", 24)
        .with_u64("cols", 80);
    write_message(&mut conn, &msg).await.unwrap();

    // Wait for started, then resize.
    loop {
        let m = read_message(&mut conn).await.unwrap().unwrap();
        if m.t() == agent_sandbox_protocol::T_STARTED {
            break;
        }
    }
    write_message(
        &mut conn,
        &Message::new(T_RESIZE, 5).with_u64("rows", 30).with_u64("cols", 100),
    )
    .await
    .unwrap();

    let mut out = Vec::new();
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let m = read_message(&mut conn).await.unwrap().unwrap();
            match m.t() {
                T_STDOUT => out.extend(m.bytes_field("data").unwrap()),
                T_EXIT => break,
                _ => {}
            }
        }
        out
    })
    .await
    .expect("pty command should finish");
    let text = String::from_utf8_lossy(&collected);
    assert!(text.contains("30 100"), "stty size reported: {text:?}");
}

#[tokio::test]
async fn pty_input_is_delivered() {
    let path = start_server().await;
    let mut conn = UnixStream::connect(&path).await.unwrap();
    // `cat` echoes its stdin; with a PTY the terminal also echoes input.
    let msg = exec_msg(6, &["/bin/cat"]).with_bool("pty", true);
    write_message(&mut conn, &msg).await.unwrap();
    loop {
        let m = read_message(&mut conn).await.unwrap().unwrap();
        if m.t() == agent_sandbox_protocol::T_STARTED {
            break;
        }
    }
    write_message(
        &mut conn,
        &Message::new(T_STDIN, 6).with_bytes("data", b"ping\n"),
    )
    .await
    .unwrap();

    let saw = tokio::time::timeout(Duration::from_secs(5), async {
        let mut out = Vec::new();
        loop {
            let m = read_message(&mut conn).await.unwrap().unwrap();
            if m.t() == T_STDOUT {
                out.extend(m.bytes_field("data").unwrap());
                if String::from_utf8_lossy(&out).contains("ping") {
                    return true;
                }
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(saw, "cat over a pty should echo stdin back");

    // Clean up the long-running cat.
    write_message(&mut conn, &Message::new(agent_sandbox_protocol::T_CANCEL, 6))
        .await
        .unwrap();
}
