use crate::config::SandboxConfig;
use crate::error::{Error, Result};
use crate::runtime::{Sandbox, SandboxId, SandboxManager, WorkspaceMountInfo};
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::io::SeekFrom;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{sleep, Duration};

/// Capacity of the stdout/stderr line channels handed back in [`GuestProcess`].
/// The producer applies backpressure when the channel is full, so consumers
/// MUST drain both streams (see [`GuestProcess`]).
const STREAM_CHANNEL_CAPACITY: usize = 64;
/// How often the host polls response files / child exit while tailing output.
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// How often the host port-forwarder polls the relay file for new guest bytes.
const FORWARD_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How long the SDK waits for an active session's command server to report
/// ready before giving up.
const SESSION_READY_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestCommand {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
}

impl GuestCommand {
    pub fn new(argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            env: Vec::new(),
            cwd: None,
        }
    }

    /// Append an environment variable, returning `self` for chaining.
    /// Equivalent to pushing onto the public `env` field.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Set the working directory, returning `self` for chaining.
    pub fn cwd(mut self, dir: impl Into<String>) -> Self {
        self.cwd = Some(dir.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestExitStatus {
    pub code: Option<i32>,
    pub success: bool,
}

/// A running guest process.
///
/// Backpressure contract: `stdout` and `stderr` are bounded channels
/// ([`STREAM_CHANNEL_CAPACITY`]). The producer blocks when a channel fills, so
/// a consumer that reads only one stream (or neither) can stall the guest
/// process. Callers MUST drain both `stdout` and `stderr` — typically in a
/// `tokio::select!` loop alongside `exit` — until they close. If output is not
/// needed, spawn a task that drains and discards each stream.
#[derive(Debug)]
pub struct GuestProcess {
    pub id: String,
    pub stdout: mpsc::Receiver<String>,
    pub stderr: mpsc::Receiver<String>,
    pub exit: oneshot::Receiver<Result<GuestExitStatus>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForward {
    pub host: String,
    pub port: u16,
}

#[async_trait]
pub trait SandboxRuntimeSdk: Send + Sync {
    async fn create_session(&self, config: SandboxConfig) -> Result<Sandbox>;
    async fn reopen_session(&self, id: &SandboxId) -> Result<Sandbox>;
    async fn start_session(&self, id: &SandboxId) -> Result<()>;
    async fn exec(&self, id: &SandboxId, command: GuestCommand) -> Result<GuestProcess>;
    async fn forward_port(&self, id: &SandboxId, guest_port: u16) -> Result<PortForward>;
    async fn inspect_mounts(&self, id: &SandboxId) -> Result<Vec<WorkspaceMountInfo>>;
    async fn cancel_process(&self, id: &SandboxId, process_id: &str) -> Result<()>;
    async fn stop_session(&self, id: &SandboxId) -> Result<()>;
    async fn delete_session(&self, id: &SandboxId) -> Result<()>;
}

/// A live port forward owned by this SDK instance: the host-side relay task and
/// the guest-side forwarder process. Tracked so the forward can be torn down
/// explicitly or when its session stops, instead of leaking until process exit.
#[derive(Debug)]
struct ActiveForward {
    local_port: u16,
    guest_process_id: String,
    host_task: tokio::task::AbortHandle,
}

#[derive(Debug, Clone)]
pub struct SandboxManagerSdk {
    manager: SandboxManager,
    cancels: Arc<Mutex<HashMap<String, std::sync::mpsc::Sender<()>>>>,
    forwards: Arc<Mutex<HashMap<String, Vec<ActiveForward>>>>,
    next_process: Arc<AtomicU64>,
    next_forward: Arc<AtomicU64>,
    host_translate: bool,
}

impl SandboxManagerSdk {
    pub fn new(manager: SandboxManager) -> Self {
        Self {
            manager,
            cancels: Arc::new(Mutex::new(HashMap::new())),
            forwards: Arc::new(Mutex::new(HashMap::new())),
            next_process: Arc::new(AtomicU64::new(1)),
            next_forward: Arc::new(AtomicU64::new(1)),
            host_translate: false,
        }
    }

    /// Enable host-path translation: rewrite a command's `cwd`, argv tokens, and
    /// env values from declared guest mount paths to their backing host paths
    /// before dispatch. This is only correct when the command server executes on
    /// the host (the host-translated / fake-launcher test topology), NOT inside a
    /// real guest VM where guest paths are already valid. Defaults to off, so
    /// production guest execution is unaffected.
    pub fn with_host_path_translation(mut self, enabled: bool) -> Self {
        self.host_translate = enabled;
        self
    }

    pub fn manager(&self) -> &SandboxManager {
        &self.manager
    }

    /// Tear down every port forward this instance started for `id`: abort the
    /// host relay tasks (dropping their listeners) and cancel the guest-side
    /// forwarder processes. Best-effort; safe to call when none exist.
    pub async fn stop_forwards(&self, id: &SandboxId) -> Result<()> {
        let forwards = self.forwards.lock().await.remove(id.as_str());
        let Some(forwards) = forwards else {
            return Ok(());
        };
        for forward in forwards {
            forward.host_task.abort();
            let _ = self.cancel_process(id, &forward.guest_process_id).await;
        }
        Ok(())
    }

    /// Tear down a single port forward identified by its host endpoint.
    pub async fn stop_forward(&self, id: &SandboxId, forward: &PortForward) -> Result<()> {
        let mut guard = self.forwards.lock().await;
        let Some(list) = guard.get_mut(id.as_str()) else {
            return Ok(());
        };
        let mut removed = Vec::new();
        list.retain(|active| {
            if active.local_port == forward.port {
                removed.push(active.guest_process_id.clone());
                active.host_task.abort();
                false
            } else {
                true
            }
        });
        if list.is_empty() {
            guard.remove(id.as_str());
        }
        drop(guard);
        for process_id in removed {
            let _ = self.cancel_process(id, &process_id).await;
        }
        Ok(())
    }

    /// Start a background task that shrinks idle, resize-enabled sandboxes back
    /// to their configured memory floor, returning the unused RAM to the host.
    ///
    /// This is the host-side interim for automatic reclaim until guest-kernel
    /// free-page reporting lands (design doc #A vs #H). It only touches sandboxes
    /// created with resize headroom (`--memory-max-mib` > `--memory-mib`); others
    /// are left alone. A sandbox is "idle" when it has no active interactive
    /// session and no live exec processes. Opt-in: embedders running a long-lived
    /// [`SandboxManagerSdk`] call this once; the returned handle aborts the loop
    /// when dropped (or call `.abort()`).
    pub fn spawn_idle_reclaim(&self, config: IdleReclaimConfig) -> tokio::task::JoinHandle<()> {
        let manager = self.manager.clone();
        tokio::spawn(async move {
            let mut idle_since: HashMap<String, std::time::Instant> = HashMap::new();
            loop {
                sleep(config.poll_interval).await;
                let scan_manager = manager.clone();
                let candidates =
                    match tokio::task::spawn_blocking(move || reclaim_scan(&scan_manager)).await {
                        Ok(Ok(candidates)) => candidates,
                        _ => continue,
                    };
                let mut seen = std::collections::HashSet::new();
                for candidate in candidates {
                    let key = candidate.id.as_str().to_string();
                    seen.insert(key.clone());
                    if candidate.busy {
                        idle_since.remove(&key);
                        continue;
                    }
                    let first_idle = *idle_since
                        .entry(key.clone())
                        .or_insert_with(std::time::Instant::now);
                    if first_idle.elapsed() >= config.idle_after {
                        let resize_manager = manager.clone();
                        let id = candidate.id.clone();
                        let floor = candidate.floor_mib;
                        // resize_memory to the floor is idempotent, so leaving the
                        // timer in place simply re-asserts the floor each cycle.
                        let _ = tokio::task::spawn_blocking(move || {
                            resize_manager.resize_memory(&id, floor)
                        })
                        .await;
                    }
                }
                idle_since.retain(|key, _| seen.contains(key));
            }
        })
    }
}

/// Tuning for [`SandboxManagerSdk::spawn_idle_reclaim`].
#[derive(Debug, Clone, Copy)]
pub struct IdleReclaimConfig {
    /// How often to scan running sandboxes.
    pub poll_interval: Duration,
    /// A resize-enabled sandbox idle for at least this long is shrunk to its
    /// memory floor.
    pub idle_after: Duration,
}

impl Default for IdleReclaimConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(15),
            idle_after: Duration::from_secs(60),
        }
    }
}

/// A running, resize-enabled sandbox the reclaim loop may act on.
struct ReclaimCandidate {
    id: SandboxId,
    busy: bool,
    floor_mib: u32,
}

/// Enumerate running, resize-enabled sandboxes and whether each is currently
/// busy (active session or live exec process). Blocking; run via spawn_blocking.
fn reclaim_scan(manager: &SandboxManager) -> Result<Vec<ReclaimCandidate>> {
    let mut out = Vec::new();
    for summary in manager.list()? {
        if !summary.running {
            continue;
        }
        let limits = match manager.memory_limits(&summary.id) {
            Ok(limits) => limits,
            Err(_) => continue,
        };
        if !limits.resize_enabled() {
            continue;
        }
        // Resize needs the per-VM control socket; skip if it is not up yet.
        if !manager.control_socket_path(&summary.id).exists() {
            continue;
        }
        let busy = active_session_pid(&summary.state_dir)
            .ok()
            .flatten()
            .is_some()
            || has_live_processes(&summary.state_dir);
        out.push(ReclaimCandidate {
            id: summary.id,
            busy,
            floor_mib: limits.min_mib,
        });
    }
    Ok(out)
}

/// A pool of pre-booted sandboxes built from a template config, handed out on
/// demand so a new agent session skips the ~1.3 s cold boot (design doc #F).
///
/// Pooled sandboxes share the template's mounts/credentials. Per-session
/// workspace attach after boot awaits guest-side bind or virtio-fs hot-plug, so
/// today the pool fits fleets of interchangeable sandboxes — e.g. a shared
/// workspace, or none — rather than arbitrary per-session host directories.
/// Admission control (`ResourceLedger`) still applies to each booted VM, so the
/// pool never oversubscribes the host budget.
#[derive(Clone)]
pub struct WarmPool {
    sdk: SandboxManagerSdk,
    template: SandboxConfig,
    size: usize,
    ready: Arc<Mutex<VecDeque<SandboxId>>>,
    counter: Arc<AtomicU64>,
}

impl WarmPool {
    pub fn new(sdk: SandboxManagerSdk, template: SandboxConfig, size: usize) -> Self {
        Self {
            sdk,
            template,
            size,
            ready: Arc::new(Mutex::new(VecDeque::new())),
            counter: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Number of pre-booted sandboxes currently ready to hand out.
    pub async fn ready_count(&self) -> usize {
        self.ready.lock().await.len()
    }

    /// Bring the pool up to its target size, booting sandboxes as needed.
    pub async fn prewarm(&self) -> Result<()> {
        self.refill().await
    }

    /// Take a ready sandbox, or boot one on demand when the pool is empty, then
    /// kick off a background refill so the pool repopulates toward its target.
    pub async fn acquire(&self) -> Result<SandboxId> {
        let pooled = self.ready.lock().await.pop_front();
        let id = match pooled {
            Some(id) => id,
            None => self.boot_one().await?,
        };
        let me = self.clone();
        tokio::spawn(async move {
            let _ = me.refill().await;
        });
        Ok(id)
    }

    async fn refill(&self) -> Result<()> {
        while self.ready.lock().await.len() < self.size {
            let id = self.boot_one().await?;
            self.ready.lock().await.push_back(id);
        }
        Ok(())
    }

    async fn boot_one(&self) -> Result<SandboxId> {
        let mut config = self.template.clone();
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        // Process-unique name so concurrent pools and runs don't collide.
        config.name = Some(format!("pool-{}-{}", std::process::id(), n));
        let sandbox = self.sdk.create_session(config).await?;
        self.sdk.start_session(&sandbox.id).await?;
        Ok(sandbox.id)
    }
}

/// Whether a sandbox has any live exec process tracked under `processes/`.
fn has_live_processes(state_dir: &std::path::Path) -> bool {
    let dir = state_dir.join("processes");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(contents) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Some(pid) = json_number(&contents, "pid") {
            if pid_is_running(pid) {
                return true;
            }
        }
    }
    false
}

#[async_trait]
impl SandboxRuntimeSdk for SandboxManagerSdk {
    async fn create_session(&self, config: SandboxConfig) -> Result<Sandbox> {
        let manager = self.manager.clone();
        tokio::task::spawn_blocking(move || manager.create(config))
            .await
            .map_err(|err| Error::Internal(format!("join create session task: {err}")))?
    }

    async fn reopen_session(&self, id: &SandboxId) -> Result<Sandbox> {
        let manager = self.manager.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || manager.reopen(&id))
            .await
            .map_err(|err| Error::Internal(format!("join reopen session task: {err}")))?
    }

    async fn start_session(&self, id: &SandboxId) -> Result<()> {
        let sandbox = self.reopen_session(id).await?;
        let existing = tokio::task::spawn_blocking({
            let state_dir = sandbox.state_dir.clone();
            move || active_session_pid(&state_dir)
        })
        .await
        .map_err(|err| Error::Internal(format!("join inspect session task: {err}")))??;
        if existing.is_some() {
            return Ok(());
        }

        let manager = self.manager.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            manager.set_initial_command(&id, guest_command_server_script())?;
            let mut launch = manager.launch_piped(&id)?;
            let mut child = launch.take_child()?;
            write_session_metadata(&sandbox.state_dir, child.id())?;
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let stdout_log = sandbox.state_dir.join("session.stdout.log");
            let stderr_log = sandbox.state_dir.join("session.stderr.log");
            if let Some(stdout) = stdout {
                std::thread::spawn(move || pipe_blocking_to_file(stdout, stdout_log));
            }
            if let Some(stderr) = stderr {
                std::thread::spawn(move || pipe_blocking_to_file(stderr, stderr_log));
            }
            std::thread::spawn(move || {
                let _ = child.wait();
                let _ = std::fs::remove_file(session_metadata_path(&sandbox.state_dir));
                drop(launch);
            });
            Ok(())
        })
        .await
        .map_err(|err| Error::Internal(format!("join start session task: {err}")))?
    }

    async fn exec(&self, id: &SandboxId, command: GuestCommand) -> Result<GuestProcess> {
        let sandbox = self.reopen_session(id).await?;
        if active_session_pid_async(sandbox.state_dir.clone())
            .await?
            .is_some()
        {
            let control_dir = sandbox.state_dir.join("runtime-control");
            if !wait_for_session_ready(&control_dir, SESSION_READY_TIMEOUT).await? {
                return Err(Error::InvalidArgument(format!(
                    "sandbox session {} is running but command server is not ready",
                    id
                )));
            }
            return self.exec_in_active_session(sandbox, command).await;
        }

        let manager = self.manager.clone();
        let id = id.clone();
        let initial_command = guest_command_to_shell(&command)?;
        let process_id = format!(
            "{}:process-{}",
            id.as_str(),
            self.next_process.fetch_add(1, Ordering::Relaxed)
        );
        let mut launch = tokio::task::spawn_blocking(move || {
            manager.set_initial_command(&id, &initial_command)?;
            manager.launch_piped(&id)
        })
        .await
        .map_err(|err| Error::Internal(format!("join launch task: {err}")))??;
        let mut child = launch.take_child()?;
        let metadata_path = process_metadata_path(&sandbox.state_dir, &process_id);
        if let Err(err) = write_process_metadata(&sandbox.state_dir, &process_id, child.id()) {
            let _ = child.kill();
            return Err(err);
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::InvalidArgument("sandbox stdout was not piped".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::InvalidArgument("sandbox stderr was not piped".to_string()))?;
        let (stdout_tx, stdout_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let (stderr_tx, stderr_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let (exit_tx, exit_rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
        self.cancels
            .lock()
            .await
            .insert(process_id.clone(), cancel_tx);

        std::thread::spawn(move || pipe_blocking_lines(stdout, stdout_tx));
        std::thread::spawn(move || pipe_blocking_lines(stderr, stderr_tx));

        let cancels = self.cancels.clone();
        let process_id_for_wait = process_id.clone();
        let metadata_path_for_wait = metadata_path.clone();
        tokio::task::spawn_blocking(move || {
            loop {
                if cancel_rx.try_recv().is_ok() {
                    let _ = child.kill();
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let result = GuestExitStatus {
                            code: status.code(),
                            success: status.success(),
                        };
                        let _ = exit_tx.send(Ok(result));
                        break;
                    }
                    Ok(None) => std::thread::sleep(OUTPUT_POLL_INTERVAL),
                    Err(err) => {
                        let _ = exit_tx.send(Err(Error::io(err)));
                        break;
                    }
                }
            }
            cancels.blocking_lock().remove(&process_id_for_wait);
            let _ = std::fs::remove_file(&metadata_path_for_wait);
            drop(launch);
        });

        Ok(GuestProcess {
            id: process_id,
            stdout: stdout_rx,
            stderr: stderr_rx,
            exit: exit_rx,
        })
    }

    async fn forward_port(&self, id: &SandboxId, guest_port: u16) -> Result<PortForward> {
        if guest_port == 0 {
            return Err(Error::InvalidArgument(
                "guest port must be non-zero".to_string(),
            ));
        }
        let sandbox = self.reopen_session(id).await?;
        if active_session_pid_async(sandbox.state_dir.clone())
            .await?
            .is_none()
        {
            return Err(Error::InvalidArgument(format!(
                "sandbox session {} is not running",
                id
            )));
        }
        let control_dir = sandbox.state_dir.join("runtime-control");
        if !wait_for_session_ready(&control_dir, SESSION_READY_TIMEOUT).await? {
            return Err(Error::InvalidArgument(format!(
                "sandbox session {} is running but command server is not ready",
                id
            )));
        }

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(Error::io)?;
        let local_port = listener.local_addr().map_err(Error::io)?.port();
        let forward_id = format!(
            "{}-forward-{}",
            id.as_str(),
            self.next_forward.fetch_add(1, Ordering::Relaxed)
        );
        let forward_dir = control_dir.join("forwards").join(&forward_id);
        tokio::fs::create_dir_all(forward_dir.join("connections"))
            .await
            .map_err(|err| Error::io_path(&forward_dir, err))?;
        make_world_writable_dir_async(forward_dir.clone()).await?;

        let mut command = GuestCommand::new(["python3", "-u", "-c", guest_port_forwarder_py()]);
        command
            .env
            .push(("AGENT_SANDBOX_FORWARD_ID".to_string(), forward_id.clone()));
        command.env.push((
            "AGENT_SANDBOX_FORWARD_GUEST_PORT".to_string(),
            guest_port.to_string(),
        ));
        let mut process = self
            .exec_in_active_session(sandbox.clone(), command)
            .await?;
        let guest_process_id = process.id.clone();
        tokio::spawn(async move { while process.stdout.recv().await.is_some() {} });
        tokio::spawn(async move { while process.stderr.recv().await.is_some() {} });

        let host_task = tokio::spawn(run_host_port_forwarder(listener, forward_dir)).abort_handle();
        self.forwards
            .lock()
            .await
            .entry(id.as_str().to_string())
            .or_default()
            .push(ActiveForward {
                local_port,
                guest_process_id,
                host_task,
            });
        Ok(PortForward {
            host: "127.0.0.1".to_string(),
            port: local_port,
        })
    }

    async fn inspect_mounts(&self, id: &SandboxId) -> Result<Vec<WorkspaceMountInfo>> {
        let manager = self.manager.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || manager.inspect_mounts(&id))
            .await
            .map_err(|err| Error::Internal(format!("join inspect mounts task: {err}")))?
    }

    async fn cancel_process(&self, id: &SandboxId, process_id: &str) -> Result<()> {
        if let Some(sender) = self.cancels.lock().await.remove(process_id) {
            let _ = sender.send(());
            return Ok(());
        }
        let sandbox = self.reopen_session(id).await?;
        let process_id = process_id.to_string();
        tokio::task::spawn_blocking(move || {
            cancel_persisted_process(&sandbox.state_dir, &process_id)
        })
        .await
        .map_err(|err| Error::Internal(format!("join cancel process task: {err}")))?
    }

    async fn stop_session(&self, id: &SandboxId) -> Result<()> {
        // Abort host relay tasks and cancel guest forwarders before tearing down
        // the session so no forward outlives the VM it bridged into.
        self.stop_forwards(id).await?;
        let sandbox = self.reopen_session(id).await?;
        tokio::task::spawn_blocking(move || {
            cancel_persisted_session(&sandbox.state_dir)?;
            cancel_all_persisted_processes(&sandbox.state_dir)
        })
        .await
        .map_err(|err| Error::Internal(format!("join stop session task: {err}")))?
    }

    async fn delete_session(&self, id: &SandboxId) -> Result<()> {
        self.stop_session(id).await?;
        let manager = self.manager.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || manager.delete(&id))
            .await
            .map_err(|err| Error::Internal(format!("join delete session task: {err}")))?
    }
}

impl SandboxManagerSdk {
    async fn exec_in_active_session(
        &self,
        sandbox: Sandbox,
        command: GuestCommand,
    ) -> Result<GuestProcess> {
        // Host-path translation, when enabled, is done here in Rust against the
        // declared mount map instead of by a blind `sed` over the command text
        // in the guest shell. It only rewrites whole guest-path tokens in cwd /
        // argv / env values, so it cannot corrupt unrelated argument text.
        let command = if self.host_translate {
            let mounts = self.inspect_mounts(&sandbox.id).await?;
            let mut command = command;
            translate_guest_paths_to_host(&mut command, &mounts);
            command
        } else {
            command
        };
        let initial_command = guest_command_to_shell(&command)?;
        let process_id = format!(
            "{}:process-{}",
            sandbox.id.as_str(),
            self.next_process.fetch_add(1, Ordering::Relaxed)
        );
        let control_dir = sandbox.state_dir.join("runtime-control");
        let request_dir = control_dir.join("requests");
        let response_dir = control_dir.join("responses");
        tokio::fs::create_dir_all(&request_dir)
            .await
            .map_err(|err| Error::io_path(&request_dir, err))?;
        tokio::fs::create_dir_all(&response_dir)
            .await
            .map_err(|err| Error::io_path(&response_dir, err))?;

        let filename = process_id_filename(&process_id);
        let request_tmp = request_dir.join(format!("{filename}.sh.tmp"));
        let request_path = request_dir.join(format!("{filename}.sh"));
        let stdout_path = response_dir.join(format!("{filename}.stdout"));
        let stderr_path = response_dir.join(format!("{filename}.stderr"));
        let status_path = response_dir.join(format!("{filename}.status"));
        let cancel_path = response_dir.join(format!("{filename}.cancel"));
        let script = format!("#!/bin/sh\n{initial_command}\n");
        tokio::fs::write(&request_tmp, script)
            .await
            .map_err(|err| Error::io_path(&request_tmp, err))?;
        tokio::fs::rename(&request_tmp, &request_path)
            .await
            .map_err(|err| Error::io_path(&request_path, err))?;
        write_session_process_metadata_async(&sandbox.state_dir, &process_id, &cancel_path).await?;

        let (stdout_tx, stdout_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let (stderr_tx, stderr_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let (exit_tx, exit_rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
        self.cancels
            .lock()
            .await
            .insert(process_id.clone(), cancel_tx);

        tokio::spawn(tail_response_lines(
            stdout_path.clone(),
            stdout_tx,
            status_path.clone(),
        ));
        tokio::spawn(tail_response_lines(
            stderr_path.clone(),
            stderr_tx,
            status_path.clone(),
        ));

        let cancels = self.cancels.clone();
        let process_id_for_wait = process_id.clone();
        let metadata_path = process_metadata_path(&sandbox.state_dir, &process_id);
        tokio::spawn(async move {
            loop {
                if cancel_rx.try_recv().is_ok() {
                    let _ = tokio::fs::write(&cancel_path, b"cancel\n").await;
                }
                match read_status_if_exists(&status_path).await {
                    Ok(Some(code)) => {
                        let _ = exit_tx.send(Ok(GuestExitStatus {
                            code: Some(code),
                            success: code == 0,
                        }));
                        break;
                    }
                    Ok(None) => sleep(OUTPUT_POLL_INTERVAL).await,
                    Err(err) => {
                        let _ = exit_tx.send(Err(err));
                        break;
                    }
                }
            }
            cancels.lock().await.remove(&process_id_for_wait);
            let _ = tokio::fs::remove_file(metadata_path).await;
        });

        Ok(GuestProcess {
            id: process_id,
            stdout: stdout_rx,
            stderr: stderr_rx,
            exit: exit_rx,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LocalProcessSandboxRuntime {
    state_root: PathBuf,
    sessions: Arc<Mutex<HashMap<String, LocalSession>>>,
    cancels: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    next_session: Arc<AtomicU64>,
    next_process: Arc<AtomicU64>,
}

#[derive(Debug, Clone)]
struct LocalSession {
    sandbox: Sandbox,
    mounts: Vec<WorkspaceMountInfo>,
}

impl LocalProcessSandboxRuntime {
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            cancels: Arc::new(Mutex::new(HashMap::new())),
            next_session: Arc::new(AtomicU64::new(1)),
            next_process: Arc::new(AtomicU64::new(1)),
        }
    }

    async fn session(&self, id: &SandboxId) -> Result<LocalSession> {
        self.sessions
            .lock()
            .await
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))
    }
}

#[async_trait]
impl SandboxRuntimeSdk for LocalProcessSandboxRuntime {
    async fn create_session(&self, config: SandboxConfig) -> Result<Sandbox> {
        let raw_id = config.name.unwrap_or_else(|| {
            format!(
                "local-{}",
                self.next_session.fetch_add(1, Ordering::Relaxed)
            )
        });
        let id = SandboxId::new(raw_id)?;
        let state_dir = self.state_root.join(id.as_str());
        tokio::fs::create_dir_all(&state_dir)
            .await
            .map_err(|err| Error::io_path(&state_dir, err))?;
        let sandbox = Sandbox {
            id: id.clone(),
            state_dir,
        };
        let mounts = config
            .workspace_mounts
            .into_iter()
            .map(|mount| WorkspaceMountInfo {
                tag: mount.tag,
                host_path: mount.host_path,
                guest_path: mount.guest_path,
                read_only: mount.read_only,
            })
            .collect();
        self.sessions.lock().await.insert(
            id.as_str().to_string(),
            LocalSession {
                sandbox: sandbox.clone(),
                mounts,
            },
        );
        Ok(sandbox)
    }

    async fn reopen_session(&self, id: &SandboxId) -> Result<Sandbox> {
        Ok(self.session(id).await?.sandbox)
    }

    async fn start_session(&self, id: &SandboxId) -> Result<()> {
        let _ = self.session(id).await?;
        Ok(())
    }

    async fn exec(&self, id: &SandboxId, command: GuestCommand) -> Result<GuestProcess> {
        let _ = self.session(id).await?;
        let (program, args) = command.argv.split_first().ok_or_else(|| {
            Error::InvalidArgument("guest command argv must not be empty".to_string())
        })?;
        let process_id = format!(
            "{}:process-{}",
            id.as_str(),
            self.next_process.fetch_add(1, Ordering::Relaxed)
        );
        let mut child = Command::new(program);
        child.args(args);
        for (key, value) in command.env {
            child.env(key, value);
        }
        if let Some(cwd) = command.cwd {
            child.current_dir(cwd);
        }
        child
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Put the child in its own process group so cancellation can terminate
        // the whole subprocess tree, matching the guest command-server path.
        #[cfg(unix)]
        child.process_group(0);
        let mut child = child.spawn().map_err(Error::io)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::InvalidArgument("child stdout was not piped".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::InvalidArgument("child stderr was not piped".to_string()))?;
        let (stdout_tx, stdout_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let (stderr_tx, stderr_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let (exit_tx, exit_rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.cancels
            .lock()
            .await
            .insert(process_id.clone(), cancel_tx);

        tokio::spawn(pipe_lines(stdout, stdout_tx));
        tokio::spawn(pipe_lines(stderr, stderr_tx));

        let cancels = self.cancels.clone();
        let process_id_for_wait = process_id.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                _ = cancel_rx => {
                    // Signal the whole process group (the child is its own group
                    // leader), then fall back to killing the direct child.
                    #[cfg(unix)]
                    if let Some(pid) = child.id() {
                        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGTERM) };
                    }
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            cancels.lock().await.remove(&process_id_for_wait);
            let result = status
                .map(|status| GuestExitStatus {
                    code: status.code(),
                    success: status.success(),
                })
                .map_err(Error::io);
            let _ = exit_tx.send(result);
        });

        Ok(GuestProcess {
            id: process_id,
            stdout: stdout_rx,
            stderr: stderr_rx,
            exit: exit_rx,
        })
    }

    async fn forward_port(&self, id: &SandboxId, guest_port: u16) -> Result<PortForward> {
        let _ = self.session(id).await?;
        Ok(PortForward {
            host: "127.0.0.1".to_string(),
            port: guest_port,
        })
    }

    async fn inspect_mounts(&self, id: &SandboxId) -> Result<Vec<WorkspaceMountInfo>> {
        Ok(self.session(id).await?.mounts)
    }

    async fn cancel_process(&self, id: &SandboxId, process_id: &str) -> Result<()> {
        let _ = self.session(id).await?;
        let sender = self
            .cancels
            .lock()
            .await
            .remove(process_id)
            .ok_or_else(|| Error::NotFound(format!("process {process_id}")))?;
        let _ = sender.send(());
        Ok(())
    }

    async fn stop_session(&self, id: &SandboxId) -> Result<()> {
        let _ = self.session(id).await?;
        Ok(())
    }

    async fn delete_session(&self, id: &SandboxId) -> Result<()> {
        let session = self
            .sessions
            .lock()
            .await
            .remove(id.as_str())
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
        tokio::fs::remove_dir_all(&session.sandbox.state_dir)
            .await
            .map_err(|err| Error::io_path(&session.sandbox.state_dir, err))
    }
}

async fn pipe_lines<R>(reader: R, tx: mpsc::Sender<String>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if tx.send(line).await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

fn pipe_blocking_lines<R>(reader: R, tx: mpsc::Sender<String>)
where
    R: std::io::Read,
{
    let reader = std::io::BufReader::new(reader);
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if tx.blocking_send(line).is_err() {
            break;
        }
    }
}

fn pipe_blocking_to_file<R>(mut reader: R, path: PathBuf)
where
    R: std::io::Read,
{
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = std::io::copy(&mut reader, &mut file);
    let _ = file.flush();
}

async fn active_session_pid_async(state_dir: PathBuf) -> Result<Option<u32>> {
    tokio::task::spawn_blocking(move || active_session_pid(&state_dir))
        .await
        .map_err(|err| Error::Internal(format!("join inspect session task: {err}")))?
}

async fn write_session_process_metadata_async(
    state_dir: &std::path::Path,
    process_id: &str,
    cancel_path: &std::path::Path,
) -> Result<()> {
    let state_dir = state_dir.to_path_buf();
    let process_id = process_id.to_string();
    let cancel_path = cancel_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        write_session_process_metadata(&state_dir, &process_id, &cancel_path)
    })
    .await
    .map_err(|err| Error::Internal(format!("join write process metadata task: {err}")))?
}

async fn make_world_writable_dir_async(path: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || make_world_writable_dir(&path))
        .await
        .map_err(|err| Error::Internal(format!("join chmod task: {err}")))?
}

async fn make_world_writable_file_async(path: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || make_world_writable_file(&path))
        .await
        .map_err(|err| Error::Internal(format!("join chmod task: {err}")))?
}

#[cfg(unix)]
fn make_world_writable_dir(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path).map_err(|err| Error::io_path(path, err))?;
    let mut permissions = std::fs::metadata(path)
        .map_err(|err| Error::io_path(path, err))?
        .permissions();
    permissions.set_mode(0o777);
    std::fs::set_permissions(path, permissions).map_err(|err| Error::io_path(path, err))?;
    for entry in std::fs::read_dir(path).map_err(|err| Error::io_path(path, err))? {
        let entry = entry.map_err(Error::io)?;
        if entry.file_type().map_err(Error::io)?.is_dir() {
            make_world_writable_dir(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_world_writable_file(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)
        .map_err(|err| Error::io_path(path, err))?
        .permissions();
    permissions.set_mode(0o666);
    std::fs::set_permissions(path, permissions).map_err(|err| Error::io_path(path, err))
}

#[cfg(not(unix))]
fn make_world_writable_dir(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|err| Error::io_path(path, err))
}

#[cfg(not(unix))]
fn make_world_writable_file(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn write_session_process_metadata(
    state_dir: &std::path::Path,
    process_id: &str,
    cancel_path: &std::path::Path,
) -> Result<()> {
    let dir = state_dir.join("processes");
    std::fs::create_dir_all(&dir).map_err(|err| Error::io_path(&dir, err))?;
    let path = process_metadata_path(state_dir, process_id);
    let contents = format!(
        "{{\n  \"process_id\": \"{}\",\n  \"cancel_path\": \"{}\"\n}}\n",
        escape_json(process_id),
        escape_json(&cancel_path.display().to_string())
    );
    std::fs::write(&path, contents).map_err(|err| Error::io_path(path, err))
}

async fn run_host_port_forwarder(listener: TcpListener, forward_dir: PathBuf) {
    let next_connection = AtomicU64::new(1);
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let conn_id = format!(
            "conn-{}-{}",
            std::process::id(),
            next_connection.fetch_add(1, Ordering::Relaxed)
        );
        let conn_dir = forward_dir.join("connections").join(conn_id);
        if make_world_writable_dir_async(conn_dir.clone())
            .await
            .is_err()
        {
            continue;
        }
        tokio::spawn(handle_forward_connection(stream, conn_dir));
    }
}

async fn handle_forward_connection(stream: TcpStream, conn_dir: PathBuf) {
    let host_to_guest = conn_dir.join("host_to_guest.bin");
    let guest_to_host = conn_dir.join("guest_to_host.bin");
    let host_eof = conn_dir.join("host_eof");
    let guest_eof = conn_dir.join("guest_eof");
    let _ = tokio::fs::File::create(&host_to_guest).await;
    let _ = tokio::fs::File::create(&guest_to_host).await;
    let _ = make_world_writable_file_async(host_to_guest.clone()).await;
    let _ = make_world_writable_file_async(guest_to_host.clone()).await;
    let (mut reader, mut writer) = stream.into_split();

    let upload_path = host_to_guest.clone();
    let upload_eof = host_eof.clone();
    let upload = tokio::spawn(async move {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => {
                    let Ok(mut file) = tokio::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&upload_path)
                        .await
                    else {
                        break;
                    };
                    if file.write_all(&buffer[..n]).await.is_err() {
                        break;
                    }
                    let _ = file.flush().await;
                }
                Err(_) => break,
            }
        }
        let _ = tokio::fs::write(upload_eof, b"").await;
    });

    let download = tokio::spawn(async move {
        let mut offset = 0u64;
        loop {
            match read_new_bytes_async(&guest_to_host, &mut offset).await {
                Ok(bytes) if !bytes.is_empty() => {
                    if writer.write_all(&bytes).await.is_err() {
                        break;
                    }
                    let _ = writer.flush().await;
                }
                Ok(_) => {}
                Err(_) => break,
            }
            if tokio::fs::try_exists(&guest_eof).await.unwrap_or(false) {
                if let Ok(bytes) = read_new_bytes_async(&guest_to_host, &mut offset).await {
                    if !bytes.is_empty() {
                        let _ = writer.write_all(&bytes).await;
                        let _ = writer.flush().await;
                    }
                }
                break;
            }
            sleep(FORWARD_POLL_INTERVAL).await;
        }
        let _ = writer.shutdown().await;
    });

    let _ = tokio::join!(upload, download);
}

async fn read_new_bytes_async(
    path: &std::path::Path,
    offset: &mut u64,
) -> std::io::Result<Vec<u8>> {
    let mut file = tokio::fs::OpenOptions::new().read(true).open(path).await?;
    file.seek(SeekFrom::Start(*offset)).await?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    *offset += bytes.len() as u64;
    Ok(bytes)
}

async fn tail_response_lines(path: PathBuf, tx: mpsc::Sender<String>, status_path: PathBuf) {
    // Read only the bytes appended since the last poll (seek from `offset`)
    // rather than re-reading the whole file each tick, and decode lossily so
    // non-UTF-8 output does not break the stream. `pending` holds bytes after
    // the last newline that have not yet formed a complete line.
    let mut offset = 0u64;
    let mut pending: Vec<u8> = Vec::new();
    loop {
        if let Ok(chunk) = read_new_bytes_async(&path, &mut offset).await {
            pending.extend_from_slice(&chunk);
            while let Some(index) = pending.iter().position(|&b| b == b'\n') {
                let mut line = pending.drain(..=index).collect::<Vec<u8>>();
                line.pop(); // drop '\n'
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if tx
                    .send(String::from_utf8_lossy(&line).into_owned())
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        if status_path.exists() {
            // The command finished; flush any trailing bytes that the producer
            // wrote without a final newline, then stop.
            if let Ok(chunk) = read_new_bytes_async(&path, &mut offset).await {
                pending.extend_from_slice(&chunk);
            }
            while let Some(index) = pending.iter().position(|&b| b == b'\n') {
                let mut line = pending.drain(..=index).collect::<Vec<u8>>();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if tx
                    .send(String::from_utf8_lossy(&line).into_owned())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if !pending.is_empty() {
                let _ = tx
                    .send(String::from_utf8_lossy(&pending).into_owned())
                    .await;
            }
            break;
        }
        sleep(OUTPUT_POLL_INTERVAL).await;
    }
}

async fn read_status_if_exists(path: &std::path::Path) -> Result<Option<i32>> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => {
            let trimmed = contents.trim();
            let code = trimmed.parse::<i32>().map_err(|err| {
                Error::InvalidArgument(format!(
                    "invalid session command status {}: {err}",
                    path.display()
                ))
            })?;
            Ok(Some(code))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::io_path(path, err)),
    }
}

async fn wait_for_session_ready(control_dir: &std::path::Path, timeout: Duration) -> Result<bool> {
    let ready_path = control_dir.join("session.ready");
    let start = std::time::Instant::now();
    loop {
        if ready_path.exists() {
            return Ok(true);
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        sleep(OUTPUT_POLL_INTERVAL).await;
    }
}

fn guest_command_server_script() -> &'static str {
    r#"control=${AGENT_SANDBOX_CONTROL_ROOT:-/run/agent-sandbox/control}
mkdir -p "$control/requests" "$control/responses"
printf 'sandbox-session-ready\n'
printf 'ready\n' > "$control/session.ready"
while :; do
  for request in "$control"/requests/*.sh; do
    [ -f "$request" ] || continue
    base="${request##*/}"
    process="${base%.sh}"
    claimed="$control/requests/$process.running"
    if mv "$request" "$claimed" 2>/dev/null; then
      stdout="$control/responses/$process.stdout"
      stderr="$control/responses/$process.stderr"
      status_tmp="$control/responses/$process.status.tmp"
      status="$control/responses/$process.status"
      cancel="$control/responses/$process.cancel"
      (
        # Path translation (guest mount paths -> host paths for host-translated
        # execution) is handled structurally on the host by the SDK, not by an
        # in-band rewrite of the command text here.
        runner="$claimed"
        # Run the job in its own process group (via setsid when available) so a
        # cancel can terminate the whole subprocess tree, not just the direct
        # child. Without setsid (e.g. minimal guests, or the host-side test
        # harness on macOS), fall back to signalling the single child.
        use_pgroup=0
        if command -v setsid >/dev/null 2>&1; then
          setsid sh "$runner" > "$stdout" 2> "$stderr" &
          use_pgroup=1
        else
          sh "$runner" > "$stdout" 2> "$stderr" &
        fi
        child=$!
        while kill -0 "$child" 2>/dev/null; do
          if [ -f "$cancel" ]; then
            if [ "$use_pgroup" = 1 ]; then
              kill -TERM -"$child" 2>/dev/null || true
            else
              kill -TERM "$child" 2>/dev/null || true
            fi
          fi
          sleep 0.1
        done
        wait "$child"
        code=$?
        printf '%s\n' "$code" > "$status_tmp"
        mv "$status_tmp" "$status"
        rm -f "$claimed" "$cancel"
      ) &
    fi
  done
  sleep 0.1
done"#
}

fn guest_port_forwarder_py() -> &'static str {
    r#"
import os
import socket
import threading
import time
from pathlib import Path

forward_id = os.environ["AGENT_SANDBOX_FORWARD_ID"]
guest_port = int(os.environ["AGENT_SANDBOX_FORWARD_GUEST_PORT"])
# Resolve the control root from the environment (set to the host control dir by
# host-translated launchers, defaulting to the in-guest mount otherwise) rather
# than hardcoding the guest path and relying on a text rewrite.
control_root = os.environ.get("AGENT_SANDBOX_CONTROL_ROOT", "/run/agent-sandbox/control")
base = Path(control_root) / "forwards" / forward_id
connections = base / "connections"
connections.mkdir(parents=True, exist_ok=True)
seen = set()

def read_new(path, offset):
    try:
        with open(path, "rb") as f:
            f.seek(offset)
            data = f.read()
            return data, offset + len(data)
    except FileNotFoundError:
        return b"", offset

def append(path, data):
    with open(path, "ab") as f:
        f.write(data)
        f.flush()

def connect_guest(deadline_seconds=10):
    deadline = time.time() + deadline_seconds
    last_error = None
    while time.time() < deadline:
        try:
            return socket.create_connection(("127.0.0.1", guest_port), timeout=1)
        except OSError as exc:
            last_error = exc
            time.sleep(0.05)
    raise last_error or TimeoutError("guest port was not reachable")

def handle(conn_dir):
    h2g = conn_dir / "host_to_guest.bin"
    g2h = conn_dir / "guest_to_host.bin"
    host_eof = conn_dir / "host_eof"
    guest_eof = conn_dir / "guest_eof"
    h2g.touch(exist_ok=True)
    g2h.touch(exist_ok=True)
    try:
        sock = connect_guest()
    except OSError:
        guest_eof.touch(exist_ok=True)
        return
    stop = threading.Event()

    def to_guest():
        offset = 0
        try:
            while not stop.is_set():
                data, offset = read_new(h2g, offset)
                if data:
                    sock.sendall(data)
                if host_eof.exists():
                    try:
                        sock.shutdown(socket.SHUT_WR)
                    except OSError:
                        pass
                    return
                time.sleep(0.01)
        finally:
            stop.set()

    def to_host():
        try:
            while True:
                data = sock.recv(8192)
                if not data:
                    return
                append(g2h, data)
        finally:
            try:
                guest_eof.touch(exist_ok=True)
            finally:
                stop.set()

    t1 = threading.Thread(target=to_guest, daemon=True)
    t2 = threading.Thread(target=to_host, daemon=True)
    t1.start()
    t2.start()
    t1.join()
    t2.join()
    sock.close()

while True:
    for item in connections.iterdir():
        if item.is_dir() and item.name not in seen:
            seen.add(item.name)
            threading.Thread(target=handle, args=(item,), daemon=True).start()
    time.sleep(0.02)
"#
}

/// Rewrite guest mount paths to their backing host paths within a command's
/// structured fields (`cwd`, argv tokens, env values). A token is translated
/// only when it equals a mount's guest path or sits beneath it (`guest/...`),
/// so substrings inside unrelated arguments are left intact. Mounts are matched
/// most-specific-first so nested mounts (e.g. `/workspace` and
/// `/workspace/generation`) resolve to the deepest match.
fn translate_guest_paths_to_host(command: &mut GuestCommand, mounts: &[WorkspaceMountInfo]) {
    let mut map: Vec<(String, String)> = mounts
        .iter()
        .map(|mount| {
            (
                mount.guest_path.clone(),
                mount.host_path.display().to_string(),
            )
        })
        .collect();
    map.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    let translate = |value: &str| -> Option<String> {
        for (guest, host) in &map {
            if value == guest {
                return Some(host.clone());
            }
            if let Some(rest) = value.strip_prefix(guest).and_then(|r| r.strip_prefix('/')) {
                return Some(format!("{host}/{rest}"));
            }
        }
        None
    };

    if let Some(cwd) = command.cwd.as_deref().and_then(translate) {
        command.cwd = Some(cwd);
    }
    for arg in &mut command.argv {
        if let Some(translated) = translate(arg) {
            *arg = translated;
        }
    }
    for (_, value) in &mut command.env {
        if let Some(translated) = translate(value) {
            *value = translated;
        }
    }
}

fn guest_command_to_shell(command: &GuestCommand) -> Result<String> {
    let (program, args) = command.argv.split_first().ok_or_else(|| {
        Error::InvalidArgument("guest command argv must not be empty".to_string())
    })?;
    let mut script = String::new();
    for (key, value) in &command.env {
        validate_env_key(key)?;
        script.push_str("export ");
        script.push_str(key);
        script.push('=');
        script.push_str(&shell_quote(value));
        script.push_str("; ");
    }
    if let Some(cwd) = &command.cwd {
        script.push_str("cd ");
        script.push_str(&shell_quote(cwd));
        script.push_str("; ");
    }
    script.push_str(&shell_quote(program));
    for arg in args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script.push_str("; status=$?; exit $status");
    Ok(script)
}

fn validate_env_key(key: &str) -> Result<()> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return Err(Error::InvalidArgument(
            "guest command env key must not be empty".to_string(),
        ));
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(Error::InvalidArgument(format!(
            "invalid guest command env key {key}"
        )));
    }
    if chars.any(|ch| !(ch == '_' || ch.is_ascii_alphanumeric())) {
        return Err(Error::InvalidArgument(format!(
            "invalid guest command env key {key}"
        )));
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn write_session_metadata(state_dir: &std::path::Path, pid: u32) -> Result<()> {
    let path = session_metadata_path(state_dir);
    let contents = format!("{{\n  \"pid\": {}\n}}\n", pid);
    std::fs::write(&path, contents).map_err(|err| Error::io_path(path, err))
}

fn session_metadata_path(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("session.json")
}

fn active_session_pid(state_dir: &std::path::Path) -> Result<Option<u32>> {
    let path = session_metadata_path(state_dir);
    let metadata = match std::fs::read_to_string(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::io_path(&path, err)),
    };
    let pid = json_number(&metadata, "pid")
        .ok_or_else(|| Error::InvalidArgument("session metadata missing pid".to_string()))?;
    if pid_is_running(pid) {
        Ok(Some(pid))
    } else {
        let _ = std::fs::remove_file(path);
        Ok(None)
    }
}

fn cancel_persisted_session(state_dir: &std::path::Path) -> Result<()> {
    let path = session_metadata_path(state_dir);
    let metadata = match std::fs::read_to_string(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::io_path(&path, err)),
    };
    let pid = json_number(&metadata, "pid")
        .ok_or_else(|| Error::InvalidArgument("session metadata missing pid".to_string()))?;
    let _ = kill_pid(pid);
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn write_process_metadata(state_dir: &std::path::Path, process_id: &str, pid: u32) -> Result<()> {
    let dir = state_dir.join("processes");
    std::fs::create_dir_all(&dir).map_err(|err| Error::io_path(&dir, err))?;
    let path = process_metadata_path(state_dir, process_id);
    let contents = format!(
        "{{\n  \"process_id\": \"{}\",\n  \"pid\": {}\n}}\n",
        escape_json(process_id),
        pid
    );
    std::fs::write(&path, contents).map_err(|err| Error::io_path(path, err))
}

fn process_metadata_path(state_dir: &std::path::Path, process_id: &str) -> PathBuf {
    state_dir
        .join("processes")
        .join(format!("{}.json", process_id_filename(process_id)))
}

fn process_id_filename(process_id: &str) -> String {
    process_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn cancel_persisted_process(state_dir: &std::path::Path, process_id: &str) -> Result<()> {
    let path = process_metadata_path(state_dir, process_id);
    let metadata = std::fs::read_to_string(&path).map_err(|err| Error::io_path(&path, err))?;
    if let Some(pid) = json_number(&metadata, "pid") {
        kill_pid(pid)?;
    } else if let Some(cancel_path) = json_string(&metadata, "cancel_path") {
        std::fs::write(&cancel_path, b"cancel\n")
            .map_err(|err| Error::io_path(cancel_path, err))?;
    } else {
        return Err(Error::InvalidArgument(format!(
            "process metadata missing pid or cancel_path: {process_id}"
        )));
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn cancel_all_persisted_processes(state_dir: &std::path::Path) -> Result<()> {
    let dir = state_dir.join("processes");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::io_path(&dir, err)),
    };
    for entry in entries {
        let entry = entry.map_err(Error::io)?;
        if !entry.file_type().map_err(Error::io)?.is_file() {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::read_to_string(&path).map_err(|err| Error::io_path(&path, err))?;
        if let Some(pid) = json_number(&metadata, "pid") {
            let _ = kill_pid(pid);
        } else if let Some(cancel_path) = json_string(&metadata, "cancel_path") {
            let _ = std::fs::write(cancel_path, b"cancel\n");
        }
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

#[cfg(unix)]
fn kill_pid(pid: u32) -> Result<()> {
    // Best-effort: signal the whole process group first (launch_piped spawns the
    // launcher as a group leader, so this reaps its child processes too), then
    // the pid itself in case it is not a group leader. The pid-targeted result
    // determines success, preserving the previous error behavior.
    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGTERM) };
    let status = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if status == 0 {
        Ok(())
    } else {
        Err(Error::io(std::io::Error::last_os_error()))
    }
}

#[cfg(unix)]
fn pid_is_running(pid: u32) -> bool {
    (unsafe { libc::kill(pid as libc::pid_t, 0) }) == 0
}

#[cfg(not(unix))]
fn kill_pid(_pid: u32) -> Result<()> {
    Err(Error::InvalidArgument(
        "persistent process cancellation is only implemented on unix".to_string(),
    ))
}

#[cfg(not(unix))]
fn pid_is_running(_pid: u32) -> bool {
    false
}

/// Return the value slice that follows `"key"` (after optional whitespace and
/// the `:`). Only matches occurrences where the quoted key is immediately
/// followed by a colon, so a `key` substring appearing inside some other
/// string value is not mistaken for the key. This is a deliberately tiny,
/// dependency-free reader for the small metadata files this crate writes.
fn json_value_after_key<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let mut search_from = 0;
    while let Some(rel) = json[search_from..].find(&needle) {
        let value_start = search_from + rel + needle.len();
        let after_key = json[value_start..].trim_start();
        if let Some(rest) = after_key.strip_prefix(':') {
            return Some(rest.trim_start());
        }
        search_from = value_start;
    }
    None
}

fn json_number(json: &str, key: &str) -> Option<u32> {
    let value = json_value_after_key(json, key)?;
    let digits: String = value.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn json_string(json: &str, key: &str) -> Option<PathBuf> {
    let value = json_value_after_key(json, key)?;
    let value = value.strip_prefix('"')?;
    let mut result = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            result.push(match ch {
                '"' => '"',
                '\\' => '\\',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(PathBuf::from(result));
        } else {
            result.push(ch);
        }
    }
    None
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentImageRef, WorkspaceMount};
    use crate::runtime::SandboxManager;

    #[tokio::test]
    async fn local_process_runtime_exec_streams_and_reports_exit() {
        let root = test_root("local_process_runtime_exec_streams_and_reports_exit");
        let runtime = LocalProcessSandboxRuntime::new(&root);
        let workspace = root.join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        let mut config = SandboxConfig::new(AgentImageRef::new(root.join("image")));
        config.name = Some("sdk-test".to_string());
        config.workspace_mounts.push(WorkspaceMount {
            tag: "generation".to_string(),
            host_path: workspace.clone(),
            guest_path: "/workspace/generation".to_string(),
            read_only: false,
        });

        let sandbox = runtime.create_session(config).await.unwrap();
        runtime.start_session(&sandbox.id).await.unwrap();
        let mounts = runtime.inspect_mounts(&sandbox.id).await.unwrap();
        assert_eq!(mounts[0].host_path, workspace);
        assert_eq!(mounts[0].guest_path, "/workspace/generation");

        let mut process = runtime
            .exec(
                &sandbox.id,
                GuestCommand::new(["/bin/sh", "-c", "printf 'out\\n'; printf 'err\\n' >&2"]),
            )
            .await
            .unwrap();

        assert_eq!(process.stdout.recv().await.as_deref(), Some("out"));
        assert_eq!(process.stderr.recv().await.as_deref(), Some("err"));
        let status = process.exit.await.unwrap().unwrap();
        assert!(status.success);
        assert_eq!(status.code, Some(0));
    }

    #[tokio::test]
    async fn local_process_runtime_cancels_process() {
        let root = test_root("local_process_runtime_cancels_process");
        let runtime = LocalProcessSandboxRuntime::new(&root);
        let mut config = SandboxConfig::new(AgentImageRef::new(root.join("image")));
        config.name = Some("cancel-test".to_string());
        let sandbox = runtime.create_session(config).await.unwrap();
        let process = runtime
            .exec(&sandbox.id, GuestCommand::new(["/bin/sh", "-c", "sleep 5"]))
            .await
            .unwrap();

        runtime
            .cancel_process(&sandbox.id, &process.id)
            .await
            .unwrap();
        let status = process.exit.await.unwrap().unwrap();
        assert!(!status.success);
    }

    #[tokio::test]
    async fn sandbox_manager_sdk_exec_launches_os_mode_with_initial_command() {
        let _env_guard = env_lock().lock().unwrap();
        let root = test_root("sandbox_manager_sdk_exec_launches_os_mode_with_initial_command");
        let image = root.join("image");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("kernel"), "kernel").unwrap();
        std::fs::write(image.join("root.raw"), "root").unwrap();
        std::fs::write(
            image.join("image.json"),
            format!(
                r#"{{
  "architecture": "{}",
  "kernel": "kernel",
  "root_disk": "root.raw"
}}"#,
                std::env::consts::ARCH
            ),
        )
        .unwrap();
        let launcher = root.join("fake-os-mode.sh");
        std::fs::write(
            &launcher,
            r#"#!/bin/sh
printf '%s\n' launcher-started
exit 0
"#,
        )
        .unwrap();
        make_executable(&launcher);
        std::env::set_var("TB_SANDBOX_OS_MODE", &launcher);
        let manager = SandboxManager {
            state_root: root.join("state"),
            cache_root: root.join("cache"),
        };
        let runtime = SandboxManagerSdk::new(manager);
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("manager-sdk-test".to_string());
        let sandbox = runtime.create_session(config).await.unwrap();

        let mut process = runtime
            .exec(
                &sandbox.id,
                GuestCommand::new(["/bin/echo", "hello from sdk"]),
            )
            .await
            .unwrap();

        assert_eq!(
            process.stdout.recv().await.as_deref(),
            Some("launcher-started")
        );
        let status = process.exit.await.unwrap().unwrap();
        assert!(status.success);
        let shell_env =
            std::fs::read_to_string(sandbox.state_dir.join("runtime-config/shell.env")).unwrap();
        assert!(shell_env.contains("INITIAL_COMMAND="));
        assert!(shell_env.contains("hello from sdk"));
        assert!(shell_env.contains("status=$?; exit $status"));
        std::env::remove_var("TB_SANDBOX_OS_MODE");
    }

    #[tokio::test]
    async fn sandbox_manager_sdk_cancel_uses_persisted_process_metadata() {
        let _env_guard = env_lock().lock().unwrap();
        let root = test_root("sandbox_manager_sdk_cancel_uses_persisted_process_metadata");
        let image = root.join("image");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("kernel"), "kernel").unwrap();
        std::fs::write(image.join("root.raw"), "root").unwrap();
        std::fs::write(
            image.join("image.json"),
            format!(
                r#"{{
  "architecture": "{}",
  "kernel": "kernel",
  "root_disk": "root.raw"
}}"#,
                std::env::consts::ARCH
            ),
        )
        .unwrap();
        let launcher = root.join("fake-os-mode-long-running.sh");
        std::fs::write(
            &launcher,
            r#"#!/bin/sh
printf '%s\n' launcher-started
while true; do sleep 1; done
"#,
        )
        .unwrap();
        make_executable(&launcher);
        std::env::set_var("TB_SANDBOX_OS_MODE", &launcher);
        let manager = SandboxManager {
            state_root: root.join("state"),
            cache_root: root.join("cache"),
        };
        let runtime = SandboxManagerSdk::new(manager.clone());
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("manager-sdk-cancel-test".to_string());
        let sandbox = runtime.create_session(config).await.unwrap();
        let mut process = runtime
            .exec(&sandbox.id, GuestCommand::new(["/bin/echo", "unused"]))
            .await
            .unwrap();
        assert_eq!(
            process.stdout.recv().await.as_deref(),
            Some("launcher-started")
        );
        let metadata_path = process_metadata_path(&sandbox.state_dir, &process.id);
        assert!(metadata_path.exists());

        let replacement_runtime = SandboxManagerSdk::new(manager);
        replacement_runtime
            .cancel_process(&sandbox.id, &process.id)
            .await
            .unwrap();

        let status = process.exit.await.unwrap().unwrap();
        assert!(!status.success);
        assert!(!metadata_path.exists());
        std::env::remove_var("TB_SANDBOX_OS_MODE");
    }

    #[tokio::test]
    async fn sandbox_manager_sdk_start_session_persists_and_stops_detached_launch() {
        let _env_guard = env_lock().lock().unwrap();
        let root =
            test_root("sandbox_manager_sdk_start_session_persists_and_stops_detached_launch");
        let image = root.join("image");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("kernel"), "kernel").unwrap();
        std::fs::write(image.join("root.raw"), "root").unwrap();
        std::fs::write(
            image.join("image.json"),
            format!(
                r#"{{
  "architecture": "{}",
  "kernel": "kernel",
  "root_disk": "root.raw"
}}"#,
                std::env::consts::ARCH
            ),
        )
        .unwrap();
        let launcher = root.join("fake-os-mode-session.sh");
        std::fs::write(
            &launcher,
            r#"#!/bin/sh
printf '%s\n' session-launch-started
while true; do sleep 1; done
"#,
        )
        .unwrap();
        make_executable(&launcher);
        std::env::set_var("TB_SANDBOX_OS_MODE", &launcher);
        let manager = SandboxManager {
            state_root: root.join("state"),
            cache_root: root.join("cache"),
        };
        let runtime = SandboxManagerSdk::new(manager.clone());
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("manager-sdk-session-test".to_string());
        let sandbox = runtime.create_session(config).await.unwrap();

        runtime.start_session(&sandbox.id).await.unwrap();
        let metadata_path = session_metadata_path(&sandbox.state_dir);
        assert!(metadata_path.exists());
        assert!(active_session_pid(&sandbox.state_dir).unwrap().is_some());

        let replacement_runtime = SandboxManagerSdk::new(manager);
        replacement_runtime.stop_session(&sandbox.id).await.unwrap();
        assert!(active_session_pid(&sandbox.state_dir).unwrap().is_none());
        std::env::remove_var("TB_SANDBOX_OS_MODE");
    }

    #[tokio::test]
    async fn sandbox_manager_sdk_exec_uses_active_session_command_queue() {
        let _env_guard = env_lock().lock().unwrap();
        let root = test_root("sandbox_manager_sdk_exec_uses_active_session_command_queue");
        let image = root.join("image");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("kernel"), "kernel").unwrap();
        std::fs::write(image.join("root.raw"), "root").unwrap();
        std::fs::write(
            image.join("image.json"),
            format!(
                r#"{{
  "architecture": "{}",
  "kernel": "kernel",
  "root_disk": "root.raw"
}}"#,
                std::env::consts::ARCH
            ),
        )
        .unwrap();
        let launcher = root.join("fake-os-mode-command-server.sh");
        std::fs::write(
            &launcher,
            r#"#!/bin/sh
config=''
control=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --virtiofs|--virtiofs-ro)
      shift
      case "$1" in
        agent-config=*) config="${1#agent-config=}" ;;
        agent-control=*) control="${1#agent-control=}" ;;
      esac
      ;;
  esac
  shift
done
test -n "$config" || exit 11
test -n "$control" || exit 12
. "$config/shell.env"
export AGENT_SANDBOX_CONTROL_ROOT="$control"
export AGENT_SANDBOX_HOST_CONTROL_PATH="$control"
eval "$INITIAL_COMMAND"
"#,
        )
        .unwrap();
        make_executable(&launcher);
        std::env::set_var("TB_SANDBOX_OS_MODE", &launcher);
        let manager = SandboxManager {
            state_root: root.join("state"),
            cache_root: root.join("cache"),
        };
        let runtime = SandboxManagerSdk::new(manager);
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("manager-sdk-session-exec-test".to_string());
        let sandbox = runtime.create_session(config).await.unwrap();

        runtime.start_session(&sandbox.id).await.unwrap();
        assert!(wait_for_session_ready(
            &sandbox.state_dir.join("runtime-control"),
            Duration::from_secs(2)
        )
        .await
        .unwrap());
        let mut process = runtime
            .exec(
                &sandbox.id,
                GuestCommand::new(["/bin/sh", "-c", "printf 'out\\n'; printf 'err\\n' >&2"]),
            )
            .await
            .unwrap();

        assert_eq!(process.stdout.recv().await.as_deref(), Some("out"));
        assert_eq!(process.stderr.recv().await.as_deref(), Some("err"));
        let status = process.exit.await.unwrap().unwrap();
        assert!(status.success);

        runtime.stop_session(&sandbox.id).await.unwrap();
        std::env::remove_var("TB_SANDBOX_OS_MODE");
    }

    #[tokio::test]
    async fn sandbox_manager_sdk_forward_port_bridges_active_session_tcp() {
        let _env_guard = env_lock().lock().unwrap();
        let root = test_root("sandbox_manager_sdk_forward_port_bridges_active_session_tcp");
        let image = root.join("image");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("kernel"), "kernel").unwrap();
        std::fs::write(image.join("root.raw"), "root").unwrap();
        std::fs::write(
            image.join("image.json"),
            format!(
                r#"{{
  "architecture": "{}",
  "kernel": "kernel",
  "root_disk": "root.raw"
}}"#,
                std::env::consts::ARCH
            ),
        )
        .unwrap();
        let launcher = root.join("fake-os-mode-port-forward.sh");
        std::fs::write(
            &launcher,
            r#"#!/bin/sh
config=''
control=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --virtiofs|--virtiofs-ro)
      shift
      case "$1" in
        agent-config=*) config="${1#agent-config=}" ;;
        agent-control=*) control="${1#agent-control=}" ;;
      esac
      ;;
  esac
  shift
done
test -n "$config" || exit 11
test -n "$control" || exit 12
. "$config/shell.env"
export AGENT_SANDBOX_CONTROL_ROOT="$control"
export AGENT_SANDBOX_HOST_CONTROL_PATH="$control"
eval "$INITIAL_COMMAND"
"#,
        )
        .unwrap();
        make_executable(&launcher);
        std::env::set_var("TB_SANDBOX_OS_MODE", &launcher);

        let guest_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let guest_port = guest_listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = guest_listener.accept().unwrap();
            let mut request = [0u8; 128];
            let _ = std::io::Read::read(&mut stream, &mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        });

        let manager = SandboxManager {
            state_root: root.join("state"),
            cache_root: root.join("cache"),
        };
        let runtime = SandboxManagerSdk::new(manager);
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("manager-sdk-forward-test".to_string());
        let sandbox = runtime.create_session(config).await.unwrap();
        runtime.start_session(&sandbox.id).await.unwrap();
        assert!(wait_for_session_ready(
            &sandbox.state_dir.join("runtime-control"),
            Duration::from_secs(2)
        )
        .await
        .unwrap());

        let forward = runtime.forward_port(&sandbox.id, guest_port).await.unwrap();
        let mut client = TcpStream::connect((forward.host.as_str(), forward.port))
            .await
            .unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).contains("\r\n\r\nok"));

        server.join().unwrap();

        // Tearing down the forward must abort the host relay task so the local
        // listener stops accepting, rather than leaking until process exit.
        runtime.stop_forward(&sandbox.id, &forward).await.unwrap();
        let mut closed = false;
        for _ in 0..50 {
            if TcpStream::connect((forward.host.as_str(), forward.port))
                .await
                .is_err()
            {
                closed = true;
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert!(
            closed,
            "host forward listener should stop accepting after stop_forward"
        );

        runtime.stop_session(&sandbox.id).await.unwrap();
        std::env::remove_var("TB_SANDBOX_OS_MODE");
    }

    #[tokio::test]
    async fn sandbox_manager_sdk_stop_session_cancels_persisted_processes() {
        let _env_guard = env_lock().lock().unwrap();
        let root = test_root("sandbox_manager_sdk_stop_session_cancels_persisted_processes");
        let image = root.join("image");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("kernel"), "kernel").unwrap();
        std::fs::write(image.join("root.raw"), "root").unwrap();
        std::fs::write(
            image.join("image.json"),
            format!(
                r#"{{
  "architecture": "{}",
  "kernel": "kernel",
  "root_disk": "root.raw"
}}"#,
                std::env::consts::ARCH
            ),
        )
        .unwrap();
        let launcher = root.join("fake-os-mode-stop-session.sh");
        std::fs::write(
            &launcher,
            r#"#!/bin/sh
printf '%s\n' launcher-started
while true; do sleep 1; done
"#,
        )
        .unwrap();
        make_executable(&launcher);
        std::env::set_var("TB_SANDBOX_OS_MODE", &launcher);
        let manager = SandboxManager {
            state_root: root.join("state"),
            cache_root: root.join("cache"),
        };
        let runtime = SandboxManagerSdk::new(manager.clone());
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("manager-sdk-stop-test".to_string());
        let sandbox = runtime.create_session(config).await.unwrap();
        let mut process = runtime
            .exec(&sandbox.id, GuestCommand::new(["/bin/echo", "unused"]))
            .await
            .unwrap();
        assert_eq!(
            process.stdout.recv().await.as_deref(),
            Some("launcher-started")
        );
        let metadata_path = process_metadata_path(&sandbox.state_dir, &process.id);
        assert!(metadata_path.exists());

        let replacement_runtime = SandboxManagerSdk::new(manager);
        replacement_runtime.stop_session(&sandbox.id).await.unwrap();

        let status = process.exit.await.unwrap().unwrap();
        assert!(!status.success);
        assert!(!metadata_path.exists());
        std::env::remove_var("TB_SANDBOX_OS_MODE");
    }

    #[test]
    fn translate_guest_paths_rewrites_only_whole_path_tokens() {
        let mounts = vec![
            WorkspaceMountInfo {
                tag: "generation".to_string(),
                host_path: PathBuf::from("/host/gen"),
                guest_path: "/workspace/generation".to_string(),
                read_only: false,
            },
            WorkspaceMountInfo {
                tag: "workspace".to_string(),
                host_path: PathBuf::from("/host/ws"),
                guest_path: "/workspace".to_string(),
                read_only: false,
            },
        ];
        let mut command = GuestCommand::new([
            "chidori",
            "serve",
            "/workspace/generation/main.py",
            "--label=/workspace/generation",
        ])
        .cwd("/workspace/generation")
        .env("ROOT", "/workspace");

        translate_guest_paths_to_host(&mut command, &mounts);

        // cwd and whole-token paths are translated; the deepest mount wins.
        assert_eq!(command.cwd.as_deref(), Some("/host/gen"));
        assert_eq!(
            command.argv,
            vec![
                "chidori".to_string(),
                "serve".to_string(),
                "/host/gen/main.py".to_string(),
                // Embedded inside a larger argument: left intact, unlike the old
                // blind sed which would have rewritten it.
                "--label=/workspace/generation".to_string(),
            ]
        );
        assert_eq!(
            command.env,
            vec![("ROOT".to_string(), "/host/ws".to_string())]
        );
    }

    #[tokio::test]
    async fn sandbox_manager_sdk_exec_translates_guest_paths_to_host_when_enabled() {
        let _env_guard = env_lock().lock().unwrap();
        let root =
            test_root("sandbox_manager_sdk_exec_translates_guest_paths_to_host_when_enabled");
        let image = root.join("image");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("kernel"), "kernel").unwrap();
        std::fs::write(image.join("root.raw"), "root").unwrap();
        std::fs::write(
            image.join("image.json"),
            format!(
                r#"{{
  "architecture": "{}",
  "kernel": "kernel",
  "root_disk": "root.raw"
}}"#,
                std::env::consts::ARCH
            ),
        )
        .unwrap();
        // Host directory backing the guest /workspace/generation mount.
        let host_workspace = root.join("generation");
        std::fs::create_dir_all(&host_workspace).unwrap();
        std::fs::write(host_workspace.join("marker.txt"), "translated-ok\n").unwrap();

        let launcher = root.join("fake-os-mode-translate.sh");
        std::fs::write(
            &launcher,
            r#"#!/bin/sh
config=''
control=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --virtiofs|--virtiofs-ro)
      shift
      case "$1" in
        agent-config=*) config="${1#agent-config=}" ;;
        agent-control=*) control="${1#agent-control=}" ;;
      esac
      ;;
  esac
  shift
done
test -n "$config" || exit 11
test -n "$control" || exit 12
. "$config/shell.env"
export AGENT_SANDBOX_CONTROL_ROOT="$control"
eval "$INITIAL_COMMAND"
"#,
        )
        .unwrap();
        make_executable(&launcher);
        std::env::set_var("TB_SANDBOX_OS_MODE", &launcher);

        let manager = SandboxManager {
            state_root: root.join("state"),
            cache_root: root.join("cache"),
        };
        let runtime = SandboxManagerSdk::new(manager).with_host_path_translation(true);
        let mut config = SandboxConfig::new(AgentImageRef::new(&image));
        config.name = Some("manager-sdk-translate-test".to_string());
        config.workspace_mounts.push(WorkspaceMount {
            tag: "generation".to_string(),
            host_path: host_workspace.clone(),
            guest_path: "/workspace/generation".to_string(),
            read_only: false,
        });
        let sandbox = runtime.create_session(config).await.unwrap();
        runtime.start_session(&sandbox.id).await.unwrap();
        assert!(wait_for_session_ready(
            &sandbox.state_dir.join("runtime-control"),
            Duration::from_secs(2)
        )
        .await
        .unwrap());

        // cwd references the guest mount path; host translation rewrites it to
        // the backing host directory so the host-side command server finds the
        // file. Without translation `cd /workspace/generation` would fail.
        let mut command = GuestCommand::new(["/bin/sh", "-c", "cat marker.txt"]);
        command.cwd = Some("/workspace/generation".to_string());
        let mut process = runtime.exec(&sandbox.id, command).await.unwrap();
        assert_eq!(
            process.stdout.recv().await.as_deref(),
            Some("translated-ok")
        );
        let status = process.exit.await.unwrap().unwrap();
        assert!(status.success);

        runtime.stop_session(&sandbox.id).await.unwrap();
        std::env::remove_var("TB_SANDBOX_OS_MODE");
    }

    #[tokio::test]
    async fn warm_pool_prewarms_and_hands_out_distinct_sandboxes() {
        let _env_guard = env_lock().lock().unwrap();
        let root = test_root("warm_pool_prewarms_and_hands_out_distinct_sandboxes");
        let image = root.join("image");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("kernel"), "kernel").unwrap();
        std::fs::write(image.join("root.raw"), "root").unwrap();
        std::fs::write(
            image.join("image.json"),
            format!(
                r#"{{
  "architecture": "{}",
  "kernel": "kernel",
  "root_disk": "root.raw"
}}"#,
                std::env::consts::ARCH
            ),
        )
        .unwrap();
        // Fake launcher stays alive briefly so started sessions look running.
        let launcher = root.join("fake-os-mode.sh");
        std::fs::write(&launcher, "#!/bin/sh\nsleep 2\n").unwrap();
        make_executable(&launcher);
        std::env::set_var("TB_SANDBOX_OS_MODE", &launcher);
        // Disable admission control so the test is independent of host RAM.
        std::env::set_var("TB_SANDBOX_MEM_BUDGET_MIB", "0");

        let manager = SandboxManager {
            state_root: root.join("state"),
            cache_root: root.join("cache"),
        };
        let sdk = SandboxManagerSdk::new(manager.clone());
        let template = SandboxConfig::new(AgentImageRef::new(&image));
        let pool = WarmPool::new(sdk, template, 2);

        pool.prewarm().await.unwrap();
        assert_eq!(pool.ready_count().await, 2);

        let id1 = pool.acquire().await.unwrap();
        let id2 = pool.acquire().await.unwrap();
        assert_ne!(id1.as_str(), id2.as_str());
        // Both handed-out sandboxes are real, listed sandboxes.
        let ids: Vec<String> = manager
            .list()
            .unwrap()
            .into_iter()
            .map(|s| s.id.as_str().to_string())
            .collect();
        assert!(ids.contains(&id1.as_str().to_string()));
        assert!(ids.contains(&id2.as_str().to_string()));

        std::env::remove_var("TB_SANDBOX_OS_MODE");
        std::env::remove_var("TB_SANDBOX_MEM_BUDGET_MIB");
    }

    fn test_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("tb-sandbox-sdk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn make_executable(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
    }

    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }
}
