//! Cooperative guest quiesce for snapshots of running sandboxes.
//!
//! ## Why
//!
//! Without quiesce, a snapshot taken while the guest is writing is
//! **crash-consistent** at best — equivalent to pulling the power on a real
//! machine. Filesystem journal replay on next boot brings the disk to a
//! consistent state, but in-flight application writes can be torn at
//! arbitrary boundaries. Cooperative quiesce flips that into a clean
//! point-in-time image: the wrapper asks the guest to flush, optionally
//! freeze the rootfs, take the snapshot, then unfreeze.
//!
//! ## Protocol
//!
//! The wrapper and the in-guest `agent-sandbox-quiesce` daemon exchange
//! text `key=value` files over the runtime-control virtio-fs mount
//! (`<sandbox_dir>/runtime-control/` on the host, `/run/agent-sandbox/control/`
//! in the guest). The mount is mode `0777`, so both sides can write.
//!
//! Files (all in the control dir):
//!
//! ```text
//!   quiesce.req  -- wrapper -> guest
//!     id=<unique>
//!     action=freeze|sync
//!     deadline_unix_millis=<when guest should auto-resume>
//!
//!   quiesce.ack  -- guest -> wrapper
//!     id=<echo>
//!     status=ok|error
//!     action=freeze|sync
//!     achieved_unix_millis=<when freeze/sync completed>
//!     message=<optional, single-line>
//!
//!   resume.req   -- wrapper -> guest
//!     id=<unique>
//!
//!   resume.ack   -- guest -> wrapper
//!     id=<echo>
//!     status=ok|error
//!     resumed_unix_millis=<when unfreeze completed>
//! ```
//!
//! Polling: the guest daemon polls at 100ms, the wrapper polls at 25ms.
//! Wrapper-to-guest latency is bounded by the guest's poll period plus the
//! filesystem flush; wrapper sees acks within ~150ms in the common case.
//!
//! ## Safety nets
//!
//! Two independent guarantees prevent a wedged guest filesystem if the
//! wrapper dies mid-snapshot:
//!
//! 1. **Per-request deadline.** Every `quiesce.req` carries
//!    `deadline_unix_millis`. The guest auto-resumes at the deadline even if
//!    no `resume.req` ever arrives — bounded worst case is a few minutes.
//! 2. **RAII drop.** [`QuiesceTicket`]'s `Drop` writes `resume.req`
//!    best-effort (no wait) so a Rust panic or early return triggers an
//!    immediate unfreeze rather than waiting for the deadline.
//!
//! Concurrent snapshots on the same sandbox serialize on a host-side flock
//! at `<control>/quiesce.lock` so two simultaneous wrappers cannot overwrite
//! each other's request files.

use crate::error::{Error, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const QUIESCE_REQ: &str = "quiesce.req";
const QUIESCE_ACK: &str = "quiesce.ack";
const RESUME_REQ: &str = "resume.req";
const RESUME_ACK: &str = "resume.ack";
const QUIESCE_LOCK: &str = "quiesce.lock";

/// What the guest should do when quiesced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuiesceAction {
    /// `sync` only — flush in-flight writes. Fast; equivalent to a normal
    /// `sync` before the disk clone. Use when the rootfs does not support
    /// `fsfreeze` (e.g. tmpfs).
    Sync,
    /// `sync` + `fsfreeze -f /` — pause all new writes for the duration.
    /// Strongest guarantee available; the rootfs must support `fsfreeze`
    /// (ext4 does; tmpfs does not).
    Freeze,
}

impl QuiesceAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Freeze => "freeze",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sync" => Some(Self::Sync),
            "freeze" => Some(Self::Freeze),
            _ => None,
        }
    }
}

/// Client that talks to the in-guest `agent-sandbox-quiesce` daemon over a
/// per-sandbox runtime-control directory.
pub struct QuiesceClient {
    control_dir: PathBuf,
}

impl QuiesceClient {
    pub fn new(control_dir: impl Into<PathBuf>) -> Self {
        Self {
            control_dir: control_dir.into(),
        }
    }

    /// Quiesce the guest filesystem. Blocks until the daemon acknowledges or
    /// `timeout` elapses. The returned [`QuiesceTicket`] must eventually be
    /// released; on drop without release, the ticket writes `resume.req`
    /// best-effort so the guest unfreezes even after a panic.
    pub fn freeze(&self, action: QuiesceAction, timeout: Duration) -> Result<QuiesceTicket> {
        if !self.control_dir.exists() {
            return Err(Error::NotFound(format!(
                "runtime-control directory {}",
                self.control_dir.display()
            )));
        }
        let lock = LockGuard::acquire(&self.control_dir.join(QUIESCE_LOCK))?;

        // Sweep stale files from a prior cycle that died mid-flight.
        for name in [QUIESCE_REQ, QUIESCE_ACK, RESUME_REQ, RESUME_ACK] {
            let _ = fs::remove_file(self.control_dir.join(name));
        }

        let req_id = unique_id();
        let deadline_unix_millis = unix_millis() + safety_window().as_millis();
        let request = format!(
            "id={req_id}\naction={}\ndeadline_unix_millis={deadline_unix_millis}\n",
            action.as_str()
        );
        atomic_write(&self.control_dir.join(QUIESCE_REQ), &request)?;

        let ack_path = self.control_dir.join(QUIESCE_ACK);
        let ack = poll_for_ack(&ack_path, &req_id, timeout).map_err(|err| {
            // Even on timeout the request file is still in place; the guest
            // may eventually see it. Send a best-effort resume to unwind any
            // partial state and surface the original error to the caller.
            let _ = self.write_resume_request_internal();
            err
        })?;
        let status = ack.field_str("status").unwrap_or("");
        if status != "ok" {
            let message = ack
                .field_str("message")
                .map(str::to_string)
                .unwrap_or_else(|| status.to_string());
            let _ = self.write_resume_request_internal();
            return Err(Error::Internal(format!(
                "guest quiesce daemon reported error: {message}"
            )));
        }
        let achieved = ack
            .field_u128("achieved_unix_millis")
            .unwrap_or_else(unix_millis);
        let _ = fs::remove_file(&ack_path);

        Ok(QuiesceTicket {
            control_dir: self.control_dir.clone(),
            achieved_unix_millis: achieved,
            action,
            released: false,
            _lock: lock,
        })
    }

    fn write_resume_request_internal(&self) -> Result<()> {
        let resume_id = unique_id();
        let request = format!("id={resume_id}\n");
        atomic_write(&self.control_dir.join(RESUME_REQ), &request)
    }
}

/// Handle on an in-flight freeze. Drop fires a best-effort `resume.req` so
/// the guest never stays frozen because a Rust panic skipped the explicit
/// release path.
#[derive(Debug)]
pub struct QuiesceTicket {
    control_dir: PathBuf,
    achieved_unix_millis: u128,
    action: QuiesceAction,
    released: bool,
    _lock: LockGuard,
}

impl QuiesceTicket {
    pub fn achieved_unix_millis(&self) -> u128 {
        self.achieved_unix_millis
    }

    pub fn action(&self) -> QuiesceAction {
        self.action
    }

    /// Send a resume request and wait for the daemon to acknowledge it. If
    /// the daemon does not ack within `timeout`, the guest's own deadline
    /// safety net (`deadline_unix_millis` recorded in the original
    /// `quiesce.req`) eventually auto-resumes the rootfs; this method still
    /// returns the error so the caller can surface the problem.
    pub fn release(mut self, timeout: Duration) -> Result<()> {
        let result = self.release_inner(timeout);
        // Mark as released regardless of outcome — on error the daemon's
        // deadline + Drop's resume.req both still apply, but the explicit
        // release attempt is over and Drop should not retry.
        self.released = true;
        result
    }

    fn release_inner(&self, timeout: Duration) -> Result<()> {
        let resume_id = unique_id();
        let resume_path = self.control_dir.join(RESUME_REQ);
        let ack_path = self.control_dir.join(RESUME_ACK);
        // Drop any stale resume.ack from a previous cycle so our poll loop
        // does not match the wrong id.
        let _ = fs::remove_file(&ack_path);
        let request = format!("id={resume_id}\n");
        atomic_write(&resume_path, &request)?;
        let ack = poll_for_ack(&ack_path, &resume_id, timeout)?;
        let status = ack.field_str("status").unwrap_or("");
        if status != "ok" {
            let message = ack
                .field_str("message")
                .map(str::to_string)
                .unwrap_or_else(|| status.to_string());
            return Err(Error::Internal(format!(
                "guest resume reported error: {message}"
            )));
        }
        let _ = fs::remove_file(&resume_path);
        let _ = fs::remove_file(&ack_path);
        Ok(())
    }
}

impl Drop for QuiesceTicket {
    fn drop(&mut self) {
        if !self.released {
            let resume_id = unique_id();
            let request = format!("id={resume_id}\n");
            let _ = atomic_write(&self.control_dir.join(RESUME_REQ), &request);
        }
    }
}

// ----------------------------------------------------------------------------

/// Parsed key=value response from the guest. Lenient: missing keys are
/// reported as `None`, blank or malformed lines are skipped.
struct ParsedResponse {
    fields: Vec<(String, String)>,
}

impl ParsedResponse {
    fn parse(text: &str) -> Self {
        let mut fields = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                fields.push((key.trim().to_string(), value.trim().to_string()));
            }
        }
        Self { fields }
    }

    fn field_str(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn field_u128(&self, key: &str) -> Option<u128> {
        self.field_str(key)?.parse().ok()
    }
}

fn poll_for_ack(path: &Path, expected_id: &str, timeout: Duration) -> Result<ParsedResponse> {
    let deadline = Instant::now() + timeout;
    let poll = poll_interval();
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            let parsed = ParsedResponse::parse(&contents);
            if parsed.field_str("id") == Some(expected_id) {
                return Ok(parsed);
            }
            // Wrong id (stale or for someone else's cycle). Keep polling
            // until the daemon writes one with our id, or we time out.
        }
        if Instant::now() >= deadline {
            let label = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("ack file");
            return Err(Error::Internal(format!(
                "guest quiesce daemon did not write {label} with id={expected_id} \
                 within {timeout:?} (control dir: {})",
                path.parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )));
        }
        std::thread::sleep(poll);
    }
}

/// Write `contents` to `path` atomically (write a sibling temp file, fsync,
/// rename). The temp name includes the pid so two wrappers writing
/// concurrently do not stomp each other (the flock above is the real
/// serialization, this is defense in depth).
fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("req");
    let tmp_path = parent.join(format!(".{}.tmp.{}", file_name, std::process::id()));
    {
        let mut tmp = File::create(&tmp_path).map_err(|err| Error::io_path(&tmp_path, err))?;
        tmp.write_all(contents.as_bytes())
            .map_err(|err| Error::io_path(&tmp_path, err))?;
        tmp.sync_all()
            .map_err(|err| Error::io_path(&tmp_path, err))?;
    }
    fs::rename(&tmp_path, path).map_err(|err| Error::io_path(path, err))
}

fn unique_id() -> String {
    // Nanos since the epoch is monotonic at the granularity we need (one
    // process generates at most a handful per snapshot) and trivial to grep
    // for in logs.
    format!(
        "{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn poll_interval() -> Duration {
    // 25ms keeps wrapper-side ack latency low. Total round-trip is bounded
    // by the guest's 100ms poll + filesystem flush + this poll.
    Duration::from_millis(25)
}

fn safety_window() -> Duration {
    // Maximum time the guest will stay frozen if the wrapper crashes mid-
    // snapshot. 5 minutes is conservative enough to allow snapshotting a
    // very large disk on a slow host while still bounding the worst-case
    // wedge.
    Duration::from_secs(5 * 60)
}

// ----------------------------------------------------------------------------
// Host-side file lock so concurrent snapshot() calls on the same sandbox
// serialize their quiesce cycles instead of stomping each other's request
// files.

#[cfg(unix)]
#[derive(Debug)]
struct LockGuard {
    file: File,
}

#[cfg(unix)]
impl LockGuard {
    fn acquire(path: &Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|err| Error::io_path(path, err))?;
        let fd = file.as_raw_fd();
        // Block on the lock — concurrent snapshots are uncommon, and waiting
        // is safer than racing. flock auto-releases on file close (and on
        // process exit), so a panic cannot leave a stale lock.
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if ret != 0 {
            return Err(Error::io(std::io::Error::last_os_error()));
        }
        Ok(Self { file })
    }
}

#[cfg(unix)]
impl Drop for LockGuard {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(not(unix))]
#[derive(Debug)]
struct LockGuard;

#[cfg(not(unix))]
impl LockGuard {
    fn acquire(_path: &Path) -> Result<Self> {
        Ok(Self)
    }
}

// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    fn temp_control_dir(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "tb-sandbox-quiesce-{label}-{}-{}",
            std::process::id(),
            unique_id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Spawn a thread that plays the guest daemon role on `dir`. Polls for
    /// `quiesce.req`, writes a matching `quiesce.ack` (with `status` from
    /// `ack_status`), then polls for `resume.req` and writes `resume.ack`.
    /// Exits when `stop` is set.
    fn spawn_fake_guest(dir: PathBuf, ack_status: &'static str, stop: Arc<AtomicBool>) {
        thread::spawn(move || {
            let mut handled_quiesce: Option<String> = None;
            while !stop.load(Ordering::SeqCst) {
                if handled_quiesce.is_none() {
                    if let Ok(text) = fs::read_to_string(dir.join(QUIESCE_REQ)) {
                        let parsed = ParsedResponse::parse(&text);
                        if let Some(id) = parsed.field_str("id") {
                            let action = parsed.field_str("action").unwrap_or("sync");
                            let ack = format!(
                                "id={id}\nstatus={ack_status}\naction={action}\nachieved_unix_millis={}\n",
                                unix_millis()
                            );
                            atomic_write(&dir.join(QUIESCE_ACK), &ack).unwrap();
                            handled_quiesce = Some(id.to_string());
                            // Wrapper deletes quiesce.req only via its own
                            // sweep on the next cycle — leave it in place.
                        }
                    }
                } else if let Ok(text) = fs::read_to_string(dir.join(RESUME_REQ)) {
                    let parsed = ParsedResponse::parse(&text);
                    if let Some(id) = parsed.field_str("id") {
                        let ack = format!(
                            "id={id}\nstatus=ok\nresumed_unix_millis={}\n",
                            unix_millis()
                        );
                        atomic_write(&dir.join(RESUME_ACK), &ack).unwrap();
                        handled_quiesce = None;
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
    }

    #[test]
    fn happy_path_round_trip() {
        let dir = temp_control_dir("happy");
        let stop = Arc::new(AtomicBool::new(false));
        spawn_fake_guest(dir.clone(), "ok", stop.clone());

        let client = QuiesceClient::new(&dir);
        let ticket = client
            .freeze(QuiesceAction::Freeze, Duration::from_secs(2))
            .expect("freeze should succeed");
        assert_eq!(ticket.action(), QuiesceAction::Freeze);
        assert!(ticket.achieved_unix_millis() > 0);
        ticket
            .release(Duration::from_secs(2))
            .expect("release should succeed");
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn freeze_times_out_when_no_guest() {
        let dir = temp_control_dir("no-guest");
        let client = QuiesceClient::new(&dir);
        let err = client
            .freeze(QuiesceAction::Sync, Duration::from_millis(150))
            .expect_err("expected timeout");
        let msg = err.to_string();
        assert!(msg.contains("quiesce.ack"), "got: {msg}");
        // Best-effort resume.req should have been written on the timeout
        // path so the guest unfreezes if it later appears.
        assert!(dir.join(RESUME_REQ).exists());
    }

    #[test]
    fn freeze_surfaces_guest_error_status() {
        let dir = temp_control_dir("err-status");
        let stop = Arc::new(AtomicBool::new(false));
        spawn_fake_guest(dir.clone(), "error", stop.clone());

        let client = QuiesceClient::new(&dir);
        let err = client
            .freeze(QuiesceAction::Freeze, Duration::from_secs(2))
            .expect_err("expected guest error to surface");
        assert!(err
            .to_string()
            .contains("guest quiesce daemon reported error"));
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn drop_without_release_sends_resume_request() {
        let dir = temp_control_dir("drop-resume");
        let stop = Arc::new(AtomicBool::new(false));
        spawn_fake_guest(dir.clone(), "ok", stop.clone());

        let client = QuiesceClient::new(&dir);
        {
            let _ticket = client
                .freeze(QuiesceAction::Sync, Duration::from_secs(2))
                .unwrap();
            // Drop without calling release — RAII must still leave a
            // resume.req on disk for the guest to find.
        }
        // The Drop is best-effort and synchronous; the file is written
        // before this assertion runs.
        assert!(dir.join(RESUME_REQ).exists());
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn parser_skips_blank_and_malformed_lines() {
        let parsed = ParsedResponse::parse("\nid=abc\nnotakey\nstatus=ok\n\n");
        assert_eq!(parsed.field_str("id"), Some("abc"));
        assert_eq!(parsed.field_str("status"), Some("ok"));
        assert_eq!(parsed.field_str("missing"), None);
    }

    #[test]
    fn freeze_fails_fast_when_control_dir_missing() {
        let client = QuiesceClient::new("/nonexistent/control/dir/for/test");
        let err = client
            .freeze(QuiesceAction::Sync, Duration::from_secs(1))
            .expect_err("should fail");
        assert!(matches!(err, Error::NotFound(_)));
    }
}
