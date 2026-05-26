use crate::config::{LogLevel, NetworkBackend, WorkspaceMount};
use crate::error::{Error, Result};
use crate::image::AgentImage;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

pub struct LaunchConfig<'a> {
    pub image: &'a AgentImage,
    pub root_disk: &'a Path,
    pub cpus: u8,
    /// Initial guest usable memory in MiB.
    pub memory_mib: u32,
    /// Resize ceiling (boot RAM) in MiB. Equal to `memory_mib` when resizing
    /// is disabled.
    pub memory_max_mib: u32,
    /// Floor for runtime memory reclaim in MiB.
    pub memory_min_mib: u32,
    /// Unix socket the VMM should serve for runtime resize commands. Present
    /// only when runtime resizing is enabled.
    pub control_socket: Option<&'a Path>,
    pub workspace_mounts: &'a [WorkspaceMount],
    pub credentials_mount: Option<&'a Path>,
    pub runtime_config_mount: &'a Path,
    pub runtime_control_mount: &'a Path,
    pub network_backend: &'a NetworkBackend,
    pub log_level: LogLevel,
}

pub fn launch(config: LaunchConfig<'_>) -> Result<ExitStatus> {
    let launcher = find_os_mode_launcher()?;
    let mut command = launch_command_with_launcher(config, &launcher)?;
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = command.status().map_err(Error::io)?;
    if status.success() {
        Ok(status)
    } else {
        Err(Error::ProcessFailed {
            command: launcher.display().to_string(),
            status,
        })
    }
}

pub fn launch_command(config: LaunchConfig<'_>) -> Result<Command> {
    let launcher = find_os_mode_launcher()?;
    launch_command_with_launcher(config, &launcher)
}

fn launch_command_with_launcher(config: LaunchConfig<'_>, launcher: &Path) -> Result<Command> {
    let mut command = Command::new(&launcher);
    if let Some(dylib_path) = local_libkrun_dylib_path()? {
        command.env(
            "DYLD_LIBRARY_PATH",
            prepend_env_path("DYLD_LIBRARY_PATH", &dylib_path),
        );
    }
    command
        .arg("--kernel")
        .arg(&config.image.kernel)
        .arg("--kernel-format")
        .arg(config.image.kernel_format.to_string())
        .arg("--cpus")
        .arg(config.cpus.to_string())
        // Boot with the ceiling; the balloon constrains usable memory down to
        // the initial size when resizing is enabled.
        .arg("--memory-mib")
        .arg(config.memory_max_mib.max(config.memory_mib).to_string())
        .arg("--serial-pty")
        .arg("--root-disk")
        .arg(config.root_disk)
        .arg("--root-device")
        .arg(&config.image.root_device)
        .arg("--root-fstype")
        .arg(&config.image.root_fstype)
        .arg("--guest-init")
        .arg(&config.image.init)
        .arg("--console")
        .arg(&config.image.console);

    if let Some(initramfs) = &config.image.initramfs {
        command.arg("--initramfs").arg(initramfs);
    }
    // Enable runtime memory resizing only when a control socket is provided and
    // there is headroom (ceiling > initial) for the balloon to grow into.
    if let Some(control_socket) = config.control_socket {
        if config.memory_max_mib > config.memory_mib {
            command
                .arg("--balloon-initial-mib")
                .arg(config.memory_mib.to_string())
                .arg("--balloon-min-mib")
                .arg(config.memory_min_mib.to_string())
                .arg("--control-socket")
                .arg(control_socket);
        }
    }
    if !config.image.root_options.is_empty() {
        command
            .arg("--root-options")
            .arg(&config.image.root_options);
    }
    match config.network_backend {
        NetworkBackend::None => {}
        NetworkBackend::Passt { socket } => {
            command.arg("--passt-socket").arg(socket);
        }
        NetworkBackend::Gvproxy { socket } => {
            command
                .arg("--gvproxy-socket")
                .arg(socket)
                .arg("--gvproxy-vfkit-magic");
        }
        NetworkBackend::GvproxyAuto { .. } => {
            return Err(Error::InvalidArgument(
                "gvproxy auto backend must be resolved before launch".to_string(),
            ));
        }
    }
    command
        .arg("--kernel-cmdline")
        .arg(shell_kernel_cmdline(config.image.kernel_cmdline.as_deref()));

    for mount in config.workspace_mounts {
        command.arg(if mount.read_only {
            "--virtiofs-ro"
        } else {
            "--virtiofs"
        });
        command.arg(format!("{}={}", mount.tag, mount.host_path.display()));
    }
    command.arg("--virtiofs-ro").arg(format!(
        "agent-config={}",
        config.runtime_config_mount.display()
    ));
    command.arg("--virtiofs").arg(format!(
        "agent-control={}",
        config.runtime_control_mount.display()
    ));
    if let Some(credentials_mount) = config.credentials_mount {
        command
            .arg("--virtiofs-ro")
            .arg(format!("agent-credentials={}", credentials_mount.display()));
    }

    if !matches!(config.log_level, LogLevel::Error) {
        eprintln!(
            "note: log level {:?} is recorded by tb-sandbox but examples/os_mode currently sets libkrun logs to error",
            config.log_level
        );
    }
    Ok(command)
}

fn shell_kernel_cmdline(image_cmdline: Option<&str>) -> String {
    const SHELL_UNIT: &str = "systemd.unit=agent-sandbox-shell.service";
    match image_cmdline {
        Some(cmdline) if cmdline.split_whitespace().any(|token| token == SHELL_UNIT) => {
            cmdline.to_string()
        }
        Some(cmdline) if !cmdline.trim().is_empty() => format!("{} {}", cmdline.trim(), SHELL_UNIT),
        _ => SHELL_UNIT.to_string(),
    }
}

fn find_os_mode_launcher() -> Result<PathBuf> {
    if let Some(path) = env::var_os("TB_SANDBOX_OS_MODE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(Error::NotFound(format!(
            "TB_SANDBOX_OS_MODE points to {}, but it is not a file",
            path.display()
        )));
    }

    let repo_relative = PathBuf::from("../thousandbirds-libkrun/examples/os_mode");
    if repo_relative.is_file() {
        return Ok(repo_relative);
    }
    let crate_relative =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../thousandbirds-libkrun/examples/os_mode");
    if crate_relative.is_file() {
        return Ok(crate_relative);
    }

    Err(Error::NotFound(
        "libkrun os_mode launcher not found; build ../thousandbirds-libkrun/examples/os_mode or set TB_SANDBOX_OS_MODE".to_string(),
    ))
}

fn local_libkrun_dylib_path() -> Result<Option<PathBuf>> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    if let Some(path) = env::var_os("TB_SANDBOX_LIBKRUN_DIR") {
        return Ok(Some(PathBuf::from(path)));
    }

    for dir in [
        PathBuf::from("../thousandbirds-libkrun/target/debug"),
        PathBuf::from("../thousandbirds-libkrun/target/release"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../thousandbirds-libkrun/target/debug"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../thousandbirds-libkrun/target/release"),
        PathBuf::from("/opt/homebrew/lib"),
        PathBuf::from("/usr/local/lib"),
    ] {
        if dir.join("libkrun.1.dylib").is_file() {
            return Ok(Some(dir));
        }
        let libkrun = dir.join("libkrun.dylib");
        if libkrun.is_file() {
            return Ok(Some(prepare_compat_dylib_dir(&libkrun)?));
        }
    }

    Ok(None)
}

fn prepare_compat_dylib_dir(libkrun: &Path) -> Result<PathBuf> {
    let dir = env::temp_dir().join("thousandbirds-agent-sandbox-dylib");
    fs::create_dir_all(&dir).map_err(|err| Error::io_path(&dir, err))?;
    let link = dir.join("libkrun.1.dylib");
    if link.exists() {
        fs::remove_file(&link).map_err(|err| Error::io_path(&link, err))?;
    }
    let target = fs::canonicalize(libkrun).map_err(|err| Error::io_path(libkrun, err))?;
    make_symlink_or_copy(&target, &link)?;
    Ok(dir)
}

#[cfg(unix)]
fn make_symlink_or_copy(target: &Path, link: &Path) -> Result<()> {
    unix_fs::symlink(target, link)
        .or_else(|_| fs::copy(target, link).map(|_| ()))
        .map_err(|err| Error::io_path(link, err))
}

#[cfg(not(unix))]
fn make_symlink_or_copy(target: &Path, link: &Path) -> Result<()> {
    fs::copy(target, link)
        .map(|_| ())
        .map_err(|err| Error::io_path(link, err))
}

fn prepend_env_path(name: &str, path: &Path) -> String {
    match env::var_os(name) {
        Some(existing) if !existing.is_empty() => {
            format!("{}:{}", path.display(), PathBuf::from(existing).display())
        }
        _ => path.display().to_string(),
    }
}
