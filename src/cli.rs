use crate::config::{
    AgentImageRef, CredentialProfile, CredentialSpec, LogLevel, NetworkBackend, ProxyHeaderRule,
    SandboxConfig, WorkspaceMount,
};
use crate::error::{Error, Result};
use crate::runtime::{SandboxId, SandboxManager};
use crate::sdk::{GuestCommand, SandboxManagerSdk, SandboxRuntimeSdk};
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
                println!(
                    "{}\t{}\t{}",
                    summary.id,
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
        "help" | "--help" | "-h" => {
            print_help(&program);
            Ok(())
        }
        other => Err(Error::InvalidArgument(format!("unknown command {other}"))),
    }
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
    let mut cpus = 2u8;
    let mut memory_mib = 2048u32;
    let mut memory_max_mib: Option<u32> = None;
    let mut memory_min_mib = 512u32;
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

Common options:
  --image PATH_OR_REF
  --name NAME
  --cpus N
  --memory-mib N            Initial guest memory (MiB)
  --memory-max-mib N        Resize ceiling (MiB); enables runtime resizing when > --memory-mib
  --memory-min-mib N        Floor for runtime memory reclaim (MiB, default 512)
  --workspace PATH
  --mount TAG=HOST_PATH[:GUEST_PATH]
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
"
    );
}
