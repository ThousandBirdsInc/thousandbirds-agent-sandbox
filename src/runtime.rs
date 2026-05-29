use crate::config::{
    AgentImageRef, NetworkBackend, NetworkProxyConfig, ProxyHeaderRule, SandboxConfig, ShellConfig,
    WorkspaceMount,
};
use crate::credentials::{prepare_credentials, PreparedCredentials};
use crate::error::{Error, Result};
use crate::image::{json_bool, json_number, json_string, json_string_array, AgentImage};
use crate::libkrun::{launch_command, LaunchConfig};
use crate::proxy::NetworkProxy;
use crate::quiesce::{QuiesceAction, QuiesceClient};
use crate::scheduler::ResourceLedger;
use crate::snapshot::{
    config_from_manifest, snapshot_dir as snapshot_dir_path, ForkOptions, RestoreOptions,
    SnapshotId, SnapshotKind, SnapshotManifest, SnapshotOptions, SnapshotSummary,
};
use std::env;
use std::fs;
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxId(String);

impl SandboxId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || !value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Err(Error::InvalidArgument(format!(
                "sandbox id must contain only letters, numbers, '-' or '_': {value}"
            )));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SandboxId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    pub id: SandboxId,
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SandboxSummary {
    pub id: SandboxId,
    pub state_dir: PathBuf,
    pub image_path: PathBuf,
    /// Whether the sandbox currently has a live VM (running marker present).
    pub running: bool,
    /// MiB this sandbox reserves against the host memory budget while running.
    pub reserved_mib: Option<u64>,
}

/// A sandbox's configured memory limits, in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLimits {
    pub initial_mib: u32,
    pub max_mib: u32,
    pub min_mib: u32,
}

impl MemoryLimits {
    /// Runtime memory resizing is enabled when the ceiling exceeds the initial
    /// memory, leaving headroom the balloon can grow into.
    pub fn resize_enabled(&self) -> bool {
        self.max_mib > self.initial_mib
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMountInfo {
    pub tag: String,
    pub host_path: PathBuf,
    pub guest_path: String,
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub struct SandboxManager {
    pub state_root: PathBuf,
    pub cache_root: PathBuf,
}

pub struct RunningSandboxLaunch {
    child: Option<Child>,
    pub control_socket: PathBuf,
    /// Directory whose running reservation marker is released when this launch
    /// handle is dropped (i.e. after the VM process exits and the SDK drops it).
    running_marker_dir: Option<PathBuf>,
    _managed_gvproxy: Option<ManagedGvproxy>,
    _proxy: Option<NetworkProxy>,
}

impl RunningSandboxLaunch {
    pub fn take_child(&mut self) -> Result<Child> {
        self.child
            .take()
            .ok_or_else(|| Error::InvalidArgument("sandbox launch child already taken".to_string()))
    }
}

impl Drop for RunningSandboxLaunch {
    fn drop(&mut self) {
        if let Some(dir) = &self.running_marker_dir {
            ResourceLedger::clear_running(dir);
        }
    }
}

#[derive(Debug, Clone)]
struct StoredSandbox {
    id: SandboxId,
    image_path: PathBuf,
    root_disk: PathBuf,
    cpus: u8,
    memory_mib: u32,
    memory_max_mib: u32,
    memory_min_mib: u32,
    virtiofs_dax_size: u64,
    workspace_mounts: Vec<WorkspaceMount>,
    credentials_staging: Option<PathBuf>,
    runtime_config: PathBuf,
    runtime_control: PathBuf,
    ephemeral: bool,
    log_level: crate::config::LogLevel,
    network_proxy: NetworkProxyConfig,
    network_backend: NetworkBackend,
}

struct ManagedGvproxy {
    child: Child,
    socket_path: PathBuf,
    socket_dir: PathBuf,
}

impl ManagedGvproxy {
    fn start(runtime_config: &Path, executable: Option<&Path>) -> Result<Self> {
        if !cfg!(target_os = "macos") {
            return Err(Error::InvalidArgument(
                "automatic gvproxy startup is only supported on macOS; use --passt-socket on Linux"
                    .to_string(),
            ));
        }

        let executable = find_gvproxy_executable(executable)?;
        let socket_dir = gvproxy_socket_dir()?;
        let socket_path = socket_dir.join("gvproxy.sock");
        let log_path = runtime_config.join("gvproxy.log");
        let ssh_port = available_localhost_port()?;
        remove_optional_file(&socket_path)?;

        let log = fs::File::create(&log_path).map_err(|err| Error::io_path(&log_path, err))?;
        let mut child = Command::new(&executable)
            .arg("--debug")
            .arg("--listen-vfkit")
            .arg(format!("unixgram://{}", socket_path.display()))
            .arg("--ssh-port")
            .arg(ssh_port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|err| Error::io_path(&executable, err))?;

        wait_for_socket(&socket_path, &mut child, &log_path)?;
        eprintln!(
            "started gvproxy: {} -> {}",
            executable.display(),
            socket_path.display()
        );
        Ok(Self {
            child,
            socket_path,
            socket_dir,
        })
    }
}

impl Drop for ManagedGvproxy {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            Err(_) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        let _ = fs::remove_dir_all(&self.socket_dir);
    }
}

impl SandboxManager {
    pub fn new() -> Self {
        Self {
            state_root: default_state_root(),
            cache_root: default_cache_root(),
        }
    }

    pub fn create(&self, config: SandboxConfig) -> Result<Sandbox> {
        let image = AgentImage::load(&config.image.path)?;
        let id = match &config.name {
            Some(name) => SandboxId::new(sanitize_id(name))?,
            None => SandboxId::new(format!("sandbox-{}", unix_millis()))?,
        };
        let sandbox_dir = self.sandbox_dir(&id);
        if sandbox_dir.exists() {
            return Err(Error::InvalidArgument(format!(
                "sandbox {} already exists at {}",
                id,
                sandbox_dir.display()
            )));
        }
        fs::create_dir_all(&sandbox_dir).map_err(|err| Error::io_path(&sandbox_dir, err))?;

        match self.create_in_dir(&sandbox_dir, id.clone(), config, image) {
            Ok(sandbox) => Ok(sandbox),
            Err(err) => {
                let _ = fs::remove_dir_all(&sandbox_dir);
                Err(err)
            }
        }
    }

    fn create_in_dir(
        &self,
        sandbox_dir: &Path,
        id: SandboxId,
        config: SandboxConfig,
        image: AgentImage,
    ) -> Result<Sandbox> {
        let root_disk = sandbox_dir.join("root.raw");
        clone_or_copy_root(&image.root_disk, &root_disk)?;

        let runtime_config = sandbox_dir.join("runtime-config");
        fs::create_dir_all(&runtime_config).map_err(|err| Error::io_path(&runtime_config, err))?;
        write_runtime_config(&runtime_config, &config, &image)?;
        let runtime_control = sandbox_dir.join("runtime-control");
        fs::create_dir_all(&runtime_control)
            .map_err(|err| Error::io_path(&runtime_control, err))?;
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&runtime_control)
                .map_err(|err| Error::io_path(&runtime_control, err))?
                .permissions();
            permissions.set_mode(0o777);
            fs::set_permissions(&runtime_control, permissions)
                .map_err(|err| Error::io_path(&runtime_control, err))?;
        }

        let credentials_staging = if config.credential_profiles.is_empty()
            && config.extra_credentials.is_empty()
            && config.credential_env.is_empty()
        {
            None
        } else {
            let prepared = prepare_credentials(
                &sandbox_dir.join("credentials-staging"),
                &config.credential_profiles,
                &config.extra_credentials,
                &config.credential_env,
                config.yes,
            )?;
            print_prepared_credentials(&prepared);
            Some(prepared.staging_dir)
        };

        let stored = StoredSandbox {
            id: id.clone(),
            image_path: image.root.clone(),
            root_disk,
            cpus: config.cpus,
            memory_mib: config.memory_mib,
            memory_max_mib: config.memory_max_mib,
            memory_min_mib: config.memory_min_mib,
            virtiofs_dax_size: config.virtiofs_dax_size,
            workspace_mounts: config.workspace_mounts.clone(),
            credentials_staging,
            runtime_config,
            runtime_control,
            ephemeral: config.ephemeral,
            log_level: config.log_level,
            network_proxy: config.network_proxy.clone(),
            network_backend: config.network_backend.clone(),
        };
        write_sandbox_metadata(&sandbox_dir.join("sandbox.json"), &stored)?;

        Ok(Sandbox {
            id,
            state_dir: sandbox_dir.to_path_buf(),
        })
    }

    pub fn shell(&self, id: &SandboxId) -> Result<()> {
        let stored = self.load(id)?;
        // Admission control: refuse (or queue) if starting this VM would exceed
        // the host memory budget. Done before any gvproxy/proxy side effects.
        ResourceLedger::from_env(&self.state_root).admit(id.as_str(), stored.memory_mib as u64)?;
        let image = AgentImage::load(&stored.image_path)?;
        write_terminal_env(&stored.runtime_config)?;
        let managed_gvproxy = match &stored.network_backend {
            NetworkBackend::GvproxyAuto { executable } => Some(ManagedGvproxy::start(
                &stored.runtime_config,
                executable.as_deref(),
            )?),
            _ => None,
        };
        let launch_network_backend = managed_gvproxy
            .as_ref()
            .map(|gvproxy| NetworkBackend::Gvproxy {
                socket: gvproxy.socket_path.clone(),
            })
            .unwrap_or_else(|| stored.network_backend.clone());

        profile_mark("backend_ready");
        let proxy = if stored.network_proxy.enabled && launch_network_backend.is_enabled() {
            let proxy = NetworkProxy::start(
                stored.network_proxy.header_rules.clone(),
                stored.network_proxy.tls_mitm,
            )?;
            write_proxy_env(&stored.runtime_config, Some(&proxy))?;
            eprintln!("agent network proxy: {}", proxy.url());
            if stored.network_proxy.tls_mitm {
                eprintln!("TLS MITM enabled: sandbox CA installed in guest trust stores");
            }
            if !stored.network_proxy.header_rules.is_empty() {
                eprintln!(
                    "proxy header substitution applies to HTTP and intercepted HTTPS requests"
                );
            }
            Some(proxy)
        } else if stored.network_proxy.enabled {
            write_proxy_env(&stored.runtime_config, None)?;
            eprintln!(
                "agent network proxy disabled: configure --gvproxy-socket or --passt-socket to give the guest a route to the host proxy"
            );
            None
        } else {
            write_proxy_env(&stored.runtime_config, None)?;
            None
        };
        profile_mark("launch");
        let control_socket = self.control_socket_path(id);
        let control_socket_arg = if stored.memory_max_mib > stored.memory_mib {
            Some(control_socket.as_path())
        } else {
            None
        };
        // Build the launcher command and run it ourselves (rather than the
        // fire-and-forget `launch` helper) so we can record the VM process PID in
        // the running reservation marker and release it once the VM exits.
        let mut command = launch_command(LaunchConfig {
            image: &image,
            root_disk: &stored.root_disk,
            cpus: stored.cpus,
            memory_mib: stored.memory_mib,
            memory_max_mib: stored.memory_max_mib,
            memory_min_mib: stored.memory_min_mib,
            control_socket: control_socket_arg,
            virtiofs_dax_size: stored.virtiofs_dax_size,
            workspace_mounts: &stored.workspace_mounts,
            credentials_mount: stored.credentials_staging.as_deref(),
            runtime_config_mount: &stored.runtime_config,
            runtime_control_mount: &stored.runtime_control,
            network_backend: &launch_network_backend,
            log_level: stored.log_level,
            // Phase 3 plumbing — populated by a future
            // SandboxManager::restore_with_memory once libkrun lands
            // --restore. Cold-boot paths leave it None.
            restore_from: None,
        })?;
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let sandbox_dir = self.sandbox_dir(id);
        let mut child = command.spawn().map_err(Error::io)?;
        let ledger = ResourceLedger::from_env(&self.state_root);
        let _ = ledger.record_running(
            &sandbox_dir,
            id.as_str(),
            child.id(),
            stored.memory_mib as u64,
        );
        let status = child.wait().map_err(Error::io)?;
        ResourceLedger::clear_running(&sandbox_dir);
        // The VMM removes the socket on exit; clean up defensively.
        let _ = fs::remove_file(&control_socket);
        drop(proxy);
        drop(managed_gvproxy);
        if stored.ephemeral {
            self.delete(id)?;
        }
        if !status.success() {
            return Err(Error::ProcessFailed {
                command: "libkrun os_mode launcher".to_string(),
                status,
            });
        }
        Ok(())
    }

    pub fn launch_piped(&self, id: &SandboxId) -> Result<RunningSandboxLaunch> {
        let stored = self.load(id)?;
        ResourceLedger::from_env(&self.state_root).admit(id.as_str(), stored.memory_mib as u64)?;
        let image = AgentImage::load(&stored.image_path)?;
        write_terminal_env(&stored.runtime_config)?;
        let managed_gvproxy = match &stored.network_backend {
            NetworkBackend::GvproxyAuto { executable } => Some(ManagedGvproxy::start(
                &stored.runtime_config,
                executable.as_deref(),
            )?),
            _ => None,
        };
        let launch_network_backend = managed_gvproxy
            .as_ref()
            .map(|gvproxy| NetworkBackend::Gvproxy {
                socket: gvproxy.socket_path.clone(),
            })
            .unwrap_or_else(|| stored.network_backend.clone());
        let proxy = if stored.network_proxy.enabled && launch_network_backend.is_enabled() {
            let proxy = NetworkProxy::start(
                stored.network_proxy.header_rules.clone(),
                stored.network_proxy.tls_mitm,
            )?;
            write_proxy_env(&stored.runtime_config, Some(&proxy))?;
            Some(proxy)
        } else {
            write_proxy_env(&stored.runtime_config, None)?;
            None
        };
        let control_socket = self.control_socket_path(id);
        let control_socket_arg = if stored.memory_max_mib > stored.memory_mib {
            Some(control_socket.as_path())
        } else {
            None
        };
        let mut command = launch_command(LaunchConfig {
            image: &image,
            root_disk: &stored.root_disk,
            cpus: stored.cpus,
            memory_mib: stored.memory_mib,
            memory_max_mib: stored.memory_max_mib,
            memory_min_mib: stored.memory_min_mib,
            control_socket: control_socket_arg,
            virtiofs_dax_size: stored.virtiofs_dax_size,
            workspace_mounts: &stored.workspace_mounts,
            credentials_mount: stored.credentials_staging.as_deref(),
            runtime_config_mount: &stored.runtime_config,
            runtime_control_mount: &stored.runtime_control,
            network_backend: &launch_network_backend,
            log_level: stored.log_level,
            // Phase 3 plumbing — populated by a future
            // SandboxManager::restore_with_memory once libkrun lands
            // --restore. Cold-boot paths leave it None.
            restore_from: None,
        })?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Run the launcher in its own process group so SDK-side cancellation
        // (kill_pid) can terminate the whole VM/launcher subprocess tree. Only
        // the detached SDK launch path uses launch_piped; the interactive
        // shell() path is unaffected.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn().map_err(Error::io)?;
        let sandbox_dir = self.sandbox_dir(id);
        let ledger = ResourceLedger::from_env(&self.state_root);
        let _ = ledger.record_running(
            &sandbox_dir,
            id.as_str(),
            child.id(),
            stored.memory_mib as u64,
        );
        Ok(RunningSandboxLaunch {
            child: Some(child),
            control_socket,
            running_marker_dir: Some(sandbox_dir),
            _managed_gvproxy: managed_gvproxy,
            _proxy: proxy,
        })
    }

    pub fn set_initial_command(&self, id: &SandboxId, initial_command: &str) -> Result<()> {
        let stored = self.load(id)?;
        write_initial_command_env(&stored.runtime_config, initial_command)
    }

    pub fn run_shell(&self, config: SandboxConfig) -> Result<()> {
        profile_mark("run_shell_start");
        let sandbox = self.create(config)?;
        profile_mark("created");
        self.shell(&sandbox.id)
    }

    pub fn reopen(&self, id: &SandboxId) -> Result<Sandbox> {
        let sandbox_dir = self.sandbox_dir(id);
        if !sandbox_dir.exists() {
            return Err(Error::NotFound(format!("sandbox {id}")));
        }
        let _ = self.load(id)?;
        Ok(Sandbox {
            id: id.clone(),
            state_dir: sandbox_dir,
        })
    }

    pub fn inspect_mounts(&self, id: &SandboxId) -> Result<Vec<WorkspaceMountInfo>> {
        let stored = self.load(id)?;
        Ok(stored
            .workspace_mounts
            .into_iter()
            .map(|mount| WorkspaceMountInfo {
                tag: mount.tag,
                host_path: mount.host_path,
                guest_path: mount.guest_path,
                read_only: mount.read_only,
            })
            .collect())
    }

    pub fn delete(&self, id: &SandboxId) -> Result<()> {
        let sandbox_dir = self.sandbox_dir(id);
        if !sandbox_dir.exists() {
            return Err(Error::NotFound(format!("sandbox {id}")));
        }
        fs::remove_dir_all(&sandbox_dir).map_err(|err| Error::io_path(&sandbox_dir, err))
    }

    pub fn inspect(&self, id: &SandboxId) -> Result<String> {
        let path = self.sandbox_dir(id).join("sandbox.json");
        fs::read_to_string(&path).map_err(|err| Error::io_path(path, err))
    }

    /// Adjust the usable memory of a running sandbox to `target_mib`, clamped to
    /// the sandbox's configured `[memory_min_mib, memory_max_mib]` range. The
    /// sandbox must have been created with resize headroom (`--memory-max-mib`
    /// greater than its initial memory) and currently be running.
    pub fn resize_memory(&self, id: &SandboxId, target_mib: u32) -> Result<String> {
        let stored = self.load(id)?;
        if stored.memory_max_mib <= stored.memory_mib {
            return Err(Error::InvalidArgument(format!(
                "sandbox {id} was not created with resize headroom; recreate it with \
                 --memory-max-mib greater than --memory-mib (currently {} MiB)",
                stored.memory_mib
            )));
        }
        let clamped = target_mib.clamp(stored.memory_min_mib, stored.memory_max_mib);
        if clamped != target_mib {
            eprintln!(
                "note: requested {target_mib} MiB clamped to {clamped} MiB \
                 (range {}-{} MiB)",
                stored.memory_min_mib, stored.memory_max_mib
            );
        }
        let socket = self.control_socket_path(id);
        if !socket.exists() {
            return Err(Error::NotFound(format!(
                "control socket for sandbox {id} not found at {}; is the sandbox running?",
                socket.display()
            )));
        }
        let response = send_control_command(&socket, &format!("target-mib {clamped}"))?;
        if response.starts_with("error") {
            return Err(Error::InvalidArgument(format!(
                "resize rejected by sandbox: {response}"
            )));
        }
        Ok(response)
    }

    /// Path of the per-sandbox Unix socket used for runtime control commands.
    pub fn control_socket_path(&self, id: &SandboxId) -> PathBuf {
        self.sandbox_dir(id).join("control.sock")
    }

    /// The sandbox's configured memory limits. Used by background reclaim to
    /// decide whether a VM has resize headroom and what floor to shrink to.
    pub fn memory_limits(&self, id: &SandboxId) -> Result<MemoryLimits> {
        let stored = self.load(id)?;
        Ok(MemoryLimits {
            initial_mib: stored.memory_mib,
            max_mib: stored.memory_max_mib,
            min_mib: stored.memory_min_mib,
        })
    }

    /// Whether the sandbox currently has a live VM process.
    pub fn is_running(&self, id: &SandboxId) -> bool {
        ResourceLedger::is_running(&self.sandbox_dir(id))
    }

    pub fn list(&self) -> Result<Vec<SandboxSummary>> {
        let root = self.state_root.join("sandboxes");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut summaries = Vec::new();
        for entry in fs::read_dir(&root).map_err(|err| Error::io_path(&root, err))? {
            let entry = entry.map_err(Error::io)?;
            if !entry.file_type().map_err(Error::io)?.is_dir() {
                continue;
            }
            let id = match entry.file_name().to_str().map(SandboxId::new) {
                Some(Ok(id)) => id,
                _ => continue,
            };
            if let Ok(stored) = self.load(&id) {
                let dir = entry.path();
                let reserved_mib = ResourceLedger::reserved_for(&dir);
                summaries.push(SandboxSummary {
                    id,
                    running: reserved_mib.is_some(),
                    reserved_mib,
                    state_dir: dir,
                    image_path: stored.image_path,
                });
            }
        }
        summaries.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(summaries)
    }

    /// Capture a sandbox's root disk and configuration into an immutable
    /// artifact under `<state>/snapshots/<snapshot_id>/`. The snapshot is
    /// **disk-only**: guest memory, vCPU state, and live processes are not
    /// preserved — `restore` cold-boots from the snapshotted disk.
    ///
    /// When the sandbox is running and `opts.skip_quiesce` is false, the
    /// in-guest `agent-sandbox-quiesce` daemon is asked to flush (and
    /// optionally `fsfreeze`) the rootfs before the disk clone, producing a
    /// clean point-in-time image. Stopped sandboxes skip quiesce regardless
    /// (nothing to flush). When quiesce is explicitly skipped on a running
    /// sandbox, the disk clone is crash-consistent — filesystem journal
    /// replays on next boot, but in-flight application writes can be torn.
    pub fn snapshot(&self, id: &SandboxId, opts: SnapshotOptions) -> Result<SnapshotId> {
        let stored = self.load(id)?;
        // Phase 3 surface check. SnapshotOptions accepts capture_memory: true
        // today so callers can write against the eventual API, but until the
        // libkrun-side capture path lands (see
        // docs/closing-the-smolvm-gap-plan.md Phase 3 and the matching
        // thousandbirds-libkrun/design_docs/snapshot_restore_implementation.md)
        // we cannot honor it. Surface a typed error here rather than silently
        // producing a disk-only snapshot — the caller would otherwise believe
        // memory was captured.
        if opts.capture_memory {
            // Phase 3 surface check. The wrapper-side surface is wired
            // through (SnapshotOptions::capture_memory, manifest's `Full`
            // kind, LaunchConfig::restore_from for the reverse path). The
            // libkrun side ships `krun_snapshot_to_path` as a reserved C
            // ABI symbol that returns -ENOSYS until the vCPU-thread
            // orchestration lands — see
            // `../thousandbirds-libkrun/design_docs/snapshot_restore_implementation.md`
            // (Phase D). When that ships, this branch flips from "return
            // typed error" to "send snapshot-memory RPC over the control
            // socket". No shape change required.
            return Err(Error::InvalidArgument(format!(
                "snapshot of sandbox {id} requested capture_memory=true, but the \
                 libkrun snapshot orchestration is -ENOSYS in this build. The \
                 wrapper-side plumbing is in place; the libkrun-side C API \
                 symbols (krun_snapshot_to_path / krun_set_snapshot_restore_from) \
                 are reserved and the type surface compiled, awaiting vCPU-thread \
                 Pause+Snapshot event orchestration. Track: \
                 docs/closing-the-smolvm-gap-plan.md Phase 3 + \
                 ../thousandbirds-libkrun/design_docs/snapshot_restore_implementation.md \
                 §Phase D."
            )));
        }
        let snap_id = match opts.name {
            Some(name) => SnapshotId::new(sanitize_id(&name))?,
            None => SnapshotId::new(format!("snapshot-{}", unix_millis()))?,
        };
        let snap_dir = self.snapshot_dir(&snap_id);
        if snap_dir.exists() {
            return Err(Error::InvalidArgument(format!(
                "snapshot {} already exists at {}",
                snap_id,
                snap_dir.display()
            )));
        }
        let snapshots_root = self.state_root.join("snapshots");
        fs::create_dir_all(&snapshots_root).map_err(|err| Error::io_path(&snapshots_root, err))?;
        fs::create_dir(&snap_dir).map_err(|err| Error::io_path(&snap_dir, err))?;

        let quiesce_timeout = opts.quiesce_timeout.unwrap_or(Duration::from_secs(10));
        let should_quiesce = !opts.skip_quiesce && self.is_running(id);
        let quiesce_action = if opts.quiesce_freeze {
            QuiesceAction::Freeze
        } else {
            QuiesceAction::Sync
        };

        let outcome = (|| -> Result<()> {
            // Hold the quiesce ticket across the disk clone so the guest
            // stays flushed/frozen for the duration. `ticket.take()` below
            // releases explicitly so the wrapper can surface release errors;
            // if `clone_or_copy_root` fails first, `?` short-circuits and
            // the ticket's `Drop` sends a best-effort `resume.req` so the
            // guest never stays frozen because of a Rust early return.
            let mut ticket = if should_quiesce {
                Some(
                    QuiesceClient::new(&stored.runtime_control)
                        .freeze(quiesce_action, quiesce_timeout)?,
                )
            } else {
                None
            };
            let achieved_unix_millis = ticket.as_ref().map(|t| t.achieved_unix_millis());

            let root_dst = snap_dir.join("root.raw");
            clone_or_copy_root(&stored.root_disk, &root_dst)?;

            if let Some(t) = ticket.take() {
                t.release(quiesce_timeout)?;
            }

            // Bundle credentials staging so a later restore can mount the
            // same auth material without re-reading host secrets.
            let had_credentials = match &stored.credentials_staging {
                Some(src) if src.exists() => {
                    let dst = snap_dir.join("credentials-staging");
                    copy_dir_recursive(src, &dst)?;
                    true
                }
                _ => false,
            };

            let manifest = SnapshotManifest {
                version: 1,
                kind: SnapshotKind::DiskOnly,
                id: snap_id.clone(),
                parent_sandbox_id: id.as_str().to_string(),
                created_unix_millis: unix_millis(),
                image_path: stored.image_path.clone(),
                cpus: stored.cpus,
                memory_mib: stored.memory_mib,
                memory_max_mib: stored.memory_max_mib,
                memory_min_mib: stored.memory_min_mib,
                virtiofs_dax_size: stored.virtiofs_dax_size,
                workspace_mounts: stored.workspace_mounts.clone(),
                network_backend: stored.network_backend.clone(),
                network_proxy: stored.network_proxy.clone(),
                had_credentials,
                quiesced: should_quiesce,
                quiesce_action: if should_quiesce {
                    Some(quiesce_action.as_str().to_string())
                } else {
                    None
                },
                quiesced_unix_millis: achieved_unix_millis,
                // Phase 3 (memory capture) lands this as Some(path) when
                // the libkrun side ships. Today's disk-only path leaves it
                // None; the manifest's `kind` stays `disk-only`.
                memory_image_path: None,
            };
            let manifest_path = snap_dir.join("manifest.json");
            fs::write(&manifest_path, manifest.to_json())
                .map_err(|err| Error::io_path(manifest_path, err))?;
            Ok(())
        })();

        match outcome {
            Ok(()) => Ok(snap_id),
            Err(err) => {
                let _ = fs::remove_dir_all(&snap_dir);
                Err(err)
            }
        }
    }

    /// Create a fresh sandbox whose root disk starts from a snapshot. The
    /// restored sandbox is a cold boot from the snapshotted disk — guest
    /// memory and live processes are not preserved.
    ///
    /// `opts` overrides the snapshot manifest field-by-field. Unset fields
    /// inherit from the snapshot.
    pub fn restore(&self, snap_id: &SnapshotId, opts: RestoreOptions) -> Result<Sandbox> {
        let snap_dir = self.snapshot_dir(snap_id);
        if !snap_dir.exists() {
            return Err(Error::NotFound(format!("snapshot {snap_id}")));
        }
        let manifest_path = snap_dir.join("manifest.json");
        let manifest_json = fs::read_to_string(&manifest_path)
            .map_err(|err| Error::io_path(&manifest_path, err))?;
        let manifest = SnapshotManifest::from_json(&manifest_json)?;
        // Phase 3 symmetry: a Full-kind snapshot expects the wrapper to
        // route the launcher invocation through LaunchConfig::restore_from
        // so libkrun rebuilds vCPU + memory state instead of cold-booting.
        // The wrapper-side plumbing is in place
        // (src/libkrun.rs LaunchConfig::restore_from emits --restore PATH),
        // and the libkrun-side C ABI symbol `krun_set_snapshot_restore_from`
        // is reserved, but the actual restore execution in libkrun is
        // pending (returns -ENOSYS until the orchestration ships). Refuse
        // here rather than silently cold-booting from the snapshot's disk
        // and discarding the captured memory — that would deceive the
        // caller into believing they got a process-preserving restore.
        if manifest.kind == SnapshotKind::Full {
            return Err(Error::InvalidArgument(format!(
                "snapshot {snap_id} is a Full-kind (memory + vCPU state) artifact \
                 but this build cannot yet restore from it: libkrun's runtime \
                 restore execution is -ENOSYS. The wrapper-side plumbing is in \
                 place (LaunchConfig::restore_from emits --restore PATH to the \
                 launcher; the C ABI symbol krun_set_snapshot_restore_from is \
                 reserved). Track: \
                 ../thousandbirds-libkrun/design_docs/snapshot_restore_implementation.md \
                 §Phase D."
            )));
        }

        let new_id = match &opts.name {
            Some(name) => SandboxId::new(sanitize_id(name))?,
            None => SandboxId::new(format!("restore-{}", unix_millis()))?,
        };
        let sandbox_dir = self.sandbox_dir(&new_id);
        if sandbox_dir.exists() {
            return Err(Error::InvalidArgument(format!(
                "sandbox {} already exists at {}",
                new_id,
                sandbox_dir.display()
            )));
        }
        let sandboxes_root = self.state_root.join("sandboxes");
        fs::create_dir_all(&sandboxes_root).map_err(|err| Error::io_path(&sandboxes_root, err))?;
        fs::create_dir(&sandbox_dir).map_err(|err| Error::io_path(&sandbox_dir, err))?;

        let mut config = config_from_manifest(&manifest, &opts);
        // The runtime-config wants a name; default to the new sandbox id when
        // the caller did not override it via `opts.name`.
        if config.name.is_none() {
            config.name = Some(new_id.as_str().to_string());
        }
        let image = AgentImage::load(&manifest.image_path)?;

        let outcome = (|| -> Result<Sandbox> {
            let root_disk = sandbox_dir.join("root.raw");
            // Clone the snapshot's root, not the image's. APFS CoW shares
            // blocks with the snapshot until either side writes.
            clone_or_copy_root(&snap_dir.join("root.raw"), &root_disk)?;

            let runtime_config = sandbox_dir.join("runtime-config");
            fs::create_dir_all(&runtime_config)
                .map_err(|err| Error::io_path(&runtime_config, err))?;
            write_runtime_config(&runtime_config, &config, &image)?;
            let runtime_control = sandbox_dir.join("runtime-control");
            fs::create_dir_all(&runtime_control)
                .map_err(|err| Error::io_path(&runtime_control, err))?;
            #[cfg(unix)]
            {
                let mut permissions = fs::metadata(&runtime_control)
                    .map_err(|err| Error::io_path(&runtime_control, err))?
                    .permissions();
                permissions.set_mode(0o777);
                fs::set_permissions(&runtime_control, permissions)
                    .map_err(|err| Error::io_path(&runtime_control, err))?;
            }

            let credentials_staging = if manifest.had_credentials && !opts.drop_credentials {
                let src = snap_dir.join("credentials-staging");
                if src.exists() {
                    let dst = sandbox_dir.join("credentials-staging");
                    copy_dir_recursive(&src, &dst)?;
                    Some(dst)
                } else {
                    None
                }
            } else {
                None
            };

            let stored = StoredSandbox {
                id: new_id.clone(),
                image_path: image.root.clone(),
                root_disk,
                cpus: config.cpus,
                memory_mib: config.memory_mib,
                memory_max_mib: config.memory_max_mib,
                memory_min_mib: config.memory_min_mib,
                virtiofs_dax_size: config.virtiofs_dax_size,
                workspace_mounts: config.workspace_mounts.clone(),
                credentials_staging,
                runtime_config,
                runtime_control,
                ephemeral: config.ephemeral,
                log_level: config.log_level,
                network_proxy: config.network_proxy.clone(),
                network_backend: config.network_backend.clone(),
            };
            write_sandbox_metadata(&sandbox_dir.join("sandbox.json"), &stored)?;

            Ok(Sandbox {
                id: new_id.clone(),
                state_dir: sandbox_dir.clone(),
            })
        })();

        match outcome {
            Ok(sandbox) => Ok(sandbox),
            Err(err) => {
                let _ = fs::remove_dir_all(&sandbox_dir);
                Err(err)
            }
        }
    }

    /// Branch a sandbox: produce a sibling whose root disk starts from a
    /// (crash-consistent) copy of the parent's at fork time, and whose
    /// configuration — hardware, workspace, credentials, network — is
    /// inherited from the parent.
    ///
    /// Composes snapshot+restore in memory but persists no intermediate
    /// artifact under `<state>/snapshots/`. Use `snapshot` + `restore`
    /// separately when you want a reusable checkpoint.
    pub fn fork(&self, parent_id: &SandboxId, opts: ForkOptions) -> Result<Sandbox> {
        let parent = self.load(parent_id)?;
        let image = AgentImage::load(&parent.image_path)?;

        let new_id = match &opts.name {
            Some(name) => SandboxId::new(sanitize_id(name))?,
            None => SandboxId::new(format!("{}-fork-{}", parent_id.as_str(), unix_millis()))?,
        };
        let sandbox_dir = self.sandbox_dir(&new_id);
        if sandbox_dir.exists() {
            return Err(Error::InvalidArgument(format!(
                "sandbox {} already exists at {}",
                new_id,
                sandbox_dir.display()
            )));
        }
        let sandboxes_root = self.state_root.join("sandboxes");
        fs::create_dir_all(&sandboxes_root).map_err(|err| Error::io_path(&sandboxes_root, err))?;
        fs::create_dir(&sandbox_dir).map_err(|err| Error::io_path(&sandbox_dir, err))?;

        let workspace_mounts = opts
            .workspace_mounts
            .clone()
            .unwrap_or_else(|| parent.workspace_mounts.clone());

        let outcome = (|| -> Result<Sandbox> {
            let root_disk = sandbox_dir.join("root.raw");
            clone_or_copy_root(&parent.root_disk, &root_disk)?;

            let runtime_config = sandbox_dir.join("runtime-config");
            fs::create_dir_all(&runtime_config)
                .map_err(|err| Error::io_path(&runtime_config, err))?;
            // Synthesize a SandboxConfig matching the parent so the runtime
            // config emits the same guest-side env. ShellConfig defaults
            // (agent user, /workspace, /bin/bash) match every existing
            // sandbox; nothing in StoredSandbox overrides them.
            let mut config = SandboxConfig::new(AgentImageRef::new(image.root.clone()));
            config.name = Some(new_id.as_str().to_string());
            config.cpus = parent.cpus;
            config.memory_mib = parent.memory_mib;
            config.memory_max_mib = parent.memory_max_mib;
            config.memory_min_mib = parent.memory_min_mib;
            config.virtiofs_dax_size = parent.virtiofs_dax_size;
            config.workspace_mounts = workspace_mounts.clone();
            config.network_proxy = parent.network_proxy.clone();
            config.network_backend = parent.network_backend.clone();
            config.ephemeral = opts.ephemeral;
            config.log_level = parent.log_level;
            config.shell = ShellConfig::default();
            write_runtime_config(&runtime_config, &config, &image)?;

            let runtime_control = sandbox_dir.join("runtime-control");
            fs::create_dir_all(&runtime_control)
                .map_err(|err| Error::io_path(&runtime_control, err))?;
            #[cfg(unix)]
            {
                let mut permissions = fs::metadata(&runtime_control)
                    .map_err(|err| Error::io_path(&runtime_control, err))?
                    .permissions();
                permissions.set_mode(0o777);
                fs::set_permissions(&runtime_control, permissions)
                    .map_err(|err| Error::io_path(&runtime_control, err))?;
            }

            let credentials_staging = match (&parent.credentials_staging, opts.drop_credentials) {
                (Some(src), false) if src.exists() => {
                    let dst = sandbox_dir.join("credentials-staging");
                    copy_dir_recursive(src, &dst)?;
                    Some(dst)
                }
                _ => None,
            };

            let stored = StoredSandbox {
                id: new_id.clone(),
                image_path: image.root.clone(),
                root_disk,
                cpus: parent.cpus,
                memory_mib: parent.memory_mib,
                memory_max_mib: parent.memory_max_mib,
                memory_min_mib: parent.memory_min_mib,
                virtiofs_dax_size: parent.virtiofs_dax_size,
                workspace_mounts,
                credentials_staging,
                runtime_config,
                runtime_control,
                ephemeral: opts.ephemeral,
                log_level: parent.log_level,
                network_proxy: parent.network_proxy.clone(),
                network_backend: parent.network_backend.clone(),
            };
            write_sandbox_metadata(&sandbox_dir.join("sandbox.json"), &stored)?;

            Ok(Sandbox {
                id: new_id.clone(),
                state_dir: sandbox_dir.clone(),
            })
        })();

        match outcome {
            Ok(sandbox) => Ok(sandbox),
            Err(err) => {
                let _ = fs::remove_dir_all(&sandbox_dir);
                Err(err)
            }
        }
    }

    /// List the snapshots stored under `<state>/snapshots/`. Snapshots whose
    /// `manifest.json` is missing or unparseable are silently skipped, the
    /// same way `list()` skips sandbox directories without metadata.
    pub fn snapshots(&self) -> Result<Vec<SnapshotSummary>> {
        let root = self.state_root.join("snapshots");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut summaries = Vec::new();
        for entry in fs::read_dir(&root).map_err(|err| Error::io_path(&root, err))? {
            let entry = entry.map_err(Error::io)?;
            if !entry.file_type().map_err(Error::io)?.is_dir() {
                continue;
            }
            let id = match entry.file_name().to_str().map(SnapshotId::new) {
                Some(Ok(id)) => id,
                _ => continue,
            };
            let manifest_path = entry.path().join("manifest.json");
            let Ok(json) = fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = SnapshotManifest::from_json(&json) else {
                continue;
            };
            let root_disk_bytes = fs::metadata(entry.path().join("root.raw"))
                .ok()
                .map(|m| m.len());
            summaries.push(SnapshotSummary {
                id,
                parent_sandbox_id: manifest.parent_sandbox_id,
                kind: manifest.kind,
                created_unix_millis: manifest.created_unix_millis,
                state_dir: entry.path(),
                root_disk_bytes,
            });
        }
        summaries.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(summaries)
    }

    /// Read a snapshot's manifest as raw JSON.
    pub fn inspect_snapshot(&self, id: &SnapshotId) -> Result<String> {
        let path = self.snapshot_dir(id).join("manifest.json");
        fs::read_to_string(&path).map_err(|err| Error::io_path(path, err))
    }

    /// Delete a snapshot directory (and its cloned root disk + bundled
    /// credentials staging, if any).
    pub fn delete_snapshot(&self, id: &SnapshotId) -> Result<()> {
        let dir = self.snapshot_dir(id);
        if !dir.exists() {
            return Err(Error::NotFound(format!("snapshot {id}")));
        }
        fs::remove_dir_all(&dir).map_err(|err| Error::io_path(&dir, err))
    }

    /// Resolve the directory a snapshot lives in under the configured state
    /// root.
    pub fn snapshot_dir(&self, id: &SnapshotId) -> PathBuf {
        snapshot_dir_path(&self.state_root, id)
    }

    fn sandbox_dir(&self, id: &SandboxId) -> PathBuf {
        self.state_root.join("sandboxes").join(id.as_str())
    }

    fn load(&self, id: &SandboxId) -> Result<StoredSandbox> {
        let sandbox_dir = self.sandbox_dir(id);
        let path = sandbox_dir.join("sandbox.json");
        let json = fs::read_to_string(&path).map_err(|err| Error::io_path(&path, err))?;
        let image_path = required_string(&json, "image_path")?;
        let root_disk = required_string(&json, "root_disk")?;
        let runtime_config = required_string(&json, "runtime_config")?;
        let runtime_control = json_string(&json, "runtime_control")
            .map(PathBuf::from)
            .unwrap_or_else(|| sandbox_dir.join("runtime-control"));
        Ok(StoredSandbox {
            id: id.clone(),
            image_path: PathBuf::from(image_path),
            root_disk: PathBuf::from(root_disk),
            cpus: json_number(&json, "cpus").unwrap_or(2) as u8,
            memory_mib: json_number(&json, "memory_mib").unwrap_or(2048) as u32,
            memory_max_mib: json_number(&json, "memory_max_mib")
                .map(|v| v as u32)
                .unwrap_or_else(|| json_number(&json, "memory_mib").unwrap_or(2048) as u32),
            memory_min_mib: json_number(&json, "memory_min_mib").unwrap_or(512) as u32,
            virtiofs_dax_size: json_number(&json, "virtiofs_dax_size").unwrap_or(0),
            workspace_mounts: parse_mounts(&json)?,
            credentials_staging: json_string(&json, "credentials_staging")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            runtime_config: PathBuf::from(runtime_config),
            runtime_control,
            ephemeral: json_bool(&json, "ephemeral").unwrap_or(false),
            log_level: crate::config::LogLevel::Error,
            network_proxy: NetworkProxyConfig {
                enabled: json_bool(&json, "network_proxy_enabled").unwrap_or(true),
                tls_mitm: json_bool(&json, "network_proxy_tls_mitm").unwrap_or(true),
                header_rules: parse_proxy_header_rules(&json)?,
            },
            network_backend: parse_network_backend(&json)?,
        })
    }
}

impl Default for SandboxManager {
    fn default() -> Self {
        Self::new()
    }
}

fn default_state_root() -> PathBuf {
    if let Some(value) = env::var_os("TB_SANDBOX_STATE_DIR") {
        return PathBuf::from(value);
    }
    if cfg!(target_os = "macos") {
        home_dir().join("Library/Application Support/thousandbirds/agent-sandbox")
    } else if let Some(value) = env::var_os("XDG_STATE_HOME") {
        PathBuf::from(value).join("thousandbirds/agent-sandbox")
    } else {
        home_dir().join(".local/state/thousandbirds/agent-sandbox")
    }
}

fn default_cache_root() -> PathBuf {
    if let Some(value) = env::var_os("TB_SANDBOX_CACHE_DIR") {
        return PathBuf::from(value);
    }
    if cfg!(target_os = "macos") {
        home_dir().join("Library/Caches/thousandbirds/agent-sandbox")
    } else if let Some(value) = env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(value).join("thousandbirds/agent-sandbox")
    } else {
        home_dir().join(".cache/thousandbirds/agent-sandbox")
    }
}

fn gvproxy_socket_dir() -> Result<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from("/private/tmp")
    } else {
        env::temp_dir()
    };
    let path = base.join(format!(
        "tb-sandbox-gvproxy-{}-{}",
        std::process::id(),
        unix_millis()
    ));
    fs::create_dir_all(&path).map_err(|err| Error::io_path(&path, err))?;
    Ok(path)
}

fn available_localhost_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(Error::io)?;
    let port = listener.local_addr().map_err(Error::io)?.port();
    drop(listener);
    Ok(port)
}

fn network_static_profile(network_backend: &NetworkBackend) -> &'static str {
    match network_backend {
        NetworkBackend::Gvproxy { .. } | NetworkBackend::GvproxyAuto { .. } => "gvproxy",
        _ => "",
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Emit a wall-clock phase marker for startup profiling when `TB_SANDBOX_PROFILE`
/// is set. Lines are `tb-sandbox-profile <phase>=<unix_millis>` on stderr so
/// `scripts/profile-sandbox.sh` can break down host-side startup phases.
fn profile_mark(phase: &str) {
    if env::var_os("TB_SANDBOX_PROFILE").is_some() {
        eprintln!("tb-sandbox-profile {phase}={}", unix_millis());
    }
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn clone_or_copy_root(src: &Path, dst: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        let status = Command::new("cp")
            .arg("-c")
            .arg(src)
            .arg(dst)
            .status()
            .map_err(Error::io)?;
        if status.success() {
            return Ok(());
        }
    }
    fs::copy(src, dst).map_err(|err| Error::io_path(dst, err))?;
    Ok(())
}

/// Recursively copy a directory tree. On macOS uses `cp -cR` so shared blocks
/// stay APFS copy-on-write (matches `clone_or_copy_root`'s disk path);
/// elsewhere uses `cp -R` and falls back to a std walk on non-unix hosts.
///
/// `dst` must not exist — `cp -R src dst` creates `dst/` mirroring `src/`'s
/// contents only when `dst` does not yet exist. Snapshot/restore/fork callers
/// build a fresh parent directory and pass a never-yet-used child path.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        let status = Command::new("cp")
            .arg("-cR")
            .arg(src)
            .arg(dst)
            .status()
            .map_err(Error::io)?;
        if status.success() {
            return Ok(());
        }
    }
    #[cfg(unix)]
    {
        let status = Command::new("cp")
            .arg("-R")
            .arg(src)
            .arg(dst)
            .status()
            .map_err(Error::io)?;
        if status.success() {
            return Ok(());
        }
        return Err(Error::ProcessFailed {
            command: format!("cp -R {} {}", src.display(), dst.display()),
            status,
        });
    }
    #[cfg(not(unix))]
    {
        copy_tree_std(src, dst)
    }
}

#[cfg(not(unix))]
fn copy_tree_std(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).map_err(|err| Error::io_path(dst, err))?;
    for entry in fs::read_dir(src).map_err(|err| Error::io_path(src, err))? {
        let entry = entry.map_err(Error::io)?;
        let entry_src = entry.path();
        let entry_dst = dst.join(entry.file_name());
        if entry.file_type().map_err(Error::io)?.is_dir() {
            copy_tree_std(&entry_src, &entry_dst)?;
        } else {
            fs::copy(&entry_src, &entry_dst).map_err(|err| Error::io_path(entry_dst, err))?;
        }
    }
    Ok(())
}

fn find_gvproxy_executable(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        if is_executable_file(path) {
            return Ok(path.to_path_buf());
        }
        return Err(Error::NotFound(format!(
            "gvproxy executable at {}",
            path.display()
        )));
    }

    if let Some(path) = find_on_path("gvproxy") {
        return Ok(path);
    }

    for path in [
        "/opt/homebrew/opt/podman/libexec/podman/gvproxy",
        "/usr/local/opt/podman/libexec/podman/gvproxy",
    ] {
        let path = PathBuf::from(path);
        if is_executable_file(&path) {
            return Ok(path);
        }
    }

    if let Ok(output) = Command::new("brew").arg("--prefix").arg("podman").output() {
        if output.status.success() {
            let prefix = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !prefix.is_empty() {
                let path = PathBuf::from(prefix).join("libexec/podman/gvproxy");
                if is_executable_file(&path) {
                    return Ok(path);
                }
            }
        }
    }

    Err(Error::NotFound(
        "gvproxy executable; install it with `brew install podman` or pass --gvproxy-bin PATH"
            .to_string(),
    ))
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|path| is_executable_file(path))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn wait_for_socket(socket_path: &Path, child: &mut Child, log_path: &Path) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if socket_path.exists() {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(Error::io)? {
            return Err(Error::InvalidArgument(format!(
                "gvproxy exited before creating {}; status {}; log: {}",
                socket_path.display(),
                status,
                log_path.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(Error::InvalidArgument(format!(
        "gvproxy did not create {} within 5s; log: {}",
        socket_path.display(),
        log_path.display()
    )))
}

#[derive(Clone, Copy)]
struct TerminalSize {
    rows: u16,
    cols: u16,
}

fn write_terminal_env(dir: &Path) -> Result<()> {
    let path = dir.join("terminal.env");
    let size = host_terminal_size();
    let contents = format!(
        "AGENT_SANDBOX_TTY_ROWS={}\nAGENT_SANDBOX_TTY_COLS={}\n",
        shell_quote(&size.rows.to_string()),
        shell_quote(&size.cols.to_string())
    );
    fs::write(&path, contents).map_err(|err| Error::io_path(path, err))
}

fn host_terminal_size() -> TerminalSize {
    host_terminal_size_from_stdio()
        .or_else(host_terminal_size_from_env)
        .unwrap_or(TerminalSize { rows: 24, cols: 80 })
}

#[cfg(unix)]
fn host_terminal_size_from_stdio() -> Option<TerminalSize> {
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0;
        if ok && size.ws_row > 0 && size.ws_col > 0 {
            return Some(TerminalSize {
                rows: size.ws_row,
                cols: size.ws_col,
            });
        }
    }
    None
}

#[cfg(not(unix))]
fn host_terminal_size_from_stdio() -> Option<TerminalSize> {
    None
}

fn host_terminal_size_from_env() -> Option<TerminalSize> {
    let rows = env::var("LINES").ok()?.parse::<u16>().ok()?;
    let cols = env::var("COLUMNS").ok()?.parse::<u16>().ok()?;
    (rows > 0 && cols > 0).then_some(TerminalSize { rows, cols })
}

fn write_runtime_config(dir: &Path, config: &SandboxConfig, image: &AgentImage) -> Result<()> {
    let json_path = dir.join("shell.json");
    let env_path = dir.join("shell.env");
    let initial_command = config.shell.initial_command.as_deref().unwrap_or("");
    write_workspace_mounts_config(dir, &config.workspace_mounts)?;
    let contents = format!(
        concat!(
            "{{\n",
            "  \"version\": 1,\n",
            "  \"sandbox_name\": \"{}\",\n",
            "  \"guest_user\": \"{}\",\n",
            "  \"workspace\": \"{}\",\n",
            "  \"shell\": \"{}\",\n",
            "  \"initial_command\": \"{}\",\n",
            "  \"image_guest_user\": \"{}\"\n",
            "}}\n"
        ),
        crate::credentials::escape_json(config.name.as_deref().unwrap_or("sandbox")),
        crate::credentials::escape_json(&config.shell.guest_user),
        crate::credentials::escape_json(&config.shell.workspace),
        crate::credentials::escape_json(&config.shell.shell),
        crate::credentials::escape_json(initial_command),
        crate::credentials::escape_json(&image.guest_user)
    );
    fs::write(&json_path, contents).map_err(|err| Error::io_path(&json_path, err))?;

    let env_contents = format!(
        concat!(
            "SANDBOX_NAME={}\n",
            "GUEST_USER={}\n",
            "WORKSPACE={}\n",
            "SHELL_PATH={}\n",
            "INITIAL_COMMAND={}\n",
            "AGENT_SANDBOX_DHCP={}\n",
            "AGENT_SANDBOX_NET_STATIC={}\n",
            "AGENT_SANDBOX_PROFILE={}\n"
        ),
        shell_quote(config.name.as_deref().unwrap_or("sandbox")),
        shell_quote(&config.shell.guest_user),
        shell_quote(&config.shell.workspace),
        shell_quote(&config.shell.shell),
        shell_quote(initial_command),
        if config.network_backend.is_enabled() {
            "1"
        } else {
            "0"
        },
        shell_quote(network_static_profile(&config.network_backend)),
        shell_quote(if env::var_os("TB_SANDBOX_PROFILE").is_some() {
            "1"
        } else {
            ""
        })
    );
    fs::write(&env_path, env_contents).map_err(|err| Error::io_path(env_path, err))
}

fn write_workspace_mounts_config(dir: &Path, mounts: &[WorkspaceMount]) -> Result<()> {
    let path = dir.join("mounts.tsv");
    let mut contents = String::new();
    for mount in mounts {
        let tag = mount_config_field(&mount.tag, "mount tag")?;
        let guest_path = mount_config_field(&mount.guest_path, "guest mount path")?;
        contents.push_str(tag);
        contents.push('\t');
        contents.push_str(guest_path);
        contents.push('\t');
        contents.push_str(if mount.read_only { "true" } else { "false" });
        contents.push('\n');
    }
    fs::write(&path, contents).map_err(|err| Error::io_path(path, err))
}

fn mount_config_field<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    if value.is_empty() || value.contains('\n') || value.contains('\r') || value.contains('\t') {
        return Err(Error::InvalidArgument(format!(
            "{label} must be non-empty and must not contain tabs or newlines"
        )));
    }
    Ok(value)
}

fn write_initial_command_env(dir: &Path, initial_command: &str) -> Result<()> {
    let env_path = dir.join("shell.env");
    let contents = fs::read_to_string(&env_path).map_err(|err| Error::io_path(&env_path, err))?;
    let mut found = false;
    let mut updated = String::new();
    for line in contents.lines() {
        if line.starts_with("INITIAL_COMMAND=") {
            found = true;
            updated.push_str("INITIAL_COMMAND=");
            updated.push_str(&shell_quote(initial_command));
            updated.push('\n');
        } else {
            updated.push_str(line);
            updated.push('\n');
        }
    }
    if !found {
        updated.push_str("INITIAL_COMMAND=");
        updated.push_str(&shell_quote(initial_command));
        updated.push('\n');
    }
    fs::write(&env_path, updated).map_err(|err| Error::io_path(env_path, err))
}

fn write_proxy_env(dir: &Path, proxy: Option<&NetworkProxy>) -> Result<()> {
    let path = dir.join("proxy.env");
    let ca_path = dir.join("mitm-ca.pem");
    if let Some(proxy) = proxy {
        let url = proxy.url();
        let mut contents = format!(
            concat!(
                "export AGENT_SANDBOX_PROXY={}\n",
                "export HTTP_PROXY={}\n",
                "export HTTPS_PROXY={}\n",
                "export ALL_PROXY={}\n",
                "export http_proxy={}\n",
                "export https_proxy={}\n",
                "export all_proxy={}\n",
                "export NO_PROXY={}\n",
                "export no_proxy={}\n"
            ),
            shell_quote(&url),
            shell_quote(&url),
            shell_quote(&url),
            shell_quote(&url),
            shell_quote(&url),
            shell_quote(&url),
            shell_quote(&url),
            shell_quote("localhost,127.0.0.1,::1"),
            shell_quote("localhost,127.0.0.1,::1")
        );
        if let Some(ca_pem) = proxy.ca_certificate_pem() {
            fs::write(&ca_path, ca_pem).map_err(|err| Error::io_path(&ca_path, err))?;
            contents.push_str(&format!(
                concat!(
                    "export AGENT_SANDBOX_MITM_CA={}\n",
                    "export NODE_EXTRA_CA_CERTS={}\n",
                    "export REQUESTS_CA_BUNDLE={}\n",
                    "export SSL_CERT_FILE={}\n",
                    "export GIT_SSL_CAINFO={}\n",
                    "export NPM_CONFIG_CAFILE={}\n",
                    "export CARGO_HTTP_CAINFO={}\n"
                ),
                shell_quote("/run/agent-sandbox/config/mitm-ca.pem"),
                shell_quote("/usr/local/share/ca-certificates/agent-sandbox-mitm-ca.crt"),
                shell_quote("/etc/ssl/certs/ca-certificates.crt"),
                shell_quote("/etc/ssl/certs/ca-certificates.crt"),
                shell_quote("/etc/ssl/certs/ca-certificates.crt"),
                shell_quote("/etc/ssl/certs/ca-certificates.crt"),
                shell_quote("/etc/ssl/certs/ca-certificates.crt")
            ));
        } else {
            remove_optional_file(&ca_path)?;
        }
        fs::write(&path, contents).map_err(|err| Error::io_path(path, err))
    } else {
        remove_optional_file(&path)?;
        remove_optional_file(&ca_path)
    }
}

fn remove_optional_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io_path(path, err)),
    }
}

/// Send a single line command to a sandbox's control socket and return the
/// one-line response from the VMM.
#[cfg(unix)]
fn send_control_command(socket: &Path, command: &str) -> Result<String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(socket).map_err(|err| Error::io_path(socket, err))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(Error::io)?;
    let mut writer = stream.try_clone().map_err(Error::io)?;
    writeln!(writer, "{command}").map_err(Error::io)?;
    writer.flush().map_err(Error::io)?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(Error::io)?;
    Ok(line.trim().to_string())
}

#[cfg(not(unix))]
fn send_control_command(_socket: &Path, _command: &str) -> Result<String> {
    Err(Error::InvalidArgument(
        "runtime memory resize is only supported on unix platforms".to_string(),
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn print_prepared_credentials(prepared: &PreparedCredentials) {
    eprintln!(
        "prepared {} credential path(s), {} env value(s); manifest: {}",
        prepared.copied.len(),
        prepared.env_names.len(),
        prepared.manifest_path.display()
    );
    for copy in &prepared.copied {
        let suffix = if copy.private_key {
            " [private key]"
        } else {
            ""
        };
        eprintln!("  {} -> {}{}", copy.source_label, copy.guest_path, suffix);
    }
}

fn write_sandbox_metadata(path: &Path, stored: &StoredSandbox) -> Result<()> {
    let mut mounts = Vec::new();
    for mount in &stored.workspace_mounts {
        mounts.push(format!(
            "{}|{}|{}|{}",
            mount.tag,
            mount.host_path.display(),
            mount.guest_path,
            mount.read_only
        ));
    }
    let credentials = stored
        .credentials_staging
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let contents = format!(
        concat!(
            "{{\n",
            "  \"version\": 1,\n",
            "  \"id\": \"{}\",\n",
            "  \"image_path\": \"{}\",\n",
            "  \"root_disk\": \"{}\",\n",
            "  \"runtime_config\": \"{}\",\n",
            "  \"runtime_control\": \"{}\",\n",
            "  \"credentials_staging\": \"{}\",\n",
            "  \"cpus\": {},\n",
            "  \"memory_mib\": {},\n",
            "  \"memory_max_mib\": {},\n",
            "  \"memory_min_mib\": {},\n",
            "  \"virtiofs_dax_size\": {},\n",
            "  \"ephemeral\": {},\n",
            "  \"network_proxy_enabled\": {},\n",
            "  \"network_proxy_tls_mitm\": {},\n",
            "  \"network_backend\": \"{}\",\n",
            "  \"proxy_header_rules\": [{}],\n",
            "  \"workspace_mounts\": [{}]\n",
            "}}\n"
        ),
        crate::credentials::escape_json(stored.id.as_str()),
        crate::credentials::escape_json(&stored.image_path.display().to_string()),
        crate::credentials::escape_json(&stored.root_disk.display().to_string()),
        crate::credentials::escape_json(&stored.runtime_config.display().to_string()),
        crate::credentials::escape_json(&stored.runtime_control.display().to_string()),
        crate::credentials::escape_json(&credentials),
        stored.cpus,
        stored.memory_mib,
        stored.memory_max_mib,
        stored.memory_min_mib,
        stored.virtiofs_dax_size,
        stored.ephemeral,
        stored.network_proxy.enabled,
        stored.network_proxy.tls_mitm,
        crate::credentials::escape_json(&stored.network_backend.encode()),
        stored
            .network_proxy
            .header_rules
            .iter()
            .map(|rule| format!("\"{}\"", crate::credentials::escape_json(&rule.encode())))
            .collect::<Vec<_>>()
            .join(", "),
        mounts
            .iter()
            .map(|mount| format!("\"{}\"", crate::credentials::escape_json(mount)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    fs::write(path, contents).map_err(|err| Error::io_path(path, err))
}

fn required_string(json: &str, key: &str) -> Result<String> {
    json_string(json, key)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::InvalidArgument(format!("sandbox metadata is missing required field {key}"))
        })
}

fn parse_mounts(json: &str) -> Result<Vec<WorkspaceMount>> {
    let Some(items) = json_string_array(json, "workspace_mounts") else {
        return Ok(Vec::new());
    };
    let mut mounts = Vec::new();
    for item in items {
        let parts: Vec<_> = item.split('|').collect();
        if parts.len() != 4 {
            return Err(Error::InvalidArgument(format!(
                "invalid workspace mount metadata: {item}"
            )));
        }
        mounts.push(WorkspaceMount {
            tag: parts[0].to_string(),
            host_path: PathBuf::from(parts[1]),
            guest_path: parts[2].to_string(),
            read_only: parts[3] == "true",
        });
    }
    Ok(mounts)
}

fn parse_proxy_header_rules(json: &str) -> Result<Vec<ProxyHeaderRule>> {
    let Some(items) = json_string_array(json, "proxy_header_rules") else {
        return Ok(Vec::new());
    };
    let mut rules = Vec::new();
    for item in items {
        let (target, value) = item.split_once('=').ok_or_else(|| {
            Error::InvalidArgument(format!("invalid proxy header rule metadata: {item}"))
        })?;
        let (host, header) = target.split_once(':').ok_or_else(|| {
            Error::InvalidArgument(format!("invalid proxy header rule metadata: {item}"))
        })?;
        let (value, value_from_env) = match value.strip_prefix("@env:") {
            Some(env_name) => (env_name.to_string(), true),
            None => (value.to_string(), false),
        };
        rules.push(ProxyHeaderRule {
            host: host.to_string(),
            header: header.to_string(),
            value,
            value_from_env,
        });
    }
    Ok(rules)
}

fn parse_network_backend(json: &str) -> Result<NetworkBackend> {
    let Some(value) = json_string(json, "network_backend") else {
        return Ok(NetworkBackend::None);
    };
    if value == "none" || value.is_empty() {
        return Ok(NetworkBackend::None);
    }
    if let Some(socket) = value.strip_prefix("passt:") {
        return Ok(NetworkBackend::Passt {
            socket: PathBuf::from(socket),
        });
    }
    if let Some(socket) = value.strip_prefix("gvproxy:") {
        return Ok(NetworkBackend::Gvproxy {
            socket: PathBuf::from(socket),
        });
    }
    if value == "gvproxy-auto" {
        return Ok(NetworkBackend::GvproxyAuto { executable: None });
    }
    if let Some(executable) = value.strip_prefix("gvproxy-auto:") {
        return Ok(NetworkBackend::GvproxyAuto {
            executable: Some(PathBuf::from(executable)),
        });
    }
    Err(Error::InvalidArgument(format!(
        "invalid network backend metadata: {value}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentImageRef;
    use crate::snapshot::{ForkOptions, RestoreOptions, SnapshotId, SnapshotOptions};

    #[test]
    fn workspace_mounts_config_records_declared_guest_paths() {
        let root = test_root("workspace_mounts_config_records_declared_guest_paths");
        fs::create_dir_all(&root).unwrap();
        write_workspace_mounts_config(
            &root,
            &[
                WorkspaceMount {
                    tag: "generation".to_string(),
                    host_path: PathBuf::from("/host/generation"),
                    guest_path: "/workspace/generation".to_string(),
                    read_only: false,
                },
                WorkspaceMount {
                    tag: "meta-agent".to_string(),
                    host_path: PathBuf::from("/host/meta-agent"),
                    guest_path: "/workspace/meta-agent".to_string(),
                    read_only: true,
                },
            ],
        )
        .unwrap();

        let contents = fs::read_to_string(root.join("mounts.tsv")).unwrap();
        assert_eq!(
            contents,
            "generation\t/workspace/generation\tfalse\nmeta-agent\t/workspace/meta-agent\ttrue\n"
        );
    }

    #[test]
    fn workspace_mounts_config_rejects_ambiguous_fields() {
        let root = test_root("workspace_mounts_config_rejects_ambiguous_fields");
        fs::create_dir_all(&root).unwrap();
        let err = write_workspace_mounts_config(
            &root,
            &[WorkspaceMount {
                tag: "bad\ttag".to_string(),
                host_path: PathBuf::from("/host/generation"),
                guest_path: "/workspace/generation".to_string(),
                read_only: false,
            }],
        )
        .unwrap_err();
        assert!(err.to_string().contains("mount tag"));
    }

    fn test_root(name: &str) -> PathBuf {
        let root =
            env::temp_dir().join(format!("tb-sandbox-runtime-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    // -- snapshot / restore / fork --------------------------------------------

    const FAKE_DISK: &[u8] = b"root-disk-contents";

    /// Construct a manager pointing at a temp state root plus a minimal fake
    /// agent image. The image just needs kernel + root.raw + image.json to
    /// satisfy `AgentImage::load` — no launching happens in these tests.
    fn snapshot_fixture(label: &str) -> (SandboxManager, PathBuf) {
        let root = env::temp_dir().join(format!(
            "tb-sandbox-snapshot-{label}-{}-{}",
            std::process::id(),
            unix_millis()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let image = root.join("image");
        fs::create_dir_all(&image).unwrap();
        fs::write(image.join("kernel"), b"kernel").unwrap();
        fs::write(image.join("root.raw"), FAKE_DISK).unwrap();
        fs::write(
            image.join("image.json"),
            format!(
                r#"{{
  "architecture": "{}",
  "kernel": "kernel",
  "root_disk": "root.raw"
}}"#,
                env::consts::ARCH
            ),
        )
        .unwrap();
        let manager = SandboxManager {
            state_root: root.join("state"),
            cache_root: root.join("cache"),
        };
        (manager, image)
    }

    /// Build a sandbox + fake a credentials-staging dir alongside it, so the
    /// snapshot/fork credentials paths can be exercised without running
    /// `prepare_credentials` (which reads host secrets).
    fn create_with_staged_credentials(
        manager: &SandboxManager,
        image: &Path,
        name: &str,
    ) -> Sandbox {
        let mut config = SandboxConfig::new(AgentImageRef::new(image));
        config.name = Some(name.to_string());
        let sandbox = manager.create(config).unwrap();
        let staging = sandbox.state_dir.join("credentials-staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("token"), b"secret").unwrap();
        let meta_path = sandbox.state_dir.join("sandbox.json");
        let meta = fs::read_to_string(&meta_path).unwrap();
        let new_meta = meta.replace(
            "\"credentials_staging\": \"\"",
            &format!("\"credentials_staging\": \"{}\"", staging.display()),
        );
        assert_ne!(new_meta, meta, "expected to rewrite credentials_staging");
        fs::write(&meta_path, new_meta).unwrap();
        sandbox
    }

    #[test]
    fn snapshot_writes_manifest_and_clones_disk() {
        let (manager, image) = snapshot_fixture("basic");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();

        let snap_id = manager
            .snapshot(&sandbox.id, SnapshotOptions::default())
            .unwrap();
        let snap_dir = manager.snapshot_dir(&snap_id);
        assert!(snap_dir.join("manifest.json").exists());
        assert!(snap_dir.join("root.raw").exists());
        assert_eq!(fs::read(snap_dir.join("root.raw")).unwrap(), FAKE_DISK);

        let manifest_json = manager.inspect_snapshot(&snap_id).unwrap();
        let manifest = crate::snapshot::SnapshotManifest::from_json(&manifest_json).unwrap();
        assert_eq!(manifest.parent_sandbox_id, "src");
        assert_eq!(manifest.image_path, image.canonicalize().unwrap());
        assert_eq!(manifest.cpus, 1);
        assert_eq!(manifest.memory_mib, 2048);
        assert!(!manifest.had_credentials);
    }

    #[test]
    fn snapshot_with_explicit_name_uses_it() {
        let (manager, image) = snapshot_fixture("named");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();

        let snap_id = manager
            .snapshot(
                &sandbox.id,
                SnapshotOptions {
                    name: Some("my-snap".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(snap_id.as_str(), "my-snap");
        assert!(manager.snapshot_dir(&snap_id).exists());
    }

    #[test]
    fn snapshot_id_collision_fails_without_clobbering() {
        let (manager, image) = snapshot_fixture("collision");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();

        let opts = SnapshotOptions {
            name: Some("dup".to_string()),
            ..Default::default()
        };
        manager.snapshot(&sandbox.id, opts.clone()).unwrap();
        let err = manager.snapshot(&sandbox.id, opts).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // The pre-existing snapshot is intact.
        let snap_id = SnapshotId::new("dup").unwrap();
        assert!(manager
            .snapshot_dir(&snap_id)
            .join("manifest.json")
            .exists());
    }

    #[test]
    fn snapshot_bundles_credentials_when_present() {
        let (manager, image) = snapshot_fixture("bundles-creds");
        let sandbox = create_with_staged_credentials(&manager, &image, "src");

        let snap_id = manager
            .snapshot(&sandbox.id, SnapshotOptions::default())
            .unwrap();
        let snap_dir = manager.snapshot_dir(&snap_id);
        assert!(snap_dir.join("credentials-staging").exists());
        let token = fs::read(snap_dir.join("credentials-staging/token")).unwrap();
        assert_eq!(token, b"secret");
        let manifest_json = manager.inspect_snapshot(&snap_id).unwrap();
        assert!(manifest_json.contains("\"had_credentials\": true"));
    }

    #[test]
    fn snapshot_fails_for_missing_sandbox() {
        let (manager, _image) = snapshot_fixture("missing");
        let bogus = SandboxId::new("nothing-here").unwrap();
        let err = manager
            .snapshot(&bogus, SnapshotOptions::default())
            .unwrap_err();
        // load() opens sandbox.json which doesn't exist; expect an IO error.
        assert!(matches!(err, Error::Io { .. }));
    }

    #[test]
    fn restore_creates_new_sandbox_from_snapshot() {
        let (manager, image) = snapshot_fixture("restore-basic");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();
        let snap_id = manager
            .snapshot(&sandbox.id, SnapshotOptions::default())
            .unwrap();

        let opts = RestoreOptions {
            name: Some("restored".to_string()),
            ..Default::default()
        };
        let new = manager.restore(&snap_id, opts).unwrap();
        assert_eq!(new.id.as_str(), "restored");
        assert!(new.state_dir.join("sandbox.json").exists());
        assert!(new.state_dir.join("root.raw").exists());
        assert_eq!(fs::read(new.state_dir.join("root.raw")).unwrap(), FAKE_DISK);

        // APFS CoW (or plain copy fallback) means writes to the restored disk
        // do not leak back into the snapshot.
        fs::write(new.state_dir.join("root.raw"), b"diverged").unwrap();
        let snap_root = fs::read(manager.snapshot_dir(&snap_id).join("root.raw")).unwrap();
        assert_eq!(snap_root, FAKE_DISK);
    }

    #[test]
    fn restore_default_name_is_unique() {
        let (manager, image) = snapshot_fixture("restore-default-name");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();
        let snap_id = manager
            .snapshot(&sandbox.id, SnapshotOptions::default())
            .unwrap();
        let restored = manager
            .restore(&snap_id, RestoreOptions::default())
            .unwrap();
        assert!(restored.id.as_str().starts_with("restore-"));
    }

    #[test]
    fn restore_with_overrides_applies_them() {
        let (manager, image) = snapshot_fixture("restore-overrides");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        config.cpus = 1;
        config.memory_mib = 1024;
        let sandbox = manager.create(config).unwrap();
        let snap_id = manager
            .snapshot(&sandbox.id, SnapshotOptions::default())
            .unwrap();

        let opts = RestoreOptions {
            name: Some("custom".to_string()),
            cpus: Some(4),
            memory_mib: Some(4096),
            memory_max_mib: Some(8192),
            workspace_mounts: Some(Vec::new()),
            ..Default::default()
        };
        let new = manager.restore(&snap_id, opts).unwrap();
        let json = manager.inspect(&new.id).unwrap();
        assert!(json.contains("\"cpus\": 4"));
        assert!(json.contains("\"memory_mib\": 4096"));
        assert!(json.contains("\"memory_max_mib\": 8192"));
    }

    #[test]
    fn restore_inherits_credentials_unless_dropped() {
        let (manager, image) = snapshot_fixture("restore-creds");
        let sandbox = create_with_staged_credentials(&manager, &image, "src");
        let snap_id = manager
            .snapshot(&sandbox.id, SnapshotOptions::default())
            .unwrap();

        let new = manager
            .restore(
                &snap_id,
                RestoreOptions {
                    name: Some("with-creds".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(new.state_dir.join("credentials-staging/token").exists());

        let dropped = manager
            .restore(
                &snap_id,
                RestoreOptions {
                    name: Some("no-creds".to_string()),
                    drop_credentials: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!dropped.state_dir.join("credentials-staging").exists());
    }

    #[test]
    fn restore_fails_for_missing_snapshot() {
        let (manager, _image) = snapshot_fixture("restore-missing");
        let bogus = SnapshotId::new("no-such-snap").unwrap();
        let err = manager
            .restore(&bogus, RestoreOptions::default())
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn fork_inherits_parent_config_and_diverges_on_write() {
        let (manager, image) = snapshot_fixture("fork-basic");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("parent".to_string());
        config.cpus = 2;
        config.memory_mib = 4096;
        let parent = manager.create(config).unwrap();

        let child = manager
            .fork(
                &parent.id,
                ForkOptions {
                    name: Some("child".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        let json = manager.inspect(&child.id).unwrap();
        assert!(json.contains("\"cpus\": 2"));
        assert!(json.contains("\"memory_mib\": 4096"));
        assert_eq!(
            fs::read(child.state_dir.join("root.raw")).unwrap(),
            FAKE_DISK
        );

        // Diverge: write to the child disk, parent is unaffected.
        fs::write(child.state_dir.join("root.raw"), b"child-write").unwrap();
        assert_eq!(
            fs::read(parent.state_dir.join("root.raw")).unwrap(),
            FAKE_DISK
        );
    }

    #[test]
    fn fork_default_name_derives_from_parent() {
        let (manager, image) = snapshot_fixture("fork-name");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("origin".to_string());
        let parent = manager.create(config).unwrap();
        let child = manager.fork(&parent.id, ForkOptions::default()).unwrap();
        assert!(child.id.as_str().starts_with("origin-fork-"));
    }

    #[test]
    fn fork_inherits_credentials_unless_dropped() {
        let (manager, image) = snapshot_fixture("fork-creds");
        let parent = create_with_staged_credentials(&manager, &image, "parent");

        let inherited = manager
            .fork(
                &parent.id,
                ForkOptions {
                    name: Some("inheritor".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(inherited
            .state_dir
            .join("credentials-staging/token")
            .exists());

        let dropped = manager
            .fork(
                &parent.id,
                ForkOptions {
                    name: Some("dropper".to_string()),
                    drop_credentials: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!dropped.state_dir.join("credentials-staging").exists());
    }

    #[test]
    fn snapshots_list_skips_malformed_directories() {
        let (manager, image) = snapshot_fixture("snapshots-list");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();
        manager
            .snapshot(
                &sandbox.id,
                SnapshotOptions {
                    name: Some("a".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        manager
            .snapshot(
                &sandbox.id,
                SnapshotOptions {
                    name: Some("b".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        // Junk directory: no manifest -> skipped on list.
        let junk = manager.state_root.join("snapshots/garbage");
        fs::create_dir_all(&junk).unwrap();

        let summaries = manager.snapshots().unwrap();
        let ids: Vec<_> = summaries
            .iter()
            .map(|s| s.id.as_str().to_string())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
        for s in &summaries {
            assert_eq!(s.root_disk_bytes, Some(FAKE_DISK.len() as u64));
            assert_eq!(s.parent_sandbox_id, "src");
        }
    }

    #[test]
    fn delete_snapshot_removes_directory() {
        let (manager, image) = snapshot_fixture("snapshot-delete");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();
        let snap_id = manager
            .snapshot(&sandbox.id, SnapshotOptions::default())
            .unwrap();
        let dir = manager.snapshot_dir(&snap_id);
        assert!(dir.exists());
        manager.delete_snapshot(&snap_id).unwrap();
        assert!(!dir.exists());
        let err = manager.delete_snapshot(&snap_id).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    // -- cooperative quiesce on running snapshots ----------------------------

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Mark a sandbox as running by writing a `running.json` marker whose PID
    /// is the current test process — `pid_is_running` will return true for
    /// the lifetime of the test.
    fn mark_running(manager: &SandboxManager, sandbox: &Sandbox) {
        ResourceLedger::from_env(&manager.state_root)
            .record_running(
                &sandbox.state_dir,
                sandbox.id.as_str(),
                std::process::id(),
                0,
            )
            .unwrap();
    }

    /// Spawn a thread that plays the in-guest quiesce daemon on
    /// `runtime_control`. Mirrors the real `agent-sandbox-quiesce` script:
    /// reads `quiesce.req`, writes `quiesce.ack`, waits for `resume.req`,
    /// writes `resume.ack`. Exits when `stop` is set.
    fn spawn_quiesce_fake_guest(runtime_control: PathBuf, stop: Arc<AtomicBool>) {
        std::thread::spawn(move || {
            let read_field = |path: &Path, key: &str| -> Option<String> {
                let text = fs::read_to_string(path).ok()?;
                for line in text.lines() {
                    let line = line.trim();
                    if let Some(value) = line.strip_prefix(&format!("{key}=")) {
                        return Some(value.to_string());
                    }
                }
                None
            };
            let write_atomic = |path: &Path, content: &str| {
                let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
                fs::write(&tmp, content).unwrap();
                fs::rename(&tmp, path).unwrap();
            };

            let mut handled_quiesce_id: Option<String> = None;
            while !stop.load(Ordering::SeqCst) {
                if handled_quiesce_id.is_none() {
                    let req_path = runtime_control.join("quiesce.req");
                    if let Some(id) = read_field(&req_path, "id") {
                        let action =
                            read_field(&req_path, "action").unwrap_or_else(|| "sync".to_string());
                        let ack = format!(
                            "id={id}\nstatus=ok\naction={action}\nachieved_unix_millis={}\n",
                            unix_millis()
                        );
                        write_atomic(&runtime_control.join("quiesce.ack"), &ack);
                        handled_quiesce_id = Some(id);
                    }
                } else {
                    let resume_path = runtime_control.join("resume.req");
                    if let Some(id) = read_field(&resume_path, "id") {
                        let ack = format!(
                            "id={id}\nstatus=ok\nresumed_unix_millis={}\n",
                            unix_millis()
                        );
                        write_atomic(&runtime_control.join("resume.ack"), &ack);
                        // Sweep so a follow-on cycle on the same dir starts
                        // clean (matches what the real daemon does).
                        let _ = fs::remove_file(runtime_control.join("quiesce.req"));
                        let _ = fs::remove_file(runtime_control.join("resume.req"));
                        handled_quiesce_id = None;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }

    #[test]
    fn snapshot_of_running_sandbox_quiesces_before_clone() {
        let (manager, image) = snapshot_fixture("quiesce-running");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();
        mark_running(&manager, &sandbox);

        let runtime_control = sandbox.state_dir.join("runtime-control");
        let stop = Arc::new(AtomicBool::new(false));
        spawn_quiesce_fake_guest(runtime_control.clone(), stop.clone());

        // Default options use freeze action; the real guest would downgrade
        // to sync — the fake guest echoes back whatever action it was asked
        // for, which is fine for protocol verification.
        let snap_id = manager
            .snapshot(&sandbox.id, SnapshotOptions::default())
            .unwrap();
        stop.store(true, Ordering::SeqCst);

        let manifest_json = manager.inspect_snapshot(&snap_id).unwrap();
        let manifest = crate::snapshot::SnapshotManifest::from_json(&manifest_json).unwrap();
        assert!(
            manifest.quiesced,
            "running sandbox snapshot should set quiesced=true; manifest: {manifest_json}"
        );
        assert_eq!(manifest.quiesce_action.as_deref(), Some("freeze"));
        assert!(manifest.quiesced_unix_millis.is_some());
    }

    #[test]
    fn snapshot_records_sync_action_when_freeze_disabled() {
        let (manager, image) = snapshot_fixture("quiesce-sync-action");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();
        mark_running(&manager, &sandbox);

        let stop = Arc::new(AtomicBool::new(false));
        spawn_quiesce_fake_guest(sandbox.state_dir.join("runtime-control"), stop.clone());

        let snap_id = manager
            .snapshot(
                &sandbox.id,
                SnapshotOptions {
                    quiesce_freeze: false,
                    ..Default::default()
                },
            )
            .unwrap();
        stop.store(true, Ordering::SeqCst);

        let manifest = crate::snapshot::SnapshotManifest::from_json(
            &manager.inspect_snapshot(&snap_id).unwrap(),
        )
        .unwrap();
        assert!(manifest.quiesced);
        assert_eq!(manifest.quiesce_action.as_deref(), Some("sync"));
    }

    #[test]
    fn snapshot_skip_quiesce_does_not_touch_runtime_control() {
        let (manager, image) = snapshot_fixture("quiesce-skip");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();
        mark_running(&manager, &sandbox);

        // No fake guest — if quiesce ran, the snapshot would time out.
        let snap_id = manager
            .snapshot(
                &sandbox.id,
                SnapshotOptions {
                    skip_quiesce: true,
                    ..Default::default()
                },
            )
            .unwrap();

        let manifest = crate::snapshot::SnapshotManifest::from_json(
            &manager.inspect_snapshot(&snap_id).unwrap(),
        )
        .unwrap();
        assert!(!manifest.quiesced);
        assert_eq!(manifest.quiesce_action, None);
        assert_eq!(manifest.quiesced_unix_millis, None);
        // The wrapper should not have created any quiesce.* files when
        // skipping (the runtime-control dir stays clean).
        let rc = sandbox.state_dir.join("runtime-control");
        assert!(!rc.join("quiesce.req").exists());
        assert!(!rc.join("quiesce.ack").exists());
    }

    #[test]
    fn snapshot_of_stopped_sandbox_skips_quiesce_silently() {
        // The pre-existing snapshot_writes_manifest_and_clones_disk test
        // covers the happy path for stopped sandboxes; here we just assert
        // the manifest fields reflect "no quiesce" without an explicit opt-
        // out (because is_running() returned false).
        let (manager, image) = snapshot_fixture("quiesce-stopped");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();
        // Note: no mark_running.

        let snap_id = manager
            .snapshot(&sandbox.id, SnapshotOptions::default())
            .unwrap();
        let manifest = crate::snapshot::SnapshotManifest::from_json(
            &manager.inspect_snapshot(&snap_id).unwrap(),
        )
        .unwrap();
        assert!(!manifest.quiesced);
        assert_eq!(manifest.quiesce_action, None);
    }

    // -- Phase 3 surface (memory snapshot) -----------------------------------

    #[test]
    fn restore_of_full_kind_snapshot_returns_typed_error_today() {
        // Phase 3 symmetric guard: a Full-kind manifest (memory snapshot)
        // is refused at restore time rather than silently cold-booting
        // from the disk and discarding the captured memory.
        let (manager, image) = snapshot_fixture("restore-full-unimplemented");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();
        let snap_id = manager
            .snapshot(
                &sandbox.id,
                SnapshotOptions {
                    name: Some("masquerade".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();

        // Forge a Full-kind manifest on disk — simulates a snapshot
        // captured by a future libkrun that supports memory capture.
        let manifest_path = manager.snapshot_dir(&snap_id).join("manifest.json");
        let json = fs::read_to_string(&manifest_path).unwrap();
        let forged = json
            .replace("\"kind\": \"disk-only\"", "\"kind\": \"full\"")
            .replace(
                "\"memory_image_path\": \"\"",
                "\"memory_image_path\": \"memory.bin\"",
            );
        fs::write(&manifest_path, forged).unwrap();

        let err = manager
            .restore(&snap_id, RestoreOptions::default())
            .expect_err("restore of Full-kind should fail loudly today");
        assert!(
            err.to_string().contains("Full-kind") && err.to_string().contains("ENOSYS"),
            "got: {err}"
        );
    }

    #[test]
    fn snapshot_with_capture_memory_returns_typed_error_today() {
        // Phase 3 forward-compat: callers can set capture_memory=true now;
        // the wrapper surfaces a clean error pointing at the gap plan
        // rather than silently downgrading to a disk-only snapshot.
        let (manager, image) = snapshot_fixture("capture-memory-unimplemented");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();

        let err = manager
            .snapshot(
                &sandbox.id,
                SnapshotOptions {
                    capture_memory: true,
                    ..Default::default()
                },
            )
            .expect_err("capture_memory should surface unimplemented today");
        let msg = err.to_string();
        assert!(
            msg.contains("capture_memory") && msg.contains("Phase 3"),
            "got: {msg}"
        );
        // No snapshot artifact was created.
        let snapshots_dir = manager.state_root.join("snapshots");
        if snapshots_dir.exists() {
            assert!(
                fs::read_dir(&snapshots_dir).unwrap().next().is_none(),
                "no snapshot dirs should be left behind on error"
            );
        }
    }

    #[test]
    fn snapshot_of_running_sandbox_fails_when_guest_unresponsive() {
        let (manager, image) = snapshot_fixture("quiesce-timeout");
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("src".to_string());
        let sandbox = manager.create(config).unwrap();
        mark_running(&manager, &sandbox);
        // No fake guest spawned: the daemon never acks the quiesce.req.

        let err = manager
            .snapshot(
                &sandbox.id,
                SnapshotOptions {
                    name: Some("doomed".to_string()),
                    quiesce_timeout: Some(Duration::from_millis(200)),
                    ..Default::default()
                },
            )
            .expect_err("snapshot should fail when guest does not ack quiesce");
        assert!(
            err.to_string().contains("quiesce.ack") || err.to_string().contains("guest quiesce"),
            "got error: {err}"
        );
        // Snapshot directory must be cleaned up on failure.
        let snap_id = SnapshotId::new("doomed").unwrap();
        assert!(!manager.snapshot_dir(&snap_id).exists());
        // Best-effort resume.req was still written (defense-in-depth so the
        // guest unfreezes if it later wakes up).
        assert!(sandbox
            .state_dir
            .join("runtime-control/resume.req")
            .exists());
    }
}
