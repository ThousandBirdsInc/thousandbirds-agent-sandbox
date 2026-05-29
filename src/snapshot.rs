//! Sandbox snapshots, restore, and VM forking — disk-state only.
//!
//! ## What this is
//!
//! A snapshot is a directory under `<state_root>/snapshots/<snapshot_id>/`
//! that bundles a sandbox's **root disk** at a point in time (APFS copy-on-write
//! cloned, so creation is ~5 ms) together with a manifest of the sandbox's
//! configuration. A restore consumes a snapshot to create a fresh sandbox whose
//! root disk starts from the snapshot. A fork is the same operation in one step
//! with no persisted intermediate snapshot.
//!
//! ## What this is NOT
//!
//! This is **disk-only** snapshot/restore. Guest memory, vCPU state, and live
//! processes are not preserved — a restored sandbox cold-boots from the
//! snapshot's disk. Memory + process preservation is the multi-month HVF work
//! tracked in
//! `../thousandbirds-libkrun/design_docs/snapshot_restore.md` and Phase 3 of
//! `docs/closing-the-smolvm-gap-plan.md`. The manifest carries a `kind` field
//! (`"disk-only"` today) so a later `"full"` kind can land without breaking the
//! on-disk format.
//!
//! ## On-disk layout
//!
//! ```text
//! <state_root>/snapshots/<snapshot_id>/
//!   manifest.json          # everything below, plus a human-readable note
//!   root.raw               # APFS-cloned root disk at snapshot time
//!   credentials-staging/   # present iff the parent sandbox had staged creds
//! ```
//!
//! ## Crash consistency
//!
//! If the parent sandbox is running at snapshot time, the disk clone is
//! **crash-consistent** — equivalent to pulling the power on a real machine.
//! Filesystem replay on boot will roll the disk forward to a consistent state.
//! There is no cooperative quiesce hook yet (Phase 2 of the gap plan); for
//! agents this is acceptable because guests run journaling filesystems.

use crate::config::{
    NetworkBackend, NetworkProxyConfig, ProxyHeaderRule, SandboxConfig, WorkspaceMount,
};
use crate::credentials::escape_json;
use crate::error::{Error, Result};
use crate::image::{json_bool, json_number, json_string, json_string_array};
use std::path::{Path, PathBuf};

/// Validated identifier for a snapshot directory under `<state>/snapshots/`.
///
/// Same character class as [`crate::runtime::SandboxId`] so snapshot IDs are
/// safe directory names and can round-trip through the CLI without escaping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotId(String);

impl SnapshotId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || !value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Err(Error::InvalidArgument(format!(
                "snapshot id must contain only letters, numbers, '-' or '_': {value}"
            )));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a snapshot captures. Open enum on disk so a future `"full"` kind that
/// also captures memory + vCPU state can land without breaking restore of
/// existing artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    /// Root disk + sandbox configuration. No memory, no live processes.
    DiskOnly,
}

impl SnapshotKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DiskOnly => "disk-only",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "disk-only" => Ok(Self::DiskOnly),
            other => Err(Error::InvalidArgument(format!(
                "unsupported snapshot kind {other}; this build only understands disk-only"
            ))),
        }
    }
}

/// On-disk record of what a snapshot captured. Mirrors the sandbox metadata
/// fields restore needs to rebuild a working sandbox configuration.
#[derive(Debug, Clone)]
pub struct SnapshotManifest {
    pub version: u32,
    pub kind: SnapshotKind,
    pub id: SnapshotId,
    /// ID of the sandbox the snapshot was taken from. Stored for traceability;
    /// the source sandbox may have been deleted by restore time.
    pub parent_sandbox_id: String,
    pub created_unix_millis: u128,

    /// Agent image directory the parent sandbox was launched from. Restore
    /// reuses the same kernel/initramfs.
    pub image_path: PathBuf,

    pub cpus: u8,
    pub memory_mib: u32,
    pub memory_max_mib: u32,
    pub memory_min_mib: u32,
    pub virtiofs_dax_size: u64,

    pub workspace_mounts: Vec<WorkspaceMount>,
    pub network_backend: NetworkBackend,
    pub network_proxy: NetworkProxyConfig,

    /// True when `credentials-staging/` is present in the snapshot directory.
    pub had_credentials: bool,
}

impl SnapshotManifest {
    /// Serialize to the JSON shape stored at `<snap_dir>/manifest.json`.
    pub fn to_json(&self) -> String {
        let mounts = self
            .workspace_mounts
            .iter()
            .map(|mount| {
                format!(
                    "\"{}\"",
                    escape_json(&format!(
                        "{}|{}|{}|{}",
                        mount.tag,
                        mount.host_path.display(),
                        mount.guest_path,
                        mount.read_only
                    ))
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let rules = self
            .network_proxy
            .header_rules
            .iter()
            .map(|rule| format!("\"{}\"", escape_json(&rule.encode())))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            concat!(
                "{{\n",
                "  \"version\": {version},\n",
                "  \"kind\": \"{kind}\",\n",
                "  \"id\": \"{id}\",\n",
                "  \"parent_sandbox_id\": \"{parent}\",\n",
                "  \"created_unix_millis\": {created},\n",
                "  \"image_path\": \"{image}\",\n",
                "  \"cpus\": {cpus},\n",
                "  \"memory_mib\": {memory_mib},\n",
                "  \"memory_max_mib\": {memory_max_mib},\n",
                "  \"memory_min_mib\": {memory_min_mib},\n",
                "  \"virtiofs_dax_size\": {dax},\n",
                "  \"network_backend\": \"{backend}\",\n",
                "  \"network_proxy_enabled\": {proxy_enabled},\n",
                "  \"network_proxy_tls_mitm\": {tls_mitm},\n",
                "  \"proxy_header_rules\": [{rules}],\n",
                "  \"workspace_mounts\": [{mounts}],\n",
                "  \"had_credentials\": {had_credentials},\n",
                "  \"note\": \"disk-only snapshot; guest memory and live processes are not preserved\"\n",
                "}}\n"
            ),
            version = self.version,
            kind = self.kind.as_str(),
            id = escape_json(self.id.as_str()),
            parent = escape_json(&self.parent_sandbox_id),
            created = self.created_unix_millis,
            image = escape_json(&self.image_path.display().to_string()),
            cpus = self.cpus,
            memory_mib = self.memory_mib,
            memory_max_mib = self.memory_max_mib,
            memory_min_mib = self.memory_min_mib,
            dax = self.virtiofs_dax_size,
            backend = escape_json(&self.network_backend.encode()),
            proxy_enabled = self.network_proxy.enabled,
            tls_mitm = self.network_proxy.tls_mitm,
            rules = rules,
            mounts = mounts,
            had_credentials = self.had_credentials,
        )
    }

    /// Parse a manifest from its JSON-on-disk form. Strict enough that an
    /// unrecognized `kind` fails fast (so a future `"full"` snapshot does not
    /// silently degrade to disk-only).
    pub fn from_json(json: &str) -> Result<Self> {
        let id = json_string(json, "id")
            .ok_or_else(|| Error::InvalidArgument("manifest missing 'id'".to_string()))?;
        let id = SnapshotId::new(id)?;
        let kind = json_string(json, "kind")
            .ok_or_else(|| Error::InvalidArgument("manifest missing 'kind'".to_string()))?;
        let kind = SnapshotKind::parse(&kind)?;
        let image_path = json_string(json, "image_path")
            .ok_or_else(|| Error::InvalidArgument("manifest missing 'image_path'".to_string()))?;
        let workspace_mounts = parse_manifest_mounts(json)?;
        let header_rules = parse_manifest_rules(json)?;
        let network_backend = parse_manifest_backend(json)?;
        Ok(Self {
            version: json_number(json, "version").unwrap_or(1) as u32,
            kind,
            id,
            parent_sandbox_id: json_string(json, "parent_sandbox_id").unwrap_or_default(),
            created_unix_millis: json_number(json, "created_unix_millis").unwrap_or(0) as u128,
            image_path: PathBuf::from(image_path),
            cpus: json_number(json, "cpus").unwrap_or(1) as u8,
            memory_mib: json_number(json, "memory_mib").unwrap_or(2048) as u32,
            memory_max_mib: json_number(json, "memory_max_mib")
                .map(|v| v as u32)
                .unwrap_or_else(|| json_number(json, "memory_mib").unwrap_or(2048) as u32),
            memory_min_mib: json_number(json, "memory_min_mib").unwrap_or(512) as u32,
            virtiofs_dax_size: json_number(json, "virtiofs_dax_size").unwrap_or(0),
            workspace_mounts,
            network_backend,
            network_proxy: NetworkProxyConfig {
                enabled: json_bool(json, "network_proxy_enabled").unwrap_or(true),
                tls_mitm: json_bool(json, "network_proxy_tls_mitm").unwrap_or(true),
                header_rules,
            },
            had_credentials: json_bool(json, "had_credentials").unwrap_or(false),
        })
    }
}

/// Summary returned by `SandboxManager::snapshots()` for listing.
#[derive(Debug, Clone)]
pub struct SnapshotSummary {
    pub id: SnapshotId,
    pub parent_sandbox_id: String,
    pub kind: SnapshotKind,
    pub created_unix_millis: u128,
    pub state_dir: PathBuf,
    /// Disk size of the snapshot's `root.raw` in bytes, when readable.
    pub root_disk_bytes: Option<u64>,
}

/// Options for `SandboxManager::snapshot`.
#[derive(Debug, Clone, Default)]
pub struct SnapshotOptions {
    /// Override the auto-generated snapshot ID. Validated as a SnapshotId.
    pub name: Option<String>,
}

/// Options for `SandboxManager::restore`. Every field is an override; the
/// snapshot manifest supplies the defaults. The restored sandbox is a fresh
/// cold boot from the snapshotted disk — there is no guest-memory or live
/// process preservation.
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// ID for the restored sandbox. Defaults to `restore-<unix_millis>`.
    pub name: Option<String>,
    /// Overrides the workspace mounts recorded in the snapshot. Pass an empty
    /// `Some(vec![])` to drop them entirely; `None` to inherit.
    pub workspace_mounts: Option<Vec<WorkspaceMount>>,
    /// Skip copying the snapshot's `credentials-staging/` into the new
    /// sandbox. Defaults to inheriting them.
    pub drop_credentials: bool,
    /// Overrides the network backend recorded in the snapshot. `None`
    /// inherits the snapshot's backend.
    pub network_backend: Option<NetworkBackend>,
    /// Override initial memory. `None` inherits the snapshot.
    pub memory_mib: Option<u32>,
    /// Override the resize ceiling. `None` inherits the snapshot.
    pub memory_max_mib: Option<u32>,
    /// Override the resize floor. `None` inherits the snapshot.
    pub memory_min_mib: Option<u32>,
    /// Override vCPU count. `None` inherits the snapshot.
    pub cpus: Option<u8>,
    /// Mark the restored sandbox as ephemeral (deleted when the VM exits).
    pub ephemeral: bool,
}

/// Options for `SandboxManager::fork`. Fork inherits the parent's config
/// wholesale by default; overrides apply on top.
#[derive(Debug, Clone, Default)]
pub struct ForkOptions {
    /// ID for the child sandbox. Defaults to `<parent-id>-fork-<unix_millis>`.
    pub name: Option<String>,
    /// Drop the parent's credentials-staging from the child. Defaults to
    /// inheriting (matches the user expectation that a fork is a sibling).
    pub drop_credentials: bool,
    /// Replace the parent's workspace mounts. `None` inherits them. Forks
    /// often want the same mounts so siblings see the same host state.
    pub workspace_mounts: Option<Vec<WorkspaceMount>>,
    /// Mark the forked sandbox as ephemeral.
    pub ephemeral: bool,
}

/// Build a [`SandboxConfig`] from a manifest plus restore-time overrides.
/// Used by `SandboxManager::restore`. Lives here so the manifest schema and
/// the overlay rules stay co-located.
pub fn config_from_manifest(
    manifest: &SnapshotManifest,
    overrides: &RestoreOptions,
) -> SandboxConfig {
    let mut config = SandboxConfig::new(crate::config::AgentImageRef::new(
        manifest.image_path.clone(),
    ));
    config.name = overrides.name.clone();
    config.cpus = overrides.cpus.unwrap_or(manifest.cpus);
    config.memory_mib = overrides.memory_mib.unwrap_or(manifest.memory_mib);
    config.memory_max_mib = overrides.memory_max_mib.unwrap_or(manifest.memory_max_mib);
    config.memory_min_mib = overrides.memory_min_mib.unwrap_or(manifest.memory_min_mib);
    config.virtiofs_dax_size = manifest.virtiofs_dax_size;
    config.workspace_mounts = overrides
        .workspace_mounts
        .clone()
        .unwrap_or_else(|| manifest.workspace_mounts.clone());
    config.network_backend = overrides
        .network_backend
        .clone()
        .unwrap_or_else(|| manifest.network_backend.clone());
    config.network_proxy = manifest.network_proxy.clone();
    config.ephemeral = overrides.ephemeral;
    // Ceiling/floor sanity. Clamp to maintain the invariant
    // `min <= initial <= max` that `parse_sandbox_config` enforces at the CLI.
    if config.memory_max_mib < config.memory_mib {
        config.memory_max_mib = config.memory_mib;
    }
    if config.memory_min_mib > config.memory_mib {
        config.memory_min_mib = config.memory_mib;
    }
    config
}

fn parse_manifest_mounts(json: &str) -> Result<Vec<WorkspaceMount>> {
    let Some(items) = json_string_array(json, "workspace_mounts") else {
        return Ok(Vec::new());
    };
    let mut mounts = Vec::new();
    for item in items {
        let parts: Vec<_> = item.split('|').collect();
        if parts.len() != 4 {
            return Err(Error::InvalidArgument(format!(
                "invalid snapshot mount metadata: {item}"
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

fn parse_manifest_rules(json: &str) -> Result<Vec<ProxyHeaderRule>> {
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

fn parse_manifest_backend(json: &str) -> Result<NetworkBackend> {
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
        "invalid network backend metadata in snapshot: {value}"
    )))
}

/// Resolve the directory a snapshot lives in under `state_root`.
pub fn snapshot_dir(state_root: &Path, id: &SnapshotId) -> PathBuf {
    state_root.join("snapshots").join(id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> SnapshotManifest {
        SnapshotManifest {
            version: 1,
            kind: SnapshotKind::DiskOnly,
            id: SnapshotId::new("snap-test").unwrap(),
            parent_sandbox_id: "src-sandbox".to_string(),
            created_unix_millis: 1_700_000_000_000,
            image_path: PathBuf::from("/img/debian"),
            cpus: 2,
            memory_mib: 2048,
            memory_max_mib: 8192,
            memory_min_mib: 512,
            virtiofs_dax_size: 1024 * 1024 * 512,
            workspace_mounts: vec![WorkspaceMount {
                tag: "workspace".to_string(),
                host_path: PathBuf::from("/host/ws"),
                guest_path: "/workspace".to_string(),
                read_only: false,
            }],
            network_backend: NetworkBackend::Gvproxy {
                socket: PathBuf::from("/tmp/gv.sock"),
            },
            network_proxy: NetworkProxyConfig {
                enabled: true,
                tls_mitm: false,
                header_rules: vec![ProxyHeaderRule {
                    host: "api.example.test".to_string(),
                    header: "authorization".to_string(),
                    value: "Bearer test".to_string(),
                    value_from_env: false,
                }],
            },
            had_credentials: true,
        }
    }

    #[test]
    fn manifest_round_trip() {
        let original = sample_manifest();
        let json = original.to_json();
        let parsed = SnapshotManifest::from_json(&json).unwrap();
        assert_eq!(parsed.id.as_str(), original.id.as_str());
        assert_eq!(parsed.kind, original.kind);
        assert_eq!(parsed.parent_sandbox_id, original.parent_sandbox_id);
        assert_eq!(parsed.image_path, original.image_path);
        assert_eq!(parsed.cpus, original.cpus);
        assert_eq!(parsed.memory_mib, original.memory_mib);
        assert_eq!(parsed.memory_max_mib, original.memory_max_mib);
        assert_eq!(parsed.memory_min_mib, original.memory_min_mib);
        assert_eq!(parsed.virtiofs_dax_size, original.virtiofs_dax_size);
        assert_eq!(parsed.workspace_mounts.len(), 1);
        assert_eq!(parsed.workspace_mounts[0].tag, "workspace");
        assert_eq!(
            parsed.workspace_mounts[0].guest_path,
            original.workspace_mounts[0].guest_path
        );
        assert!(matches!(
            parsed.network_backend,
            NetworkBackend::Gvproxy { .. }
        ));
        assert!(parsed.network_proxy.enabled);
        assert!(!parsed.network_proxy.tls_mitm);
        assert_eq!(parsed.network_proxy.header_rules.len(), 1);
        assert_eq!(
            parsed.network_proxy.header_rules[0].host,
            "api.example.test"
        );
        assert!(parsed.had_credentials);
    }

    #[test]
    fn snapshot_id_rejects_invalid_characters() {
        assert!(SnapshotId::new("ok-1").is_ok());
        assert!(SnapshotId::new("with space").is_err());
        assert!(SnapshotId::new("").is_err());
        assert!(SnapshotId::new("path/sep").is_err());
    }

    #[test]
    fn unknown_kind_fails_fast() {
        let mut json = sample_manifest().to_json();
        json = json.replace("\"disk-only\"", "\"full\"");
        let err = SnapshotManifest::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("disk-only"));
    }

    #[test]
    fn config_overrides_apply_on_top_of_manifest() {
        let manifest = sample_manifest();
        let overrides = RestoreOptions {
            name: Some("restored".to_string()),
            workspace_mounts: Some(Vec::new()),
            cpus: Some(4),
            memory_mib: Some(4096),
            memory_max_mib: Some(4096),
            memory_min_mib: Some(1024),
            drop_credentials: true,
            network_backend: Some(NetworkBackend::None),
            ephemeral: true,
        };
        let config = config_from_manifest(&manifest, &overrides);
        assert_eq!(config.name.as_deref(), Some("restored"));
        assert_eq!(config.cpus, 4);
        assert_eq!(config.memory_mib, 4096);
        assert_eq!(config.memory_max_mib, 4096);
        assert_eq!(config.memory_min_mib, 1024);
        assert!(config.workspace_mounts.is_empty());
        assert!(matches!(config.network_backend, NetworkBackend::None));
        assert!(config.ephemeral);
    }

    #[test]
    fn config_inherits_when_overrides_absent() {
        let manifest = sample_manifest();
        let overrides = RestoreOptions::default();
        let config = config_from_manifest(&manifest, &overrides);
        assert_eq!(config.cpus, manifest.cpus);
        assert_eq!(config.memory_mib, manifest.memory_mib);
        assert_eq!(config.memory_max_mib, manifest.memory_max_mib);
        assert_eq!(config.workspace_mounts.len(), 1);
        assert!(matches!(
            config.network_backend,
            NetworkBackend::Gvproxy { .. }
        ));
    }
}
