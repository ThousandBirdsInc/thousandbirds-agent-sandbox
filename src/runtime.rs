use crate::config::{
    NetworkBackend, NetworkProxyConfig, ProxyHeaderRule, SandboxConfig, WorkspaceMount,
};
use crate::credentials::{prepare_credentials, PreparedCredentials};
use crate::error::{Error, Result};
use crate::image::{json_bool, json_number, json_string, json_string_array, AgentImage};
use crate::libkrun::{launch_command, LaunchConfig};
use crate::proxy::NetworkProxy;
use crate::scheduler::ResourceLedger;
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
        ResourceLedger::from_env(&self.state_root)
            .admit(id.as_str(), stored.memory_mib as u64)?;
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
        ResourceLedger::from_env(&self.state_root)
            .admit(id.as_str(), stored.memory_mib as u64)?;
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
}
