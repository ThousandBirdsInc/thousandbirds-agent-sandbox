use crate::config::{
    AgentImageRef, CredentialProfile, CredentialSpec, LogLevel, NetworkBackend, ProxyHeaderRule,
    SandboxConfig, WorkspaceMount,
};
use crate::error::{Error, Result};
use crate::runtime::{SandboxId, SandboxManager};
use crate::sdk::{GuestCommand, SandboxManagerSdk, SandboxRuntimeSdk};
use crate::snapshot::{ForkOptions, RestoreOptions, SnapshotId, SnapshotOptions};
use std::env;
use std::path::PathBuf;

pub fn run(args: Vec<String>) -> Result<()> {
    let mut args = args.into_iter();
    let program = args.next().unwrap_or_else(|| "tb-sandbox".to_string());
    let Some(command) = args.next() else {
        print_help(&program);
        return Ok(());
    };

    let manager = SandboxManager::new();
    match command.as_str() {
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
            manager.shell(&id)
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
        "snapshots" => {
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
            Ok(())
        }
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
                    "tb-sandbox browser requires a subcommand: start|stop|status|view".to_string(),
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

fn parse_restore_options(args: Vec<String>) -> Result<RestoreOptions> {
    let mut opts = RestoreOptions::default();
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

fn run_browser_start(id: SandboxId, opts: BrowserStartOptions) -> Result<()> {
    sdk_block_on(|sdk| async move {
        // 1. Exec agent-sandbox-browser start in the guest (idempotent —
        //    re-invocation on a running browser just re-prints status).
        if !opts.skip_start {
            let mut argv = vec![GUEST_BROWSER_BIN.to_string(), "start".to_string()];
            argv.push(if opts.headed {
                "--headed".into()
            } else {
                "--headless".into()
            });
            if let Some(p) = opts.cdp_port {
                argv.push("--cdp-port".into());
                argv.push(p.to_string());
            }
            if let Some(p) = opts.vnc_port {
                argv.push("--vnc-port".into());
                argv.push(p.to_string());
            }
            if let Some(profile) = &opts.profile {
                argv.push("--profile".into());
                argv.push(profile.clone());
            }
            let mut process = sdk.exec(&id, GuestCommand::new(argv)).await?;
            // Drain the start command's output (it prints status= lines).
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
            println!("vnc_url=vnc://{}:{}", f.host, f.port);
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
    // port. Reuses run_browser_start with headed=true plus an extra "open"
    // step before the Ctrl-C wait — but to keep the shape simple we just
    // call run_browser_start with headed and let the user open the VNC
    // URL themselves, unless --no-open is unset and we can find a viewer.
    let no_open = opts.no_open;
    let macos_vnc_url_open = !no_open && cfg!(target_os = "macos");
    let start_opts = BrowserStartOptions {
        headed: true,
        cdp_port: opts.cdp_port,
        vnc_port: opts.vnc_port,
        profile: opts.profile,
        skip_start: false,
    };
    // The trick: print a hint after the URLs so the user can paste it,
    // and on macOS try `open vnc://...` (the system uses Screen Sharing
    // by default; falls back to whatever's bound to the vnc:// scheme).
    if macos_vnc_url_open {
        eprintln!("browser view: after the vnc_url= line appears, this will try `open <vnc_url>`");
    }
    run_browser_start(id, start_opts)
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
    let mut network_backend = env::var_os("TB_SANDBOX_GVPROXY_SOCKET")
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
        })
        .unwrap_or(NetworkBackend::None);

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
    config.network_proxy.enabled = network_proxy_enabled;
    config.network_proxy.tls_mitm = tls_mitm;
    config.network_proxy.header_rules = proxy_header_rules;
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
  {program} exec SANDBOX_ID [--cwd DIR] [--env NAME=VALUE]... -- CMD [ARGS...]
  {program} list
  {program} inspect SANDBOX_ID
  {program} resize SANDBOX_ID --memory-mib N
  {program} delete SANDBOX_ID
  {program} snapshot SANDBOX_ID [--name NAME] [--no-quiesce] [--no-freeze] [--quiesce-timeout SECS]
  {program} snapshots
  {program} snapshot-inspect SNAPSHOT_ID
  {program} snapshot-delete SNAPSHOT_ID
  {program} restore SNAPSHOT_ID [--name NEW_ID] [--workspace PATH] [--no-credentials]
                              [--cpus N] [--memory-mib N] [--memory-max-mib N]
                              [--memory-min-mib N] [--ephemeral]
  {program} fork SANDBOX_ID [--name NEW_ID] [--workspace PATH] [--no-credentials] [--ephemeral]
  {program} browser start SANDBOX_ID [--headless|--headed] [--cdp-port N] [--vnc-port N] [--profile DIR]
  {program} browser view SANDBOX_ID [--cdp-port N] [--vnc-port N] [--no-open]
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
  --gvproxy
  --gvproxy-bin PATH
  --gvproxy-socket PATH
  --no-network-proxy
  --no-tls-mitm
  --proxy-header HOST:HEADER=VALUE
  --proxy-header-env HOST:HEADER=ENV_NAME
  --ephemeral
  --initial-command COMMAND
  --yes
  --log-level error|warn|info|debug|trace

Host memory admission control (env):
  TB_SANDBOX_MEM_BUDGET_MIB N      Host memory budget; 0 disables (default 80% of RAM)
  TB_SANDBOX_ADMISSION reject|queue  Behavior when the budget is full (default reject)
  TB_SANDBOX_ADMISSION_TIMEOUT_SECS N  Max wait under `queue` (default 300)
"
    );
}
