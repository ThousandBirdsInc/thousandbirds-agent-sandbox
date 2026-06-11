//! Conformance for the host `DaemonClient` against the real guest daemon
//! server, connected over an `AF_UNIX` socket — the same `handle_connection`
//! code path the guest runs over vsock, and the same client the SDK uses. This
//! verifies the full host↔daemon stack (framing, multiplexing, line
//! reassembly, exit/cancel/fs/git) without booting a VM.

use std::path::PathBuf;
use std::time::Duration;

use thousandbirds_agent_sandbox::daemon_client::{DaemonClient, PtyOptions};
use thousandbirds_agent_sandbox::sdk::GuestCommand;
use tokio::net::UnixListener;

async fn spawn_daemon() -> (DaemonClient, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "tb-daemon-conf-{}-{}",
        std::process::id(),
        unique()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("control.vsock");
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let _ = agent_sandbox_daemon::serve_unix(listener).await;
    });
    // Connect; retry briefly while the accept loop spins up.
    let mut last_err = None;
    for _ in 0..50 {
        match DaemonClient::connect(&path).await {
            Ok(client) => return (client, dir),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    panic!("could not connect to daemon: {last_err:?}");
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

#[tokio::test]
async fn health_handshake_succeeds() {
    let (client, _dir) = spawn_daemon().await;
    let version = client.health().await.expect("health");
    assert!(!version.is_empty(), "daemon should report a version");
}

#[tokio::test]
async fn exec_streams_lines_and_exit_status() {
    let (client, _dir) = spawn_daemon().await;
    let cmd = GuestCommand::new(["/bin/sh", "-c", "echo one; echo two; echo err 1>&2; exit 5"]);
    let (_id, mut proc) = client.exec(&cmd, PtyOptions::off()).await.expect("exec");

    let mut stdout = Vec::new();
    while let Some(line) = proc.stdout.recv().await {
        stdout.push(line);
    }
    let mut stderr = Vec::new();
    while let Some(line) = proc.stderr.recv().await {
        stderr.push(line);
    }
    let exit = proc.exit.await.expect("exit channel").expect("exit status");

    assert_eq!(stdout, vec!["one".to_string(), "two".to_string()]);
    assert_eq!(stderr, vec!["err".to_string()]);
    assert_eq!(exit.code, Some(5));
    assert!(!exit.success);
}

#[tokio::test]
async fn cancel_terminates_a_running_process() {
    let (client, _dir) = spawn_daemon().await;
    let cmd = GuestCommand::new(["/bin/sh", "-c", "sleep 60 & sleep 60"]);
    let (id, mut proc) = client.exec(&cmd, PtyOptions::off()).await.expect("exec");
    // Drain output so the process never blocks on backpressure.
    tokio::spawn(async move { while proc.stdout.recv().await.is_some() {} });

    client.cancel(id).await.expect("cancel");
    let exit = tokio::time::timeout(Duration::from_secs(5), proc.exit)
        .await
        .expect("exit within timeout")
        .expect("exit channel")
        .expect("exit status");
    assert!(!exit.success, "cancelled process should not report success");
}

#[tokio::test]
async fn concurrent_execs_are_multiplexed_independently() {
    let (client, _dir) = spawn_daemon().await;
    let a = client
        .exec(&GuestCommand::new(["/bin/sh", "-c", "echo aaa"]), PtyOptions::off())
        .await
        .unwrap()
        .1;
    let b = client
        .exec(&GuestCommand::new(["/bin/sh", "-c", "echo bbb"]), PtyOptions::off())
        .await
        .unwrap()
        .1;

    async fn collect(mut p: thousandbirds_agent_sandbox::sdk::GuestProcess) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(l) = p.stdout.recv().await {
            out.push(l);
        }
        let _ = p.exit.await;
        out
    }
    let (ra, rb) = tokio::join!(collect(a), collect(b));
    assert_eq!(ra, vec!["aaa".to_string()]);
    assert_eq!(rb, vec!["bbb".to_string()]);
}

#[tokio::test]
async fn filesystem_round_trip_and_not_found() {
    let (client, dir) = spawn_daemon().await;
    let file = dir.join("greeting.txt");
    let path = file.to_str().unwrap();

    let n = client.fs_write(path, b"hello\nworld", "overwrite").await.unwrap();
    assert_eq!(n, 11);
    let got = client.fs_read(path).await.unwrap();
    assert_eq!(got, b"hello\nworld");

    let appended = client.fs_write(path, b"!", "append").await.unwrap();
    assert_eq!(appended, 1);
    assert_eq!(client.fs_read(path).await.unwrap(), b"hello\nworld!");

    let err = client.fs_read("/definitely/not/here").await.unwrap_err();
    assert!(
        matches!(err, thousandbirds_agent_sandbox::Error::NotFound(_)),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn git_runs_in_the_guest() {
    let (client, dir) = spawn_daemon().await;
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo_str = repo.to_str().unwrap();
    // `git init` then `git status --porcelain` via the daemon's git transport.
    let (code, _out, _err) = client
        .git(repo_str, &["init".to_string()])
        .await
        .expect("git init");
    if code != 0 {
        // git not installed on the test host; skip rather than fail.
        eprintln!("skipping: git unavailable (init exit {code})");
        return;
    }
    let (code, out, _err) = client
        .git(repo_str, &["status".to_string(), "--porcelain".to_string()])
        .await
        .expect("git status");
    assert_eq!(code, 0);
    // Fresh repo: clean working tree → empty porcelain output.
    assert!(out.is_empty(), "porcelain output: {:?}", String::from_utf8_lossy(&out));
}
