use crate::api::{ApiServer, Auth, Json, API_VERSION};
use crate::client::{ApiClient, ExecEvent};
use crate::config::{
    AgentImageRef, CredentialProfile, CredentialSpec, LogLevel, NetworkBackend, ProxyHeaderRule,
    SandboxConfig, WorkspaceMount,
};
use crate::credentials::escape_json;
use crate::error::{Error, Result};
use crate::preview::PreviewProxy;
use crate::runtime::{SandboxId, SandboxManager};
use crate::sdk::{GuestCommand, SandboxManagerSdk, SandboxRuntimeSdk};
use crate::snapshot::{ForkOptions, RestoreOptions, SnapshotId, SnapshotOptions};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command as HostCommand;
use std::sync::Arc;
use tokio::net::TcpListener;

pub fn run(args: Vec<String>) -> Result<()> {
    let mut args = args.into_iter();
    let program = args.next().unwrap_or_else(|| "tb-sandbox".to_string());
    let Some(command) = args.next() else {
        print_help(&program);
        return Ok(());
    };
    let rest: Vec<String> = args.collect();

    // API mode: when TB_SANDBOX_API_URL is set, the supported lifecycle/exec
    // commands are dispatched to the localhost API server instead of driving the
    // runtime in-process. Unsupported commands (shell, snapshot, browser, …)
    // fall through to the local path below.
    if let Some(client) = api_client_from_env()? {
        if let Some(result) = dispatch_api(&client, &command, &rest) {
            return result;
        }
    }

    let mut args = rest.into_iter();
    let manager = SandboxManager::new();
    match command.as_str() {
        "server" => run_server(args.collect()),
        "preview" => run_preview(args.collect()),
        "stop" => {
            let id = required_id(args.next(), "stop")?;
            let stop_id = id.clone();
            sdk_block_on(|sdk| async move { sdk.stop_session(&stop_id).await })?;
            println!("stopped {id}");
            Ok(())
        }
        "shell" => {
            let config = parse_sandbox_config(args.collect(), true)?;
            manager.run_shell(config)
        }
        "create" => {
            // Non-interactive lifecycle goes through the SDK so the CLI is a
            // consumer of the same automation surface other services use,
            // rather than a parallel orchestration path.
            let config = parse_sandbox_config(args.collect(), false)?;
            let sandbox = sdk_block_on(|sdk| async move { sdk.create_session(config).await })?;
            println!("created {}", sandbox.id);
            println!("{}", sandbox.state_dir.display());
            Ok(())
        }
        "start" => {
            let id = required_id(args.next(), "start")?;
            let start_id = id.clone();
            sdk_block_on(|sdk| async move { sdk.start_session(&start_id).await })?;
            println!("started {id} (detached; session is running in the background)");
            Ok(())
        }
        "exec" => {
            let id = required_id(args.next(), "exec")?;
            let command = parse_exec_command(args.collect())?;
            run_exec(id, command)
        }
        "list" => {
            for summary in manager.list()? {
                let state = if summary.running {
                    "running"
                } else {
                    "stopped"
                };
                let reserved = summary
                    .reserved_mib
                    .map(|mib| format!("{mib}MiB"))
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    summary.id,
                    state,
                    reserved,
                    summary.state_dir.display(),
                    summary.image_path.display()
                );
            }
            Ok(())
        }
        "inspect" => {
            let id = required_id(args.next(), "inspect")?;
            print!("{}", manager.inspect(&id)?);
            Ok(())
        }
        "resize" => {
            let id = required_id(args.next(), "resize")?;
            let mut target_mib: Option<u32> = None;
            let mut rest = args.peekable();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--memory-mib" => {
                        target_mib = Some(next_value(&mut rest, "--memory-mib")?.parse().map_err(
                            |_| {
                                Error::InvalidArgument(
                                    "--memory-mib must be an integer".to_string(),
                                )
                            },
                        )?);
                    }
                    other => {
                        return Err(Error::InvalidArgument(format!(
                            "unexpected argument to resize: {other}"
                        )));
                    }
                }
            }
            let target_mib = target_mib.ok_or_else(|| {
                Error::InvalidArgument("tb-sandbox resize requires --memory-mib N".to_string())
            })?;
            let response = manager.resize_memory(&id, target_mib)?;
            println!("{response}");
            Ok(())
        }
        "delete" => {
            let id = required_id(args.next(), "delete")?;
            let delete_id = id.clone();
            sdk_block_on(|sdk| async move { sdk.delete_session(&delete_id).await })?;
            println!("deleted {id}");
            Ok(())
        }
        "snapshot" => {
            let id = required_id(args.next(), "snapshot")?;
            let opts = parse_snapshot_options(args.collect())?;
            let snap_id = manager.snapshot(&id, opts)?;
            println!("{snap_id}");
            println!("{}", manager.snapshot_dir(&snap_id).display());
            Ok(())
        }
        "snapshots" => run_snapshots(manager, args.collect()),
        "snapshot-inspect" => {
            let id = required_snapshot_id(args.next(), "snapshot-inspect")?;
            print!("{}", manager.inspect_snapshot(&id)?);
            Ok(())
        }
        "snapshot-delete" => {
            let id = required_snapshot_id(args.next(), "snapshot-delete")?;
            manager.delete_snapshot(&id)?;
            println!("deleted {id}");
            Ok(())
        }
        "restore" => {
            let id = required_snapshot_id(args.next(), "restore")?;
            let opts = parse_restore_options(args.collect())?;
            let sandbox = manager.restore(&id, opts)?;
            println!("restored {}", sandbox.id);
            println!("{}", sandbox.state_dir.display());
            Ok(())
        }
        "fork" => {
            let id = required_id(args.next(), "fork")?;
            let opts = parse_fork_options(args.collect())?;
            let sandbox = manager.fork(&id, opts)?;
            println!("forked {}", sandbox.id);
            println!("{}", sandbox.state_dir.display());
            Ok(())
        }
        "browser" => {
            // Sub-dispatch on the next token. Each subcommand drives the
            // SDK to exec agent-sandbox-browser in the guest plus (for
            // start/view) holds a port forward open for the lifetime of
            // the command. SIGINT cleans up the forward and stops the
            // browser cleanly.
            let sub = args.next().ok_or_else(|| {
                Error::InvalidArgument(
                    "tb-sandbox browser requires a subcommand: start|stop|status|view|stream"
                        .to_string(),
                )
            })?;
            match sub.as_str() {
                "start" => {
                    let id = required_id(args.next(), "browser start")?;
                    let opts = parse_browser_start_options(args.collect())?;
                    run_browser_start(id, opts)
                }
                "view" => {
                    let id = required_id(args.next(), "browser view")?;
                    let opts = parse_browser_view_options(args.collect())?;
                    run_browser_view(id, opts)
                }
                "stream" => {
                    let id = required_id(args.next(), "browser stream")?;
                    let opts = parse_browser_stream_options(args.collect())?;
                    run_browser_stream(id, opts)
                }
                "stop" => {
                    let id = required_id(args.next(), "browser stop")?;
                    run_browser_simple(id, vec!["stop".to_string()])
                }
                "status" => {
                    let id = required_id(args.next(), "browser status")?;
                    run_browser_simple(id, vec!["status".to_string()])
                }
                other => Err(Error::InvalidArgument(format!(
                    "unknown browser subcommand: {other}"
                ))),
            }
        }
        "help" | "--help" | "-h" => {
            print_help(&program);
            Ok(())
        }
        other => Err(Error::InvalidArgument(format!("unknown command {other}"))),
    }
}

fn required_snapshot_id(value: Option<String>, command: &str) -> Result<SnapshotId> {
    let value = value.ok_or_else(|| {
        Error::InvalidArgument(format!("tb-sandbox {command} requires SNAPSHOT_ID"))
    })?;
    SnapshotId::new(value)
}

fn parse_snapshot_options(args: Vec<String>) -> Result<SnapshotOptions> {
    let mut opts = SnapshotOptions::default();
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--name" => opts.name = Some(next_value(&mut iter, "--name")?),
            "--no-quiesce" => opts.skip_quiesce = true,
            "--no-freeze" => opts.quiesce_freeze = false,
            "--capture-memory" => opts.capture_memory = true,
            "--quiesce-timeout" => {
                let secs: u64 = next_value(&mut iter, "--quiesce-timeout")?
                    .parse()
                    .map_err(|_| {
                        Error::InvalidArgument(
                            "--quiesce-timeout must be an integer number of seconds".to_string(),
                        )
                    })?;
                opts.quiesce_timeout = Some(std::time::Duration::from_secs(secs));
            }
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected argument to snapshot: {other}"
                )));
            }
        }
    }
    Ok(opts)
}

fn run_snapshots(manager: SandboxManager, args: Vec<String>) -> Result<()> {
    if args.is_empty() {
        for summary in manager.snapshots()? {
            let bytes = summary
                .root_disk_bytes
                .map(|b| format!("{b}B"))
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                summary.id,
                summary.kind.as_str(),
                summary.parent_sandbox_id,
                summary.created_unix_millis,
                bytes,
                summary.state_dir.display(),
            );
        }
        return Ok(());
    }

    if args.first().map(String::as_str) != Some("prune") {
        return Err(Error::InvalidArgument(format!(
            "unknown snapshots subcommand {}",
            args[0]
        )));
    }
    let mut delete = false;
    for arg in args.into_iter().skip(1) {
        match arg.as_str() {
            "--delete" => delete = true,
            "--dry-run" => delete = false,
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected argument to snapshots prune: {other}"
                )));
            }
        }
    }

    let report = manager.prune_snapshots(delete)?;
    if report.entries.is_empty() {
        println!("no unusable snapshots found");
        return Ok(());
    }
    for entry in &report.entries {
        let label = entry
            .id
            .as_ref()
            .map(|id| id.as_str().to_string())
            .unwrap_or_else(|| entry.state_dir.display().to_string());
        println!(
            "{}\t{}\t{}",
            if report.deleted {
                "deleted"
            } else {
                "would-delete"
            },
            label,
            entry.reason
        );
    }
    if !report.deleted {
        eprintln!(
            "run `tb-sandbox snapshots prune --delete` to remove these {} snapshot(s)",
            report.entries.len()
        );
    }
    Ok(())
}

fn parse_restore_options(args: Vec<String>) -> Result<RestoreOptions> {
    let mut opts = RestoreOptions::default();
    let mut workspace_override: Option<Vec<WorkspaceMount>> = None;
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--name" => opts.name = Some(next_value(&mut iter, "--name")?),
            "--image-path" => {
                let path = PathBuf::from(next_value(&mut iter, "--image-path")?);
                opts.image_path = Some(canonicalize_existing(&path)?);
            }
            "--workspace" => {
                let path = PathBuf::from(next_value(&mut iter, "--workspace")?);
                workspace_override = Some(vec![WorkspaceMount::workspace(canonicalize_existing(
                    &path,
                )?)]);
            }
            "--no-workspace" => {
                workspace_override = Some(Vec::new());
            }
            "--no-credentials" => opts.drop_credentials = true,
            "--cpus" => {
                opts.cpus = Some(next_value(&mut iter, "--cpus")?.parse().map_err(|_| {
                    Error::InvalidArgument("--cpus must be an integer".to_string())
                })?);
            }
            "--memory-mib" => {
                opts.memory_mib = Some(next_value(&mut iter, "--memory-mib")?.parse().map_err(
                    |_| Error::InvalidArgument("--memory-mib must be an integer".to_string()),
                )?);
            }
            "--memory-max-mib" => {
                opts.memory_max_mib = Some(
                    next_value(&mut iter, "--memory-max-mib")?
                        .parse()
                        .map_err(|_| {
                            Error::InvalidArgument(
                                "--memory-max-mib must be an integer".to_string(),
                            )
                        })?,
                );
            }
            "--memory-min-mib" => {
                opts.memory_min_mib = Some(
                    next_value(&mut iter, "--memory-min-mib")?
                        .parse()
                        .map_err(|_| {
                            Error::InvalidArgument(
                                "--memory-min-mib must be an integer".to_string(),
                            )
                        })?,
                );
            }
            "--ephemeral" => opts.ephemeral = true,
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected argument to restore: {other}"
                )));
            }
        }
    }
    opts.workspace_mounts = workspace_override;
    Ok(opts)
}

// --- browser subcommand ----------------------------------------------------

#[derive(Default)]
struct BrowserStartOptions {
    headed: bool,
    cdp_port: Option<u16>,
    vnc_port: Option<u16>,
    profile: Option<String>,
    /// Open the local VNC URL after forwarding it. Only used by `browser view`.
    open_vnc: bool,
    /// Skip opening the browser command — useful when the caller just wants
    /// the port-forward loop without invoking start.
    skip_start: bool,
}

#[derive(Default)]
struct BrowserViewOptions {
    cdp_port: Option<u16>,
    vnc_port: Option<u16>,
    profile: Option<String>,
    /// If set, do not try to spawn a local VNC client; just print the URL.
    no_open: bool,
}

fn parse_browser_start_options(args: Vec<String>) -> Result<BrowserStartOptions> {
    let mut opts = BrowserStartOptions::default();
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--headless" => opts.headed = false,
            "--headed" => opts.headed = true,
            "--cdp-port" => {
                opts.cdp_port = Some(parse_port(&next_value(&mut iter, "--cdp-port")?)?);
            }
            "--vnc-port" => {
                opts.vnc_port = Some(parse_port(&next_value(&mut iter, "--vnc-port")?)?);
            }
            "--profile" => opts.profile = Some(next_value(&mut iter, "--profile")?),
            "--no-start" => opts.skip_start = true,
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected argument to browser start: {other}"
                )));
            }
        }
    }
    Ok(opts)
}

fn parse_browser_view_options(args: Vec<String>) -> Result<BrowserViewOptions> {
    let mut opts = BrowserViewOptions::default();
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--cdp-port" => {
                opts.cdp_port = Some(parse_port(&next_value(&mut iter, "--cdp-port")?)?);
            }
            "--vnc-port" => {
                opts.vnc_port = Some(parse_port(&next_value(&mut iter, "--vnc-port")?)?);
            }
            "--profile" => opts.profile = Some(next_value(&mut iter, "--profile")?),
            "--no-open" => opts.no_open = true,
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected argument to browser view: {other}"
                )));
            }
        }
    }
    Ok(opts)
}

#[derive(Default)]
struct BrowserStreamOptions {
    cdp_port: Option<u16>,
    /// Stream headed too (lets a VNC session run alongside). Streaming itself
    /// needs no display, so the default is headless.
    headed: bool,
    profile: Option<String>,
    /// Host address for the viewer server; `127.0.0.1:0` picks a free port.
    listen: String,
    /// Skip auto-opening the viewer URL in the local browser.
    no_open: bool,
}

fn parse_browser_stream_options(args: Vec<String>) -> Result<BrowserStreamOptions> {
    let mut opts = BrowserStreamOptions {
        listen: "127.0.0.1:0".to_string(),
        ..Default::default()
    };
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--cdp-port" => {
                opts.cdp_port = Some(parse_port(&next_value(&mut iter, "--cdp-port")?)?);
            }
            "--headed" => opts.headed = true,
            "--headless" => opts.headed = false,
            "--profile" => opts.profile = Some(next_value(&mut iter, "--profile")?),
            "--listen" => opts.listen = next_value(&mut iter, "--listen")?,
            "--no-open" => opts.no_open = true,
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected argument to browser stream: {other}"
                )));
            }
        }
    }
    Ok(opts)
}

fn parse_port(value: &str) -> Result<u16> {
    value
        .parse::<u16>()
        .map_err(|_| Error::InvalidArgument(format!("port must be 1..=65535, got {value}")))
}

const GUEST_BROWSER_BIN: &str = "/usr/local/bin/agent-sandbox-browser";

/// Run agent-sandbox-browser <args> in the guest; print combined output
/// (its key=value status / start results) and return.
fn run_browser_simple(id: SandboxId, browser_args: Vec<String>) -> Result<()> {
    sdk_block_on(|sdk| async move {
        let mut argv = vec![GUEST_BROWSER_BIN.to_string()];
        argv.extend(browser_args);
        let mut process = sdk.exec(&id, GuestCommand::new(argv)).await?;
        let mut exit_rx = process.exit;
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut exit = None;
        loop {
            tokio::select! {
                line = process.stdout.recv(), if stdout_open => match line {
                    Some(line) => println!("{line}"),
                    None => stdout_open = false,
                },
                line = process.stderr.recv(), if stderr_open => match line {
                    Some(line) => eprintln!("{line}"),
                    None => stderr_open = false,
                },
                status = &mut exit_rx, if exit.is_none() => {
                    exit = Some(status.map_err(|_| {
                        Error::Internal("guest browser exit channel closed".to_string())
                    })??);
                }
            }
            if exit.is_some() && !stdout_open && !stderr_open {
                break;
            }
        }
        Ok(())
    })
}

/// Exec `agent-sandbox-browser start` in the guest, streaming its `key=value`
/// output through, and verify it reported `status=running`. Idempotent —
/// re-invocation on a running browser just re-prints status. Shared by
/// `browser start`/`view` and `browser stream`.
async fn exec_browser_start(
    sdk: &SandboxManagerSdk,
    id: &SandboxId,
    headed: bool,
    cdp_port: Option<u16>,
    vnc_port: Option<u16>,
    profile: &Option<String>,
) -> Result<()> {
    let mut argv = vec![GUEST_BROWSER_BIN.to_string(), "start".to_string()];
    argv.push(if headed {
        "--headed".into()
    } else {
        "--headless".into()
    });
    if let Some(p) = cdp_port {
        argv.push("--cdp-port".into());
        argv.push(p.to_string());
    }
    if let Some(p) = vnc_port {
        argv.push("--vnc-port".into());
        argv.push(p.to_string());
    }
    if let Some(profile) = profile {
        argv.push("--profile".into());
        argv.push(profile.clone());
    }
    let mut process = sdk.exec(id, GuestCommand::new(argv)).await?;
    // Drain the start command's output (it prints status= lines).
    let mut exit_rx = process.exit;
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut exit = None;
    let mut saw_running = false;
    loop {
        tokio::select! {
            line = process.stdout.recv(), if stdout_open => match line {
                Some(line) => {
                    if line == "status=running" {
                        saw_running = true;
                    }
                    println!("{line}");
                },
                None => stdout_open = false,
            },
            line = process.stderr.recv(), if stderr_open => match line {
                Some(line) => eprintln!("{line}"),
                None => stderr_open = false,
            },
            status = &mut exit_rx, if exit.is_none() => {
                exit = Some(status.map_err(|_| {
                    Error::Internal("guest browser exit channel closed".to_string())
                })??);
            }
        }
        if exit.is_some() && !stdout_open && !stderr_open {
            break;
        }
    }
    if !saw_running {
        return Err(Error::InvalidArgument(
            "browser did not report status=running; refusing to open forwards".to_string(),
        ));
    }
    Ok(())
}

fn run_browser_start(id: SandboxId, opts: BrowserStartOptions) -> Result<()> {
    sdk_block_on(|sdk| async move {
        // 1. Start the browser in the guest (idempotent).
        if !opts.skip_start {
            exec_browser_start(
                &sdk,
                &id,
                opts.headed,
                opts.cdp_port,
                opts.vnc_port,
                &opts.profile,
            )
            .await?;
        }

        // 2. Forward the CDP port (and the VNC port if headed) to host
        //    localhost ports. The forwards are SDK-owned and live as long
        //    as this command does.
        let cdp_port = opts.cdp_port.unwrap_or(9222);
        let cdp_forward = sdk.forward_port(&id, cdp_port).await?;
        println!("cdp_url=http://{}:{}", cdp_forward.host, cdp_forward.port);
        let _vnc_forward = if opts.headed {
            let vnc_port = opts.vnc_port.unwrap_or(5900);
            let f = sdk.forward_port(&id, vnc_port).await?;
            let vnc_url = format!("vnc://{}:{}", f.host, f.port);
            println!("vnc_url={vnc_url}");
            if f.native {
                eprintln!("browser: VNC forward using native gvproxy transport (low latency)");
            } else {
                eprintln!(
                    "browser: VNC forward using the file-relay fallback — interactivity will be \
                     poor. Recreate this sandbox with gvproxy networking (the default when \
                     `gvproxy` is installed; otherwise pass `--gvproxy`) for a responsive session."
                );
            }
            if opts.open_vnc {
                open_vnc_url(&vnc_url);
            }
            Some(f)
        } else {
            None
        };

        eprintln!(
            "browser: forwarding cdp port; press Ctrl-C to stop browser and tear down forwards"
        );

        // 3. Wait for SIGINT, then stop browser + drop forwards (the SDK's
        //    Drop tears down the host listener; the guest port-forwarder
        //    process is killed via sdk.cancel_process inside the SDK's
        //    teardown when stop_forwards is called).
        tokio::signal::ctrl_c().await.map_err(Error::io)?;
        eprintln!("browser: stopping...");
        let _ = sdk.stop_forwards(&id).await;
        let stop = sdk
            .exec(
                &id,
                GuestCommand::new([GUEST_BROWSER_BIN.to_string(), "stop".to_string()]),
            )
            .await?;
        let _ = stop.exit.await;
        Ok(())
    })
}

fn run_browser_view(id: SandboxId, opts: BrowserViewOptions) -> Result<()> {
    // View = headed start + open local VNC client against the forwarded
    // port. Reuses run_browser_start with headed=true plus an extra host-side
    // URL open when the platform has a default VNC URL handler.
    let no_open = opts.no_open;
    let macos_vnc_url_open = !no_open && cfg!(target_os = "macos");
    let start_opts = BrowserStartOptions {
        headed: true,
        cdp_port: opts.cdp_port,
        vnc_port: opts.vnc_port,
        profile: opts.profile,
        open_vnc: macos_vnc_url_open,
        skip_start: false,
    };
    // On macOS, Screen Sharing handles vnc:// URLs by default; fall back to
    // the printed URL if the user disabled opening or has no handler.
    if macos_vnc_url_open {
        eprintln!("browser view: opening the forwarded vnc:// URL with the macOS URL handler");
    }
    run_browser_start(id, start_opts)
}

/// `browser stream`: start the browser, forward CDP, and run the host-side
/// DOM-diff stream server. Prints a `stream_url=` the user opens in any browser
/// for an efficient, interactive view of the in-sandbox Chromium — no VNC, and
/// (by default) no display server in the guest.
/// Launch (idempotently) a small in-guest TCP relay so Chromium's loopback-only
/// CDP becomes reachable on the guest's external interface — and therefore
/// through the gvproxy port forward. Uses the guest's python3 (already a
/// requirement of the file-relay forwarder) and detaches via setsid so the relay
/// outlives this exec.
async fn launch_cdp_relay(
    sdk: &SandboxManagerSdk,
    id: &SandboxId,
    listen_port: u16,
    target_port: u16,
) -> Result<()> {
    // Note: the embedded python contains no `{`/`}`, so the only format
    // placeholders are the named port args below.
    let script = format!(
        r#"cat > /tmp/tb-cdp-relay.py <<'PYEOF'
import socket, threading, sys
listen_port = int(sys.argv[1]); target_port = int(sys.argv[2])
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", listen_port)); srv.listen(128)
def pump(a, b):
    try:
        while True:
            data = a.recv(65536)
            if not data:
                break
            b.sendall(data)
    except Exception:
        pass
    try:
        b.shutdown(socket.SHUT_WR)
    except Exception:
        pass
def handle(client):
    try:
        upstream = socket.create_connection(("127.0.0.1", target_port))
    except Exception:
        client.close(); return
    threading.Thread(target=pump, args=(client, upstream), daemon=True).start()
    threading.Thread(target=pump, args=(upstream, client), daemon=True).start()
while True:
    conn, _ = srv.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
PYEOF
# Only start if nothing is already serving on the relay port (idempotent).
if python3 -c 'import socket,sys; s=socket.socket(); s.settimeout(0.5); sys.exit(0 if s.connect_ex(("127.0.0.1", {listen}))==0 else 1)' 2>/dev/null; then
  echo relay-already-running
else
  setsid python3 /tmp/tb-cdp-relay.py {listen} {target} >/tmp/tb-cdp-relay.log 2>&1 </dev/null &
  echo relay-started
fi
"#,
        listen = listen_port,
        target = target_port,
    );
    let mut process = sdk
        .exec(
            id,
            GuestCommand::new(["sh".to_string(), "-c".to_string(), script]),
        )
        .await?;
    // Drain to completion; the relay itself is detached (setsid) and keeps
    // running.
    while process.stdout.recv().await.is_some() {}
    while process.stderr.recv().await.is_some() {}
    let _ = process.exit.await;
    Ok(())
}

fn run_browser_stream(id: SandboxId, opts: BrowserStreamOptions) -> Result<()> {
    sdk_block_on(|sdk| async move {
        // 1. Ensure a persistent guest session exists so this is a single
        //    foreground command — no separate `start` needed. If we start it,
        //    we own it and tear the VM down on exit; if it was already running,
        //    we leave it alone.
        let owns_session = !sdk.session_active(&id).await?;
        if owns_session {
            eprintln!("browser: booting sandbox VM (first boot can take up to ~90s)…");
            sdk.start_session(&id).await?;
        }

        // 2. Start the browser (idempotent).
        eprintln!("browser: starting Chromium in the sandbox…");
        exec_browser_start(&sdk, &id, opts.headed, opts.cdp_port, None, &opts.profile).await?;
        let cdp_port = opts.cdp_port.unwrap_or(9222);

        // Modern Chromium ignores --remote-debugging-address and binds CDP to
        // 127.0.0.1 only, so the gvproxy forward (which targets the guest's
        // external IP) can't reach it. Launch a small in-guest relay that
        // listens on all interfaces and bridges to Chromium's loopback CDP, then
        // forward the relay port instead. (VNC needs no such relay — x11vnc
        // honours -listen 0.0.0.0.)
        let relay_port = cdp_port.wrapping_add(1).max(1);
        launch_cdp_relay(&sdk, &id, relay_port, cdp_port).await?;
        let cdp = sdk.forward_port(&id, relay_port).await?;
        if !cdp.native {
            eprintln!(
                "browser: CDP forward using the file-relay fallback — the stream will work but \
                 latency will be higher. Recreate this sandbox with gvproxy networking for the \
                 low-latency path."
            );
        }

        // 3. Bind the viewer server and announce the URL.
        let listener = TcpListener::bind(&opts.listen).await.map_err(Error::io)?;
        let addr = listener.local_addr().map_err(Error::io)?;
        println!("stream_url=http://{addr}");
        eprintln!(
            "browser: DOM-diff stream live; open the stream_url above. Press Ctrl-C to stop the \
             browser and tear down."
        );
        if !opts.no_open {
            open_stream_url(&format!("http://{addr}"));
        }

        // 4. Run the stream server until the user interrupts or it errors.
        //    Capture the outcome but always run teardown (a serve() failure
        //    must not leak a VM we started).
        let serve = crate::browser_stream::serve(cdp.host.clone(), cdp.port, listener);
        let outcome = tokio::select! {
            r = serve => r,
            _ = tokio::signal::ctrl_c() => Ok(()),
        };

        eprintln!("browser: stopping...");
        let _ = sdk.stop_forwards(&id).await;
        if owns_session {
            // We started the VM for this stream, so stop it too. stop_session
            // tears down the guest command server and the VM.
            let _ = sdk.stop_session(&id).await;
        } else {
            // The session predated us — just stop the browser, leave the VM.
            if let Ok(stop) = sdk
                .exec(
                    &id,
                    GuestCommand::new([GUEST_BROWSER_BIN.to_string(), "stop".to_string()]),
                )
                .await
            {
                let _ = stop.exit.await;
            }
        }
        outcome
    })
}

/// Open the DOM-stream viewer URL in the host's default browser (best effort).
fn open_stream_url(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    match HostCommand::new(opener).arg(url).status() {
        Ok(status) if status.success() => eprintln!("browser: opened {url}"),
        Ok(status) => eprintln!("browser: {opener} {url} exited with {status}; open it manually"),
        Err(err) => eprintln!("browser: failed to open {url} ({err}); open it manually"),
    }
}

/// Locate a TigerVNC `vncviewer` binary, if installed. TigerVNC negotiates the
/// Tight encoding with JPEG, which macOS's built-in Screen Sharing does not —
/// a large bandwidth/CPU win for an interactive browser framebuffer.
fn find_tigervnc() -> Option<PathBuf> {
    if let Ok(path) = env::var("TB_SANDBOX_VNC_VIEWER") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    let candidates = [
        "/Applications/TigerVNC.app/Contents/MacOS/vncviewer",
        "/opt/homebrew/bin/vncviewer",
        "/usr/local/bin/vncviewer",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn open_vnc_url(vnc_url: &str) {
    // Prefer TigerVNC: it speaks Tight+JPEG, so the framebuffer ships as small
    // compressed deltas rather than the near-raw updates Screen Sharing pulls.
    // Tuned for the native (gvproxy) transport, where bandwidth is ample
    // (~5 Gbit/s localhost) so we trade a little of it for lower CPU and crisp
    // quality: high QualityLevel, low CompressLevel.
    if let Some(viewer) = find_tigervnc() {
        let host_port = vnc_url.strip_prefix("vnc://").unwrap_or(vnc_url);
        let spawned = HostCommand::new(&viewer)
            .arg("-PreferredEncoding=Tight")
            .arg("-QualityLevel=8")
            .arg("-CompressLevel=1")
            .arg("-FullScreen=0")
            .arg(host_port)
            .spawn();
        match spawned {
            Ok(_) => {
                eprintln!(
                    "browser: opened {host_port} with TigerVNC (Tight+JPEG). Password: the \
                     vnc_password printed above (default `sandbox`)."
                );
                return;
            }
            Err(err) => {
                eprintln!(
                    "browser: failed to launch TigerVNC ({err}); falling back to Screen Sharing"
                );
            }
        }
    }

    let screen_sharing = HostCommand::new("open")
        .arg("-a")
        .arg("Screen Sharing")
        .arg(vnc_url)
        .status();
    match screen_sharing {
        Ok(status) if status.success() => {
            eprintln!("browser: opened {vnc_url} with Screen Sharing");
            return;
        }
        Ok(status) => {
            eprintln!("browser: Screen Sharing open exited with {status}");
        }
        Err(err) => {
            eprintln!("browser: failed to run Screen Sharing for {vnc_url}: {err}");
        }
    }

    let generic = HostCommand::new("open").arg(vnc_url).status();
    match generic {
        Ok(status) if status.success() => {
            eprintln!("browser: opened {vnc_url}");
        }
        Ok(status) => {
            eprintln!("browser: open {vnc_url} exited with {status}; manually open {vnc_url}");
        }
        Err(err) => {
            eprintln!("browser: failed to run open for {vnc_url}: {err}; manually open {vnc_url}");
        }
    }
}

fn parse_fork_options(args: Vec<String>) -> Result<ForkOptions> {
    let mut opts = ForkOptions::default();
    let mut workspace_override: Option<Vec<WorkspaceMount>> = None;
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--name" => opts.name = Some(next_value(&mut iter, "--name")?),
            "--workspace" => {
                let path = PathBuf::from(next_value(&mut iter, "--workspace")?);
                workspace_override = Some(vec![WorkspaceMount::workspace(canonicalize_existing(
                    &path,
                )?)]);
            }
            "--no-workspace" => {
                workspace_override = Some(Vec::new());
            }
            "--no-credentials" => opts.drop_credentials = true,
            "--ephemeral" => opts.ephemeral = true,
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected argument to fork: {other}"
                )));
            }
        }
    }
    opts.workspace_mounts = workspace_override;
    Ok(opts)
}

/// Build a single-threaded tokio runtime and run an async SDK operation to
/// completion. Lets the synchronous CLI drive the async `SandboxRuntimeSdk`.
fn sdk_block_on<F, T>(f: impl FnOnce(SandboxManagerSdk) -> F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let sdk = SandboxManagerSdk::new(SandboxManager::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::io)?;
    runtime.block_on(f(sdk))
}

// ---------------------------------------------------------------------------
// API server + API client mode (Daytona gap plan, Phase 2)
// ---------------------------------------------------------------------------

/// Start the localhost API server backed by the real VM runtime.
fn run_server(args: Vec<String>) -> Result<()> {
    let mut bind = "127.0.0.1:8080".to_string();
    let mut no_auth = false;
    let mut token_arg: Option<String> = None;
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--bind" => bind = next_value(&mut iter, "--bind")?,
            "--no-auth" => no_auth = true,
            "--token" => token_arg = Some(next_value(&mut iter, "--token")?),
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected argument to server: {other}"
                )));
            }
        }
    }

    let manager = SandboxManager::new();
    let auth = if no_auth {
        Auth::None
    } else {
        let token = match token_arg {
            Some(token) => token,
            None => load_or_create_token(&manager.state_root)?,
        };
        Auth::Bearer(token)
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::io)?;
    runtime.block_on(async move {
        let listener = TcpListener::bind(&bind).await.map_err(Error::io)?;
        let addr = listener.local_addr().map_err(Error::io)?;
        let sdk = SandboxManagerSdk::new(manager);
        let server = ApiServer::new(Arc::new(sdk), auth.clone());
        println!("tb-sandbox API server v{API_VERSION} listening on http://{addr}");
        match &auth {
            Auth::Bearer(token) => {
                println!("API token: {token}");
                println!(
                    "clients: export TB_SANDBOX_API_URL=http://{addr} TB_SANDBOX_API_TOKEN={token}"
                );
            }
            Auth::None => println!("authentication: disabled (--no-auth)"),
        }
        let mut events = server.subscribe();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                println!("event {event}");
            }
        });
        server.serve(listener).await
    })
}

/// Start the preview reverse proxy in front of a running API server.
fn run_preview(args: Vec<String>) -> Result<()> {
    let mut bind = "127.0.0.1:0".to_string();
    let mut no_auth = false;
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--bind" => bind = next_value(&mut iter, "--bind")?,
            "--no-auth" => no_auth = true,
            other if other.starts_with("--") => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected argument to preview: {other}"
                )));
            }
            other => positional.push(other.to_string()),
        }
    }

    let client = api_client_from_env()?.ok_or_else(|| {
        Error::InvalidArgument(
            "tb-sandbox preview requires TB_SANDBOX_API_URL to point at a running server"
                .to_string(),
        )
    })?;
    let require_token = if no_auth {
        None
    } else {
        Some(resolved_preview_token()?)
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::io)?;
    runtime.block_on(async move {
        let listener = TcpListener::bind(&bind).await.map_err(Error::io)?;
        let addr = listener.local_addr().map_err(Error::io)?;
        println!("tb-sandbox preview proxy listening on http://{addr}");
        if positional.len() >= 2 {
            let id = &positional[0];
            let port = &positional[1];
            let token_query = require_token
                .as_ref()
                .map(|token| format!("?token={token}"))
                .unwrap_or_default();
            println!(
                "preview URL: http://{port}-{id}.localhost:{}/{token_query}",
                addr.port()
            );
            println!(
                "(ensure {port}-{id}.localhost resolves to 127.0.0.1; *.localhost usually does)"
            );
        } else {
            println!(
                "open http://{{guest_port}}-{{sandbox_id}}.localhost:{}/ to reach a guest server",
                addr.port()
            );
        }
        let proxy = PreviewProxy::new(client, require_token);
        proxy.serve(listener).await
    })
}

/// Resolve the API token from the environment or the persisted state-dir file.
fn resolved_api_token() -> Option<String> {
    env::var("TB_SANDBOX_API_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
        .or_else(|| {
            let path = SandboxManager::new().state_root.join("api-token");
            std::fs::read_to_string(path)
                .ok()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
        })
}

/// Resolve the preview proxy token, kept DISTINCT from the control-plane API
/// token so that even if it leaks (it rides in URLs, browser history, and
/// `Referer` headers) it cannot be replayed against the API server. Reads
/// `TB_SANDBOX_PREVIEW_TOKEN`, else a persisted `preview-token` file generated
/// on first use.
fn resolved_preview_token() -> Result<String> {
    if let Ok(token) = env::var("TB_SANDBOX_PREVIEW_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }
    load_or_create_named_token(&SandboxManager::new().state_root, "preview-token")
}

/// Load the persisted API token, generating and storing one on first use.
fn load_or_create_token(state_root: &Path) -> Result<String> {
    load_or_create_named_token(state_root, "api-token")
}

/// Load a persisted token file under `state_root`, generating and storing a
/// fresh one (0600) on first use.
fn load_or_create_named_token(state_root: &Path, name: &str) -> Result<String> {
    let path = state_root.join(name);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    std::fs::create_dir_all(state_root).map_err(|err| Error::io_path(state_root, err))?;
    let token = generate_token();
    std::fs::write(&path, &token).map_err(|err| Error::io_path(&path, err))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .map_err(|err| Error::io_path(&path, err))?
            .permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(&path, perms);
    }
    Ok(token)
}

/// Generate a random API token. Uses `/dev/urandom` for entropy; this is a
/// localhost bearer token, not a long-lived secret store.
fn generate_token() -> String {
    let mut bytes = [0u8; 24];
    use std::io::Read;
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(&mut bytes).is_ok() {
            return hex_encode(&bytes);
        }
    }
    // Fallback: time + pid mixed into the buffer. Weaker, but only reached if
    // /dev/urandom is unavailable.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mixed = nanos ^ (pid << 64) ^ (pid.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    format!("{mixed:032x}")
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Build an API client when `TB_SANDBOX_API_URL` is set; otherwise `None`.
fn api_client_from_env() -> Result<Option<ApiClient>> {
    let Some(url) = env::var_os("TB_SANDBOX_API_URL") else {
        return Ok(None);
    };
    let url = url.to_string_lossy().to_string();
    if url.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(ApiClient::new(&url, resolved_api_token())?))
}

/// Dispatch a CLI command through the API client. Returns `None` for commands
/// that are not supported in API mode (they fall through to the local path).
fn dispatch_api(client: &ApiClient, command: &str, rest: &[String]) -> Option<Result<()>> {
    match command {
        "create" => Some(api_create(client, rest.to_vec())),
        "list" => Some(api_list(client)),
        "inspect" => Some(api_inspect(client, rest)),
        "start" => Some(api_start(client, rest)),
        "stop" => Some(api_stop(client, rest)),
        "exec" => Some(api_exec(client, rest.to_vec())),
        "delete" => Some(api_delete(client, rest)),
        _ => None,
    }
}

fn api_block_on<F, T>(fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::io)?
        .block_on(fut)
}

fn api_required_id<'a>(rest: &'a [String], command: &str) -> Result<&'a str> {
    rest.first()
        .map(String::as_str)
        .ok_or_else(|| Error::InvalidArgument(format!("tb-sandbox {command} requires SANDBOX_ID")))
}

fn api_create(client: &ApiClient, args: Vec<String>) -> Result<()> {
    let config = parse_sandbox_config(args, false)?;
    let body = config_to_json(&config);
    let response = api_block_on(client.post("/v1/sandboxes", &body))?;
    let id = response
        .get("id")
        .and_then(Json::as_str)
        .unwrap_or_default();
    let state_dir = response
        .get("state_dir")
        .and_then(Json::as_str)
        .unwrap_or_default();
    println!("created {id}");
    println!("{state_dir}");
    Ok(())
}

fn api_list(client: &ApiClient) -> Result<()> {
    let response = api_block_on(client.get("/v1/sandboxes"))?;
    if let Some(Json::Arr(items)) = response.get("sandboxes") {
        for item in items {
            let id = item.get("id").and_then(Json::as_str).unwrap_or_default();
            let state = item.get("state").and_then(Json::as_str).unwrap_or_default();
            let state_dir = item
                .get("state_dir")
                .and_then(Json::as_str)
                .unwrap_or_default();
            println!("{id}\t{state}\t{state_dir}");
        }
    }
    Ok(())
}

fn api_inspect(client: &ApiClient, rest: &[String]) -> Result<()> {
    let id = api_required_id(rest, "inspect")?;
    let response = api_block_on(client.get(&format!("/v1/sandboxes/{id}")))?;
    let state = response.get("state").and_then(Json::as_str).unwrap_or("");
    let state_dir = response
        .get("state_dir")
        .and_then(Json::as_str)
        .unwrap_or("");
    println!("id\t{id}");
    println!("state\t{state}");
    println!("state_dir\t{state_dir}");
    if let Some(Json::Arr(mounts)) = response.get("mounts") {
        for mount in mounts {
            let tag = mount.get("tag").and_then(Json::as_str).unwrap_or("");
            let guest = mount.get("guest_path").and_then(Json::as_str).unwrap_or("");
            let host = mount.get("host_path").and_then(Json::as_str).unwrap_or("");
            let ro = mount
                .get("read_only")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            let mode = if ro { "ro" } else { "rw" };
            println!("mount\t{tag}\t{guest}\t{host}\t{mode}");
        }
    }
    Ok(())
}

fn api_start(client: &ApiClient, rest: &[String]) -> Result<()> {
    let id = api_required_id(rest, "start")?;
    api_block_on(client.post(&format!("/v1/sandboxes/{id}/start"), "{}"))?;
    println!("started {id}");
    Ok(())
}

fn api_stop(client: &ApiClient, rest: &[String]) -> Result<()> {
    let id = api_required_id(rest, "stop")?;
    api_block_on(client.post(&format!("/v1/sandboxes/{id}/stop"), "{}"))?;
    println!("stopped {id}");
    Ok(())
}

fn api_delete(client: &ApiClient, rest: &[String]) -> Result<()> {
    let id = api_required_id(rest, "delete")?;
    api_block_on(client.delete(&format!("/v1/sandboxes/{id}")))?;
    println!("deleted {id}");
    Ok(())
}

fn api_exec(client: &ApiClient, args: Vec<String>) -> Result<()> {
    let mut iter = args.into_iter();
    let id = iter
        .next()
        .ok_or_else(|| Error::InvalidArgument("tb-sandbox exec requires SANDBOX_ID".to_string()))?;
    let command = parse_exec_command(iter.collect())?;
    let body = command_to_json(&command);
    let (code, _success) = api_block_on(client.exec_stream(&id, &body, |event| match event {
        ExecEvent::Stdout(line) => println!("{line}"),
        ExecEvent::Stderr(line) => eprintln!("{line}"),
        ExecEvent::Error(message) => eprintln!("error: {message}"),
        ExecEvent::Started { .. } | ExecEvent::Exit { .. } => {}
    }))?;
    std::process::exit(code.unwrap_or(0));
}

/// Serialize the API-settable subset of a [`SandboxConfig`] to a JSON body.
fn config_to_json(config: &SandboxConfig) -> String {
    let mut fields = vec![format!(
        "\"image\":\"{}\"",
        escape_json(&config.image.path.display().to_string())
    )];
    if let Some(name) = &config.name {
        fields.push(format!("\"name\":\"{}\"", escape_json(name)));
    }
    fields.push(format!("\"cpus\":{}", config.cpus));
    fields.push(format!("\"memory_mib\":{}", config.memory_mib));
    fields.push(format!("\"memory_max_mib\":{}", config.memory_max_mib));
    fields.push(format!("\"memory_min_mib\":{}", config.memory_min_mib));
    let mounts: Vec<String> = config
        .workspace_mounts
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
    fields.push(format!("\"mounts\":[{}]", mounts.join(",")));
    format!("{{{}}}", fields.join(","))
}

/// Serialize a [`GuestCommand`] to the exec request body shape.
fn command_to_json(command: &GuestCommand) -> String {
    let argv: Vec<String> = command
        .argv
        .iter()
        .map(|arg| format!("\"{}\"", escape_json(arg)))
        .collect();
    let mut fields = vec![format!("\"argv\":[{}]", argv.join(","))];
    if let Some(cwd) = &command.cwd {
        fields.push(format!("\"cwd\":\"{}\"", escape_json(cwd)));
    }
    if !command.env.is_empty() {
        let env: Vec<String> = command
            .env
            .iter()
            .map(|(key, value)| format!("\"{}\":\"{}\"", escape_json(key), escape_json(value)))
            .collect();
        fields.push(format!("\"env\":{{{}}}", env.join(",")));
    }
    format!("{{{}}}", fields.join(","))
}

fn parse_exec_command(args: Vec<String>) -> Result<GuestCommand> {
    let mut cwd = None;
    let mut env = Vec::new();
    let mut argv: Vec<String> = Vec::new();
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--cwd" => cwd = Some(next_value(&mut iter, "--cwd")?),
            "--env" => {
                let spec = next_value(&mut iter, "--env")?;
                let (name, value) = spec.split_once('=').ok_or_else(|| {
                    Error::InvalidArgument("--env must use NAME=VALUE".to_string())
                })?;
                env.push((name.to_string(), value.to_string()));
            }
            "--" => {
                argv = iter.collect();
                break;
            }
            other if other.starts_with("--") => {
                return Err(Error::InvalidArgument(format!("unknown option {other}")));
            }
            other => {
                // First non-option token starts the command; the remainder are
                // its arguments and are taken verbatim.
                argv.push(other.to_string());
                argv.extend(iter.by_ref());
                break;
            }
        }
    }
    if argv.is_empty() {
        return Err(Error::InvalidArgument(
            "tb-sandbox exec requires a command (after options or `--`)".to_string(),
        ));
    }
    let mut command = GuestCommand::new(argv);
    command.env = env;
    command.cwd = cwd;
    Ok(command)
}

/// Run a guest command via the SDK, streaming stdout/stderr to the terminal and
/// exiting with the guest command's exit code.
fn run_exec(id: SandboxId, command: GuestCommand) -> Result<()> {
    let code = sdk_block_on(|sdk| async move {
        let mut process = sdk.exec(&id, command).await?;
        let mut exit_rx = process.exit;
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut exit = None;
        loop {
            tokio::select! {
                line = process.stdout.recv(), if stdout_open => match line {
                    Some(line) => println!("{line}"),
                    None => stdout_open = false,
                },
                line = process.stderr.recv(), if stderr_open => match line {
                    Some(line) => eprintln!("{line}"),
                    None => stderr_open = false,
                },
                status = &mut exit_rx, if exit.is_none() => {
                    exit = Some(status.map_err(|_| {
                        Error::Internal("guest process exit channel closed".to_string())
                    })??);
                }
            }
            if exit.is_some() && !stdout_open && !stderr_open {
                break;
            }
        }
        Ok(exit.and_then(|status| status.code).unwrap_or(0))
    })?;
    std::process::exit(code);
}

fn parse_sandbox_config(args: Vec<String>, shell_command: bool) -> Result<SandboxConfig> {
    let mut image = env::var_os("TB_SANDBOX_IMAGE").map(PathBuf::from);
    let mut name = None;
    // Density-aware default: 1 vCPU (see SandboxConfig::new / design doc #G).
    let mut cpus = 1u8;
    let mut memory_mib = 2048u32;
    let mut memory_max_mib: Option<u32> = None;
    let mut memory_min_mib = 512u32;
    let mut virtiofs_dax_size = 0u64;
    let mut workspace_mounts = Vec::new();
    let mut credential_profiles = Vec::new();
    let mut extra_credentials = Vec::new();
    let mut credential_env = Vec::new();
    let mut ephemeral = false;
    let mut yes = false;
    let mut log_level = LogLevel::Error;
    let mut initial_command = None;
    let mut network_proxy_enabled = true;
    let mut tls_mitm = true;
    let mut proxy_header_rules = Vec::new();
    let mut egress_allowlist: Vec<String> = Vec::new();
    // A backend chosen via env (or, later, a flag) is "explicit": it keeps the
    // historical behavior where enabling networking also enables the filtering
    // proxy by default. When nothing is specified we auto-select gvproxy purely
    // for its native low-latency port-forward path (see the post-parse block),
    // and there we deliberately leave the filtering proxy off.
    let env_backend = env::var_os("TB_SANDBOX_GVPROXY_SOCKET")
        .map(PathBuf::from)
        .map(|socket| NetworkBackend::Gvproxy { socket })
        .or_else(|| {
            env::var_os("TB_SANDBOX_PASST_SOCKET")
                .map(PathBuf::from)
                .map(|socket| NetworkBackend::Passt { socket })
        })
        .or_else(|| match env::var_os("TB_SANDBOX_GVPROXY") {
            Some(value) if value == "auto" || value.is_empty() => {
                Some(NetworkBackend::GvproxyAuto { executable: None })
            }
            Some(value) => Some(NetworkBackend::GvproxyAuto {
                executable: Some(PathBuf::from(value)),
            }),
            None => None,
        });
    let mut network_backend_explicit = env_backend.is_some();
    let mut network_backend = env_backend.unwrap_or(NetworkBackend::None);

    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--image" => image = Some(PathBuf::from(next_value(&mut iter, "--image")?)),
            "--name" => name = Some(next_value(&mut iter, "--name")?),
            "--cpus" => {
                cpus = next_value(&mut iter, "--cpus")?
                    .parse()
                    .map_err(|_| Error::InvalidArgument("--cpus must be an integer".to_string()))?
            }
            "--memory-mib" => {
                memory_mib = next_value(&mut iter, "--memory-mib")?
                    .parse()
                    .map_err(|_| {
                        Error::InvalidArgument("--memory-mib must be an integer".to_string())
                    })?
            }
            "--memory-max-mib" => {
                memory_max_mib = Some(next_value(&mut iter, "--memory-max-mib")?.parse().map_err(
                    |_| Error::InvalidArgument("--memory-max-mib must be an integer".to_string()),
                )?)
            }
            "--memory-min-mib" => {
                memory_min_mib =
                    next_value(&mut iter, "--memory-min-mib")?
                        .parse()
                        .map_err(|_| {
                            Error::InvalidArgument(
                                "--memory-min-mib must be an integer".to_string(),
                            )
                        })?
            }
            "--workspace" => {
                let path = PathBuf::from(next_value(&mut iter, "--workspace")?);
                workspace_mounts.push(WorkspaceMount::workspace(canonicalize_existing(&path)?));
            }
            "--mount" => {
                workspace_mounts.push(parse_mount(&next_value(&mut iter, "--mount")?)?);
            }
            "--shared-mount" => {
                workspace_mounts.push(parse_shared_mount(&next_value(
                    &mut iter,
                    "--shared-mount",
                )?)?);
            }
            "--virtiofs-dax-size" => {
                virtiofs_dax_size =
                    parse_size_bytes(&next_value(&mut iter, "--virtiofs-dax-size")?)?;
            }
            "--credentials" => {
                let value = next_value(&mut iter, "--credentials")?;
                if value == "none" {
                    credential_profiles.clear();
                } else {
                    for name in value.split(',').filter(|name| !name.is_empty()) {
                        let profile = CredentialProfile::parse(name).ok_or_else(|| {
                            Error::InvalidArgument(format!("unknown credential profile {name}"))
                        })?;
                        if !credential_profiles.contains(&profile) {
                            credential_profiles.push(profile);
                        }
                    }
                }
            }
            "--credential-path" => {
                extra_credentials.push(parse_credential_path(&next_value(
                    &mut iter,
                    "--credential-path",
                )?)?);
            }
            "--credential-env" => {
                credential_env.push(next_value(&mut iter, "--credential-env")?);
            }
            "--ephemeral" => ephemeral = true,
            "--initial-command" => {
                initial_command = Some(next_value(&mut iter, "--initial-command")?)
            }
            "--no-network-proxy" => network_proxy_enabled = false,
            "--no-tls-mitm" => tls_mitm = false,
            "--gvproxy" => {
                if network_backend.is_enabled() {
                    return Err(Error::InvalidArgument(
                        "only one network backend may be configured".to_string(),
                    ));
                }
                network_backend = NetworkBackend::GvproxyAuto { executable: None };
                network_backend_explicit = true;
            }
            "--gvproxy-bin" => {
                if network_backend.is_enabled() {
                    return Err(Error::InvalidArgument(
                        "only one network backend may be configured".to_string(),
                    ));
                }
                network_backend = NetworkBackend::GvproxyAuto {
                    executable: Some(PathBuf::from(next_value(&mut iter, "--gvproxy-bin")?)),
                };
                network_backend_explicit = true;
            }
            "--passt-socket" => {
                if network_backend.is_enabled() {
                    return Err(Error::InvalidArgument(
                        "only one network backend may be configured".to_string(),
                    ));
                }
                network_backend = NetworkBackend::Passt {
                    socket: PathBuf::from(next_value(&mut iter, "--passt-socket")?),
                };
                network_backend_explicit = true;
            }
            "--gvproxy-socket" => {
                if network_backend.is_enabled() {
                    return Err(Error::InvalidArgument(
                        "only one network backend may be configured".to_string(),
                    ));
                }
                network_backend = NetworkBackend::Gvproxy {
                    socket: PathBuf::from(next_value(&mut iter, "--gvproxy-socket")?),
                };
                network_backend_explicit = true;
            }
            "--proxy-header" => {
                proxy_header_rules.push(parse_proxy_header_rule(&next_value(
                    &mut iter,
                    "--proxy-header",
                )?)?);
            }
            "--proxy-header-env" => {
                let spec = next_value(&mut iter, "--proxy-header-env")?;
                proxy_header_rules.push(parse_proxy_header_env_rule(&spec)?);
            }
            "--egress-allow" => {
                // Repeatable; accumulates patterns. Supports comma-separated
                // shorthand so a single flag can carry the whole set.
                let value = next_value(&mut iter, "--egress-allow")?;
                for pattern in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    egress_allowlist.push(pattern.to_string());
                }
            }
            "--yes" => yes = true,
            "--log-level" => {
                let value = next_value(&mut iter, "--log-level")?;
                log_level = LogLevel::parse(&value)
                    .ok_or_else(|| Error::InvalidArgument(format!("unknown log level {value}")))?;
            }
            "--" => {
                let rest: Vec<String> = iter.collect();
                if !rest.is_empty() {
                    initial_command = Some(rest.join(" "));
                }
                break;
            }
            other if other.starts_with("--") => {
                return Err(Error::InvalidArgument(format!("unknown option {other}")));
            }
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected positional argument {other}"
                )));
            }
        }
    }

    let image = image.ok_or_else(|| {
        Error::InvalidArgument("--image is required unless TB_SANDBOX_IMAGE is set".to_string())
    })?;
    if shell_command && workspace_mounts.is_empty() {
        let cwd = env::current_dir().map_err(Error::io)?;
        workspace_mounts.push(WorkspaceMount::workspace(cwd));
    }

    // The ceiling defaults to the initial memory (resizing disabled).
    let memory_max_mib = memory_max_mib.unwrap_or(memory_mib);
    if memory_max_mib < memory_mib {
        return Err(Error::InvalidArgument(
            "--memory-max-mib must be >= --memory-mib".to_string(),
        ));
    }
    if memory_min_mib > memory_mib {
        return Err(Error::InvalidArgument(
            "--memory-min-mib must be <= --memory-mib".to_string(),
        ));
    }

    let mut config = SandboxConfig::new(AgentImageRef::new(image));
    config.name = name;
    config.cpus = cpus;
    config.memory_mib = memory_mib;
    config.memory_max_mib = memory_max_mib;
    config.memory_min_mib = memory_min_mib;
    config.virtiofs_dax_size = virtiofs_dax_size;
    config.workspace_mounts = workspace_mounts;
    config.credential_profiles = credential_profiles;
    config.extra_credentials = extra_credentials;
    config.credential_env = credential_env;
    config.ephemeral = ephemeral;
    config.yes = yes;
    config.log_level = log_level;
    config.shell.initial_command = initial_command;
    // Auto-select gvproxy when the user didn't configure any backend and the
    // host can run it. This is what gives `forward_port` (and therefore VNC/CDP)
    // the native, low-latency transport instead of the polled file relay — the
    // file relay made headed-browser VNC unusable. Unlike an explicitly chosen
    // backend, the auto path leaves the filtering/MITM proxy OFF: we only want
    // fast forwarding here, not a behavior change to the guest's egress.
    // Opt out with TB_SANDBOX_NO_NETWORK=1 to keep the built-in `None` backend.
    if !network_backend_explicit
        && matches!(network_backend, NetworkBackend::None)
        && env::var_os("TB_SANDBOX_NO_NETWORK").is_none()
        && crate::runtime::gvproxy_available()
    {
        network_backend = NetworkBackend::GvproxyAuto { executable: None };
        network_proxy_enabled = false;
    }

    config.network_proxy.enabled = network_proxy_enabled;
    config.network_proxy.tls_mitm = tls_mitm;
    config.network_proxy.header_rules = proxy_header_rules;
    config.network_proxy.egress_allowlist = egress_allowlist;
    config.network_backend = network_backend;
    Ok(config)
}

fn required_id(value: Option<String>, command: &str) -> Result<SandboxId> {
    let value = value.ok_or_else(|| {
        Error::InvalidArgument(format!("tb-sandbox {command} requires SANDBOX_ID"))
    })?;
    SandboxId::new(value)
}

fn next_value(
    iter: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    option: &str,
) -> Result<String> {
    iter.next()
        .ok_or_else(|| Error::InvalidArgument(format!("{option} requires a value")))
}

fn canonicalize_existing(path: &PathBuf) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|err| Error::io_path(path, err))
}

fn parse_mount(value: &str) -> Result<WorkspaceMount> {
    let (tag, rest) = value.split_once('=').ok_or_else(|| {
        Error::InvalidArgument("--mount must use TAG=HOST_PATH[:GUEST_PATH]".to_string())
    })?;
    if tag.is_empty() {
        return Err(Error::InvalidArgument(
            "mount tag must not be empty".to_string(),
        ));
    }
    let (host, guest) = match rest.rsplit_once(':') {
        Some((host, guest)) if guest.starts_with('/') => (host, guest),
        _ => (rest, "/workspace"),
    };
    Ok(WorkspaceMount {
        tag: tag.to_string(),
        host_path: canonicalize_existing(&PathBuf::from(host))?,
        guest_path: guest.to_string(),
        read_only: false,
    })
}

/// Parse a read-only shared-asset mount: `TAG=HOST_PATH[:GUEST_PATH]`. Unlike
/// `--mount`, the guest sees this read-only, so many sandboxes can point at the
/// same host directory (toolchains, package caches, model assets) and share its
/// pages without one VM mutating another's view. Guest path defaults to
/// `/mnt/shared/<TAG>` when omitted.
fn parse_shared_mount(value: &str) -> Result<WorkspaceMount> {
    let (tag, rest) = value.split_once('=').ok_or_else(|| {
        Error::InvalidArgument("--shared-mount must use TAG=HOST_PATH[:GUEST_PATH]".to_string())
    })?;
    if tag.is_empty() {
        return Err(Error::InvalidArgument(
            "shared mount tag must not be empty".to_string(),
        ));
    }
    let (host, guest) = match rest.rsplit_once(':') {
        Some((host, guest)) if guest.starts_with('/') => (host.to_string(), guest.to_string()),
        _ => (rest.to_string(), format!("/mnt/shared/{tag}")),
    };
    Ok(WorkspaceMount {
        tag: tag.to_string(),
        host_path: canonicalize_existing(&PathBuf::from(host))?,
        guest_path: guest,
        read_only: true,
    })
}

/// Parse a byte size that optionally ends in a K/M/G (base-1024) suffix, e.g.
/// `1073741824`, `1024M`, or `1G`.
fn parse_size_bytes(value: &str) -> Result<u64> {
    let trimmed = value.trim();
    let (number, multiplier) = match trimmed.chars().last() {
        Some('K' | 'k') => (&trimmed[..trimmed.len() - 1], 1024u64),
        Some('M' | 'm') => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
        _ => (trimmed, 1),
    };
    let base: u64 = number.trim().parse().map_err(|_| {
        Error::InvalidArgument(format!(
            "invalid byte size {value}; use bytes or a K/M/G suffix"
        ))
    })?;
    base.checked_mul(multiplier)
        .ok_or_else(|| Error::InvalidArgument(format!("byte size {value} overflows")))
}

fn parse_credential_path(value: &str) -> Result<CredentialSpec> {
    let (host, guest) = value.split_once(':').ok_or_else(|| {
        Error::InvalidArgument(
            "--credential-path must use HOST_PATH:GUEST_RELATIVE_PATH".to_string(),
        )
    })?;
    if guest.starts_with('/') {
        return Err(Error::InvalidArgument(
            "credential guest path must be relative to /home/agent".to_string(),
        ));
    }
    Ok(CredentialSpec {
        host_path: PathBuf::from(host),
        guest_relative_path: PathBuf::from(guest),
    })
}

fn parse_proxy_header_rule(value: &str) -> Result<ProxyHeaderRule> {
    let (target, header_value) = value.split_once('=').ok_or_else(|| {
        Error::InvalidArgument("--proxy-header must use HOST:HEADER=VALUE".to_string())
    })?;
    let (host, header) = target.split_once(':').ok_or_else(|| {
        Error::InvalidArgument("--proxy-header must use HOST:HEADER=VALUE".to_string())
    })?;
    validate_proxy_rule_parts(host, header)?;
    Ok(ProxyHeaderRule {
        host: host.to_string(),
        header: header.to_string(),
        value: header_value.to_string(),
        value_from_env: false,
    })
}

fn parse_proxy_header_env_rule(value: &str) -> Result<ProxyHeaderRule> {
    let (target, env_name) = value.split_once('=').ok_or_else(|| {
        Error::InvalidArgument("--proxy-header-env must use HOST:HEADER=ENV_NAME".to_string())
    })?;
    let (host, header) = target.split_once(':').ok_or_else(|| {
        Error::InvalidArgument("--proxy-header-env must use HOST:HEADER=ENV_NAME".to_string())
    })?;
    validate_proxy_rule_parts(host, header)?;
    Ok(ProxyHeaderRule {
        host: host.to_string(),
        header: header.to_string(),
        value: env_name.to_string(),
        value_from_env: true,
    })
}

fn validate_proxy_rule_parts(host: &str, header: &str) -> Result<()> {
    if host.is_empty() || header.is_empty() {
        return Err(Error::InvalidArgument(
            "proxy header host and header name must be non-empty".to_string(),
        ));
    }
    if header
        .bytes()
        .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'-'))
    {
        return Err(Error::InvalidArgument(format!(
            "invalid proxy header name {header}"
        )));
    }
    Ok(())
}

fn print_help(program: &str) {
    println!(
        "\
{program} - boot libkrun-backed agent sandboxes

Usage:
  {program} shell [OPTIONS] [-- INITIAL_COMMAND...]
  {program} create [OPTIONS]
  {program} start SANDBOX_ID
  {program} stop SANDBOX_ID
  {program} exec SANDBOX_ID [--cwd DIR] [--env NAME=VALUE]... -- CMD [ARGS...]
  {program} list
  {program} inspect SANDBOX_ID
  {program} resize SANDBOX_ID --memory-mib N
  {program} delete SANDBOX_ID
  {program} server [--bind ADDR] [--no-auth] [--token TOKEN]
  {program} preview [--bind ADDR] [--no-auth] [SANDBOX_ID GUEST_PORT]
  {program} snapshot SANDBOX_ID [--name NAME] [--capture-memory]
                              [--no-quiesce] [--no-freeze] [--quiesce-timeout SECS]
  {program} snapshots
  {program} snapshots prune [--delete]
  {program} snapshot-inspect SNAPSHOT_ID
  {program} snapshot-delete SNAPSHOT_ID
  {program} restore SNAPSHOT_ID [--name NEW_ID] [--image-path DIR] [--workspace PATH]
                              [--no-credentials] [--cpus N] [--memory-mib N]
                              [--memory-max-mib N] [--memory-min-mib N] [--ephemeral]
  {program} fork SANDBOX_ID [--name NEW_ID] [--workspace PATH] [--no-credentials] [--ephemeral]
  {program} browser start SANDBOX_ID [--headless|--headed] [--cdp-port N] [--vnc-port N] [--profile DIR]
  {program} browser view SANDBOX_ID [--cdp-port N] [--vnc-port N] [--no-open]
  {program} browser stream SANDBOX_ID [--headless|--headed] [--cdp-port N] [--listen ADDR] [--profile DIR] [--no-open]
  {program} browser status SANDBOX_ID
  {program} browser stop SANDBOX_ID

Common options:
  --image PATH_OR_REF
  --name NAME
  --cpus N                  vCPUs (default 1)
  --memory-mib N            Initial guest memory (MiB)
  --memory-max-mib N        Resize ceiling (MiB); enables runtime resizing when > --memory-mib
  --memory-min-mib N        Floor for runtime memory reclaim (MiB, default 512)
  --workspace PATH
  --mount TAG=HOST_PATH[:GUEST_PATH]          Read-write workspace mount
  --shared-mount TAG=HOST_PATH[:GUEST_PATH]   Read-only mount shared across sandboxes
  --virtiofs-dax-size BYTES   DAX window (e.g. 1G) so shared mounts share host page cache
  --credentials none|claude|codex|git|PROFILE[,PROFILE...]
  --credential-path HOST_PATH:GUEST_RELATIVE_PATH
  --credential-env NAME
  --passt-socket PATH
  --gvproxy                 Force the auto-managed gvproxy backend (also turns on
                            the filtering proxy by default). On macOS gvproxy is
                            already auto-selected when installed, purely for its
                            low-latency port forwards (VNC/CDP); that auto path
                            leaves the filtering proxy OFF. Set TB_SANDBOX_NO_NETWORK=1
                            to fall back to the built-in `None` backend (whose
                            forwards use the slow file relay).
  --gvproxy-bin PATH
  --gvproxy-socket PATH
  --no-network-proxy
  --no-tls-mitm
  --proxy-header HOST:HEADER=VALUE
  --proxy-header-env HOST:HEADER=ENV_NAME
  --egress-allow PATTERN[,PATTERN...]  Restrict outbound HTTP(S) to listed hosts
                                       (exact hostname or '*.suffix.example.com');
                                       repeatable. Default: no restriction.
  --ephemeral
  --initial-command COMMAND
  --yes
  --log-level error|warn|info|debug|trace

Host memory admission control (env):
  TB_SANDBOX_MEM_BUDGET_MIB N      Host memory budget; 0 disables (default 80% of RAM)
  TB_SANDBOX_ADMISSION reject|queue  Behavior when the budget is full (default reject)
  TB_SANDBOX_ADMISSION_TIMEOUT_SECS N  Max wait under `queue` (default 300)

API server / client mode:
  {program} server starts a localhost HTTP API over the same runtime. With
  TB_SANDBOX_API_URL set (and TB_SANDBOX_API_TOKEN, or the persisted token in
  the state dir), create/list/inspect/start/stop/exec/delete are dispatched to
  that server instead of running in-process.
"
    );
}
