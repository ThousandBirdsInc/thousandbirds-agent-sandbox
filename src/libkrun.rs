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
    /// virtio-fs DAX window size in bytes (0 = disabled).
    pub virtiofs_dax_size: u64,
    pub workspace_mounts: &'a [WorkspaceMount],
    pub credentials_mount: Option<&'a Path>,
    pub runtime_config_mount: &'a Path,
    pub runtime_control_mount: &'a Path,
    pub network_backend: &'a NetworkBackend,
    pub log_level: LogLevel,
    /// Path to a snapshot directory to restore from (gap plan Phase 3). When
    /// `Some`, the launcher receives `--restore PATH` and is expected to
    /// rebuild the VM's memory + vCPU + device state from that snapshot
    /// rather than cold-booting from the kernel/initramfs/root disk.
    ///
    /// Today no production caller sets this — the `--restore` flag is added
    /// by Phase D of the libkrun-side
    /// `thousandbirds-libkrun/design_docs/snapshot_restore_implementation.md`
    /// plan. The wrapper plumbing is in place so that when libkrun lands the
    /// flag, integrating it here is a single `SandboxManager::restore`
    /// branch and no `LaunchConfig` shape change.
    pub restore_from: Option<&'a Path>,
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

    // Phase 3 (memory snapshot restore) path. When `restore_from` is set we
    // pass `--restore PATH` and the matching `--root-disk` (libkrun's
    // restore wires the disk into the restored virtio-blk). The kernel /
    // initramfs / boot config args become inert under restore so we omit
    // them — this keeps the os_mode launcher's mutually-exclusive arg
    // contract clean once it lands the flag.
    if let Some(snapshot_path) = config.restore_from {
        command
            .arg("--restore")
            .arg(snapshot_path)
            .arg("--cpus")
            .arg(config.cpus.to_string())
            .arg("--memory-mib")
            .arg(config.memory_max_mib.max(config.memory_mib).to_string())
            .args(serial_pty_args())
            .arg("--root-disk")
            .arg(config.root_disk)
            .arg("--console")
            .arg(&config.image.console);
        append_common_launcher_args(&mut command, &config)?;
        return Ok(command);
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
        .args(serial_pty_args())
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
    if !config.image.root_options.is_empty() {
        command
            .arg("--root-options")
            .arg(&config.image.root_options);
    }
    command
        .arg("--kernel-cmdline")
        .arg(shell_kernel_cmdline(config.image.kernel_cmdline.as_deref()));

    append_common_launcher_args(&mut command, &config)?;

    Ok(command)
}

/// Append the launcher arguments that are common to both the cold-boot path
/// and the restore path: balloon control socket (for runtime memory resize),
/// virtio-fs DAX window, network backend, all virtio-fs mounts (workspace,
/// agent-config, agent-control, credentials), and the log-level note.
fn append_common_launcher_args(command: &mut Command, config: &LaunchConfig<'_>) -> Result<()> {
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
    // Open a virtio-fs DAX window so shared read-only mounts are served from the
    // host page cache (shared across VMs) instead of copied per-guest.
    if config.virtiofs_dax_size > 0 {
        command
            .arg("--virtiofs-dax-size")
            .arg(config.virtiofs_dax_size.to_string());
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
    Ok(())
}

/// libkrun exposes a PTY-backed serial console only on the macOS/HVF path
/// (the guest sees it as ttyAMA0). On Linux/KVM the guest console is the
/// virtio-console (hvc0) wired straight to the launcher's stdio, and there is
/// no serial-pty device — so the flag is macOS-only. Returned as a slice so it
/// drops cleanly out of the launcher arg chain on Linux.
fn serial_pty_args() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["--serial-pty"]
    } else {
        &[]
    }
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
