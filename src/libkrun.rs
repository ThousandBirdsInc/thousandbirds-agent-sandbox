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
    /// Guest vsock ports to bridge to host unix sockets: `(guest_port,
    /// host_socket_path)`. Emitted as `--vsock-port PORT=PATH`. Used for the
    /// in-guest `agent-sandbox-daemon` control channel; empty for boots that
    /// don't need it.
    pub vsock_ports: &'a [(u32, PathBuf)],
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
    let launcher = find_runner_launcher()?;
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
    let launcher = find_runner_launcher()?;
    launch_command_with_launcher(config, &launcher)
}

fn launch_command_with_launcher(config: LaunchConfig<'_>, launcher: &Path) -> Result<Command> {
    let mut command = Command::new(&launcher);
    if is_legacy_os_mode_launcher(launcher) {
        if let Some(dylib_path) = local_libkrun_dylib_path()? {
            command.env(
                "DYLD_LIBRARY_PATH",
                prepend_env_path("DYLD_LIBRARY_PATH", &dylib_path),
            );
        }
    }

    // Phase 3 (memory snapshot restore) path. When `restore_from` is set we
    // pass `--restore PATH` and the matching `--root-disk` (libkrun's
    // restore wires the disk into the restored virtio-blk). The kernel /
    // initramfs / boot config args become inert under restore so we omit
    // them because the runner treats restore as mutually exclusive with
    // cold-boot kernel configuration.
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
    // Bridge guest vsock ports to host unix sockets (agent-sandbox-daemon
    // control channel). The host connects to PATH; libkrun routes it to the
    // guest daemon listening on the vsock port.
    for (port, path) in config.vsock_ports {
        command
            .arg("--vsock-port")
            .arg(format!("{}={}", port, path.display()));
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
            "note: log level {:?} is recorded by tb-sandbox but tb-sandbox-runner currently sets libkrun logs to error",
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

fn find_runner_launcher() -> Result<PathBuf> {
    if let Some(path) = env::var_os("TB_SANDBOX_RUNNER") {
        let path = PathBuf::from(path);
        if path.is_file() {
            ensure_runner_signed(&path);
            return Ok(path);
        }
        return Err(Error::NotFound(format!(
            "TB_SANDBOX_RUNNER points to {}, but it is not a file",
            path.display()
        )));
    }

    // One-release compatibility escape hatch for the previous C launcher.
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

    // Cargo-built runner: this is the primary integration path. It links the
    // libkrun Rust crate directly and preserves the child-process boundary the
    // runtime uses for status and cancellation.
    for dir in bundled_dirs() {
        let candidate = dir.join("tb-sandbox-runner");
        if candidate.is_file() {
            ensure_runner_signed(&candidate);
            return Ok(candidate);
        }
    }

    Err(Error::NotFound(
        "tb-sandbox-runner not found; run `make setup`, or set TB_SANDBOX_RUNNER. For the legacy C launcher, set TB_SANDBOX_OS_MODE".to_string(),
    ))
}

/// Ensure the cargo-built runner carries the `com.apple.security.hypervisor`
/// entitlement it needs to call Hypervisor.framework.
///
/// Why this exists: `cargo build` / `cargo test` produce an UNSIGNED
/// `tb-sandbox-runner` (no entitlement). Booting with it fails deep inside
/// libkrun as `VmSetup(VmCreate)` / `krun_start_enter: Invalid argument
/// (os error 22)` — an opaque error that looks like a libkrun bug but is just a
/// missing entitlement. Rather than make every developer remember to re-sign
/// after each build, we self-heal here: if the binary lacks the entitlement we
/// ad-hoc re-sign it in place (exactly what `scripts/setup-local.sh` does).
///
/// Safety / scope:
/// - macOS only; a no-op elsewhere.
/// - Only touches binaries we know are locally built/installed by Cargo: a path
///   under `target/{debug,release}` (cargo build) or under `$CARGO_HOME/bin`
///   (`cargo install`, default `~/.cargo/bin`). Notarized/Homebrew releases live
///   elsewhere (e.g. `/opt/homebrew/bin`, `/Applications`) AND already carry the
///   entitlement, so they are never inspected or modified — zero overhead and no
///   risk of clobbering a Developer ID signature.
/// - Best-effort: if signing fails, boot proceeds and surfaces the original
///   clear error.
#[cfg(target_os = "macos")]
fn ensure_runner_signed(path: &Path) {
    if !is_self_signable_location(path) || runner_has_hypervisor_entitlement(path) {
        return;
    }
    let Some(entitlements) = write_hypervisor_entitlements_plist() else {
        return;
    };
    let status = Command::new("codesign")
        .arg("--entitlements")
        .arg(&entitlements)
        .arg("--force")
        .arg("-s")
        .arg("-")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(&entitlements);
    if matches!(status, Ok(s) if s.success()) {
        eprintln!(
            "tb-sandbox: re-signed {} with the hypervisor entitlement \
             (cargo build had stripped it)",
            path.display()
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn ensure_runner_signed(_path: &Path) {}

/// True when `path` is a locally Cargo-produced binary we may self-sign: a build
/// output under `.../target/{debug,release}/`, or a `cargo install` output under
/// `$CARGO_HOME/bin` (default `~/.cargo/bin`). Never an installed system release.
#[cfg(target_os = "macos")]
fn is_self_signable_location(path: &Path) -> bool {
    // cargo build: .../target/{debug,release}/...
    let mut saw_target = false;
    for component in path.components() {
        let part = component.as_os_str().to_string_lossy();
        if saw_target && (part == "debug" || part == "release") {
            return true;
        }
        saw_target = part == "target";
    }

    // cargo install: $CARGO_HOME/bin/... (default ~/.cargo/bin). Compare both
    // raw and canonicalized, since current_exe() may resolve symlinks (e.g. a
    // home dir on a symlinked volume) while the candidate dirs are not resolved.
    if let Some(parent) = path.parent() {
        let parent_canon = std::fs::canonicalize(parent).ok();
        for dir in cargo_install_bin_dirs() {
            if parent == dir {
                return true;
            }
            if let Some(ref pc) = parent_canon {
                if std::fs::canonicalize(&dir).ok().as_ref() == Some(pc) {
                    return true;
                }
            }
        }
    }
    false
}

/// Candidate `cargo install` bin directories: `$CARGO_HOME/bin` if set, plus the
/// default `~/.cargo/bin`.
#[cfg(target_os = "macos")]
fn cargo_install_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(cargo_home) = env::var_os("CARGO_HOME") {
        dirs.push(PathBuf::from(cargo_home).join("bin"));
    }
    if let Some(home) = env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".cargo").join("bin"));
    }
    dirs
}

#[cfg(target_os = "macos")]
fn runner_has_hypervisor_entitlement(path: &Path) -> bool {
    // `codesign -d --entitlements -` dumps entitlements (to stdout on modern
    // macOS, with some chatter on stderr); an unsigned binary yields neither.
    match Command::new("codesign")
        .arg("-d")
        .arg("--entitlements")
        .arg("-")
        .arg(path)
        .output()
    {
        Ok(output) => {
            let needle = "com.apple.security.hypervisor";
            String::from_utf8_lossy(&output.stdout).contains(needle)
                || String::from_utf8_lossy(&output.stderr).contains(needle)
        }
        Err(_) => false,
    }
}

/// Write the minimal hypervisor-only entitlements plist (matching
/// `packaging/os_mode.entitlements`) to a temp file. Returns its path, or None
/// if it could not be written.
#[cfg(target_os = "macos")]
fn write_hypervisor_entitlements_plist() -> Option<PathBuf> {
    const PLIST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>com.apple.security.hypervisor</key>\n\
\t<true/>\n\
</dict>\n\
</plist>\n";
    let path = env::temp_dir().join(format!(
        "tb-sandbox-hv-entitlements-{}.plist",
        std::process::id()
    ));
    std::fs::write(&path, PLIST).ok()?;
    Some(path)
}

/// Directories to search for the bundled runner in a packaged install, resolved
/// relative to the running `tb-sandbox` executable. Covers a flat layout
/// (artifacts next to the binary) and a Homebrew-style `bin/` + `libexec/`
/// split.
fn bundled_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(bin) = exe.parent() {
            dirs.push(bin.to_path_buf());
            dirs.push(bin.join("../libexec/tb-sandbox"));
        }
    }
    dirs
}

fn is_legacy_os_mode_launcher(launcher: &Path) -> bool {
    launcher
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "os_mode")
}

fn local_libkrun_dylib_path() -> Result<Option<PathBuf>> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    if let Some(path) = env::var_os("TB_SANDBOX_LIBKRUN_DIR") {
        return Ok(Some(PathBuf::from(path)));
    }

    // Legacy C launcher support only. Prefer release paths before debug paths
    // because local os_mode is usually linked against release libkrun;
    // preferring debug first can accidentally pick up a stale/incomplete dylib.
    let search: Vec<PathBuf> = bundled_dirs()
        .into_iter()
        .chain([
            PathBuf::from("../thousandbirds-libkrun/target/release"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../thousandbirds-libkrun/target/release"),
            PathBuf::from("../thousandbirds-libkrun/target/debug"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../thousandbirds-libkrun/target/debug"),
            PathBuf::from("/opt/homebrew/lib"),
            PathBuf::from("/usr/local/lib"),
        ])
        .collect();
    for dir in search {
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
