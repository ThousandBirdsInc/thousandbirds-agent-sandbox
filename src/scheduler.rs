//! Single-host memory admission control.
//!
//! The scheduler keeps the host from oversubscribing RAM across concurrently
//! running sandboxes. Each running VM writes a `running.json` reservation marker
//! into its state directory; admission sums the live reservations and refuses
//! (or queues) a new launch that would exceed the configured budget. Markers are
//! self-reaping: a marker whose recorded launcher PID is no longer alive is
//! deleted on the next scan, so a crashed VM never permanently holds budget.
//!
//! Reservations are based on a sandbox's *initial* usable memory (`memory_mib`),
//! not its resize ceiling — HVF backs guest pages on demand and the balloon /
//! idle reclaim return unused pages, so the ceiling overstates real cost. See
//! docs/vm-density-and-resource-sharing-design.md (#B).

use crate::error::{Error, Result};
use crate::image::{json_number, json_string};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What to do when an admission request would exceed the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionPolicy {
    /// Fail fast with [`Error::ResourceExhausted`].
    Reject,
    /// Block, polling until budget frees up or the timeout elapses.
    Queue { timeout: Duration },
}

/// A live reservation read back from a sandbox's `running.json` marker.
#[derive(Debug, Clone)]
pub struct Reservation {
    pub id: String,
    pub pid: u32,
    pub reserved_mib: u64,
}

/// Memory admission ledger over a `<state_root>/sandboxes` tree.
#[derive(Debug, Clone)]
pub struct ResourceLedger {
    sandboxes_root: PathBuf,
    /// `None` disables admission control entirely (unlimited).
    budget_mib: Option<u64>,
    policy: AdmissionPolicy,
}

const RUNNING_MARKER: &str = "running.json";

impl ResourceLedger {
    /// Build a ledger from environment configuration:
    /// - `TB_SANDBOX_MEM_BUDGET_MIB`: host memory budget in MiB. `0` (or unset
    ///   with no detectable host memory) disables admission control. When unset
    ///   and host memory is detectable, defaults to 80% of physical RAM.
    /// - `TB_SANDBOX_ADMISSION`: `reject` (default) or `queue`.
    /// - `TB_SANDBOX_ADMISSION_TIMEOUT_SECS`: queue timeout (default 300).
    pub fn from_env(state_root: &Path) -> Self {
        let budget_mib = match std::env::var("TB_SANDBOX_MEM_BUDGET_MIB") {
            Ok(value) => match value.trim().parse::<u64>() {
                Ok(0) => None,
                Ok(mib) => Some(mib),
                Err(_) => default_budget_mib(),
            },
            Err(_) => default_budget_mib(),
        };
        let policy = match std::env::var("TB_SANDBOX_ADMISSION").ok().as_deref() {
            Some("queue") => {
                let secs = std::env::var("TB_SANDBOX_ADMISSION_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(300);
                AdmissionPolicy::Queue {
                    timeout: Duration::from_secs(secs),
                }
            }
            _ => AdmissionPolicy::Reject,
        };
        Self {
            sandboxes_root: state_root.join("sandboxes"),
            budget_mib,
            policy,
        }
    }

    /// Total MiB reserved by live sandboxes, excluding `exclude_id`. Reaps any
    /// markers whose launcher PID is no longer running.
    pub fn reserved_mib_excluding(&self, exclude_id: &str) -> u64 {
        self.live_reservations()
            .into_iter()
            .filter(|r| r.id != exclude_id)
            .map(|r| r.reserved_mib)
            .sum()
    }

    /// All live reservations (markers with a running PID). Stale markers are
    /// deleted as a side effect.
    pub fn live_reservations(&self) -> Vec<Reservation> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.sandboxes_root) {
            Ok(entries) => entries,
            Err(_) => return out,
        };
        for entry in entries.flatten() {
            let marker = entry.path().join(RUNNING_MARKER);
            let Ok(contents) = std::fs::read_to_string(&marker) else {
                continue;
            };
            let Some(pid) = json_number(&contents, "pid").map(|p| p as u32) else {
                continue;
            };
            if !pid_is_running(pid) {
                let _ = std::fs::remove_file(&marker);
                continue;
            }
            let reserved_mib = json_number(&contents, "reserved_mib").unwrap_or(0);
            let id = json_string(&contents, "id").unwrap_or_else(|| {
                entry
                    .file_name()
                    .to_str()
                    .map(str::to_string)
                    .unwrap_or_default()
            });
            out.push(Reservation {
                id,
                pid,
                reserved_mib,
            });
        }
        out
    }

    /// Check that admitting `want_mib` for sandbox `id` fits the budget. Returns
    /// `Ok(())` if admission control is disabled, the request fits, or (under the
    /// queue policy) budget frees up before the timeout. Errors with
    /// [`Error::ResourceExhausted`] otherwise.
    pub fn admit(&self, id: &str, want_mib: u64) -> Result<()> {
        let Some(budget) = self.budget_mib else {
            return Ok(());
        };
        let deadline = match self.policy {
            AdmissionPolicy::Queue { timeout } => Some(Instant::now() + timeout),
            AdmissionPolicy::Reject => None,
        };
        let mut waited_notice = false;
        loop {
            let reserved = self.reserved_mib_excluding(id);
            if reserved + want_mib <= budget {
                return Ok(());
            }
            let message = format!(
                "starting sandbox {id} needs {want_mib} MiB but only {} MiB of the \
                 {budget} MiB host budget is free ({reserved} MiB reserved by running \
                 sandboxes); raise TB_SANDBOX_MEM_BUDGET_MIB, stop a sandbox, or set \
                 TB_SANDBOX_ADMISSION=queue",
                budget.saturating_sub(reserved)
            );
            match deadline {
                None => return Err(Error::ResourceExhausted(message)),
                Some(deadline) => {
                    if Instant::now() >= deadline {
                        return Err(Error::ResourceExhausted(format!(
                            "{message} (gave up after queue timeout)"
                        )));
                    }
                    if !waited_notice {
                        eprintln!(
                            "tb-sandbox: host memory budget full; waiting for capacity to start {id}"
                        );
                        waited_notice = true;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    /// Write the running reservation marker for a launched sandbox.
    pub fn record_running(
        &self,
        sandbox_dir: &Path,
        id: &str,
        pid: u32,
        reserved_mib: u64,
    ) -> Result<()> {
        let marker = sandbox_dir.join(RUNNING_MARKER);
        let contents = format!(
            "{{\n  \"id\": \"{}\",\n  \"pid\": {},\n  \"reserved_mib\": {}\n}}\n",
            crate::credentials::escape_json(id),
            pid,
            reserved_mib
        );
        std::fs::write(&marker, contents).map_err(|err| Error::io_path(marker, err))
    }

    /// Remove a sandbox's running marker. Best-effort; missing is not an error.
    pub fn clear_running(sandbox_dir: &Path) {
        let _ = std::fs::remove_file(sandbox_dir.join(RUNNING_MARKER));
    }

    /// Whether a specific sandbox currently has a live running marker.
    pub fn is_running(sandbox_dir: &Path) -> bool {
        let marker = sandbox_dir.join(RUNNING_MARKER);
        let Ok(contents) = std::fs::read_to_string(&marker) else {
            return false;
        };
        match json_number(&contents, "pid").map(|p| p as u32) {
            Some(pid) if pid_is_running(pid) => true,
            _ => {
                let _ = std::fs::remove_file(&marker);
                false
            }
        }
    }

    /// Reserved MiB recorded for a single sandbox if it is live, else `None`.
    pub fn reserved_for(sandbox_dir: &Path) -> Option<u64> {
        let marker = sandbox_dir.join(RUNNING_MARKER);
        let contents = std::fs::read_to_string(&marker).ok()?;
        let pid = json_number(&contents, "pid").map(|p| p as u32)?;
        if !pid_is_running(pid) {
            return None;
        }
        Some(json_number(&contents, "reserved_mib").unwrap_or(0))
    }

    pub fn budget_mib(&self) -> Option<u64> {
        self.budget_mib
    }
}

/// Default budget: 80% of detected physical RAM, or `None` (unlimited) when host
/// memory cannot be detected.
fn default_budget_mib() -> Option<u64> {
    host_memory_mib().map(|total| total * 4 / 5)
}

/// Physical RAM in MiB.
#[cfg(target_os = "macos")]
fn host_memory_mib() -> Option<u64> {
    let output = std::process::Command::new("sysctl")
        .arg("-n")
        .arg("hw.memsize")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let bytes: u64 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
    Some(bytes / (1024 * 1024))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn host_memory_mib() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kib / 1024);
        }
    }
    None
}

#[cfg(not(unix))]
fn host_memory_mib() -> Option<u64> {
    None
}

#[cfg(unix)]
fn pid_is_running(pid: u32) -> bool {
    // kill(0, ...) targets the caller's whole process group, not a real PID, so
    // treat 0 as "not running" rather than letting it spuriously succeed.
    if pid == 0 {
        return false;
    }
    // Signal 0 performs error checking without delivering a signal: success or
    // EPERM means the process exists; ESRCH means it does not.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_is_running(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "tb-sandbox-sched-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sandboxes")).unwrap();
        root
    }

    fn ledger(root: &Path, budget: Option<u64>) -> ResourceLedger {
        ResourceLedger {
            sandboxes_root: root.join("sandboxes"),
            budget_mib: budget,
            policy: AdmissionPolicy::Reject,
        }
    }

    #[test]
    fn unlimited_budget_always_admits() {
        let root = temp_root("unlimited");
        let l = ledger(&root, None);
        assert!(l.admit("a", 1_000_000).is_ok());
    }

    #[test]
    fn live_reservation_counts_against_budget() {
        let root = temp_root("live");
        let dir = root.join("sandboxes").join("a");
        std::fs::create_dir_all(&dir).unwrap();
        let l = ledger(&root, Some(4096));
        // Record a live reservation for the current process (definitely alive).
        l.record_running(&dir, "a", std::process::id(), 3072).unwrap();
        assert_eq!(l.reserved_mib_excluding("other"), 3072);
        // 3072 reserved + 2048 want > 4096 budget -> rejected.
        assert!(matches!(
            l.admit("b", 2048),
            Err(Error::ResourceExhausted(_))
        ));
        // Excluding the holder itself frees the budget.
        assert!(l.admit("a", 2048).is_ok());
    }

    #[test]
    fn stale_marker_is_reaped() {
        let root = temp_root("stale");
        let dir = root.join("sandboxes").join("a");
        std::fs::create_dir_all(&dir).unwrap();
        let l = ledger(&root, Some(4096));
        // PID 0 is never a real user process; treated as not running.
        l.record_running(&dir, "a", 0, 3072).unwrap();
        assert_eq!(l.reserved_mib_excluding("other"), 0);
        assert!(!dir.join(RUNNING_MARKER).exists());
    }
}
