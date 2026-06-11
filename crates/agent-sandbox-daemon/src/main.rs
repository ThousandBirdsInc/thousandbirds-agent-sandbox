//! Entry point for the in-guest `agent-sandbox-daemon`.
//!
//! Usage:
//!   agent-sandbox-daemon [--vsock-port N] [--unix PATH]
//!
//! With no arguments it listens on the default vsock port
//! ([`agent_sandbox_protocol::DAEMON_VSOCK_PORT`]) on Linux. `--unix PATH` runs
//! it over an `AF_UNIX` socket instead — used by the host-side conformance
//! tests and for local debugging on non-Linux hosts.

use agent_sandbox_protocol::DAEMON_VSOCK_PORT;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    rt.block_on(run());
}

enum Mode {
    Vsock(u32),
    Unix(String),
}

async fn run() {
    let mode = parse_args();
    match mode {
        Mode::Unix(path) => {
            let _ = std::fs::remove_file(&path);
            let listener = match tokio::net::UnixListener::bind(&path) {
                Ok(l) => l,
                Err(err) => {
                    eprintln!("agent-sandbox-daemon: bind unix {path}: {err}");
                    std::process::exit(1);
                }
            };
            eprintln!("agent-sandbox-daemon: listening on unix:{path}");
            if let Err(err) = agent_sandbox_daemon::serve_unix(listener).await {
                eprintln!("agent-sandbox-daemon: serve unix: {err}");
                std::process::exit(1);
            }
        }
        Mode::Vsock(port) => serve_vsock(port).await,
    }
}

#[cfg(target_os = "linux")]
async fn serve_vsock(port: u32) {
    eprintln!("agent-sandbox-daemon: listening on vsock:{port}");
    if let Err(err) = agent_sandbox_daemon::serve_vsock(port).await {
        eprintln!("agent-sandbox-daemon: serve vsock: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
async fn serve_vsock(_port: u32) {
    eprintln!("agent-sandbox-daemon: vsock is only supported on Linux; use --unix PATH");
    std::process::exit(1);
}

fn parse_args() -> Mode {
    let mut args = std::env::args().skip(1);
    let mut mode = Mode::Vsock(DAEMON_VSOCK_PORT);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--vsock-port" => {
                let port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DAEMON_VSOCK_PORT);
                mode = Mode::Vsock(port);
            }
            "--unix" => {
                let path = args.next().unwrap_or_else(|| {
                    eprintln!("agent-sandbox-daemon: --unix requires a PATH");
                    std::process::exit(2);
                });
                mode = Mode::Unix(path);
            }
            "--help" | "-h" => {
                println!("Usage: agent-sandbox-daemon [--vsock-port N] [--unix PATH]");
                std::process::exit(0);
            }
            other => {
                eprintln!("agent-sandbox-daemon: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    mode
}
