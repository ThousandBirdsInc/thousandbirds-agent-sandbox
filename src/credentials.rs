use crate::config::{CredentialProfile, CredentialSpec};
use crate::error::{Error, Result};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const CLAUDE_CREDENTIALS_RELATIVE_PATH: &str = ".claude/.credentials.json";
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const CLAUDE_PORTABLE_ENV_NAMES: &[&str] = &[
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
];
const CODEX_PORTABLE_ENV_NAMES: &[&str] = &["OPENAI_API_KEY"];

#[derive(Debug, Clone)]
pub struct PreparedCredentials {
    pub staging_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub copied: Vec<CredentialCopy>,
    pub env_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CredentialCopy {
    pub source_label: String,
    pub staged_path: PathBuf,
    pub guest_path: String,
    pub private_key: bool,
}

#[derive(Debug, Clone)]
struct Candidate {
    profile: String,
    host_path: PathBuf,
    guest_relative_path: PathBuf,
    optional: bool,
    private_key: bool,
}

pub fn prepare_credentials(
    staging_dir: &Path,
    profiles: &[CredentialProfile],
    extra: &[CredentialSpec],
    env_names: &[String],
    yes: bool,
) -> Result<PreparedCredentials> {
    if staging_dir.exists() {
        fs::remove_dir_all(staging_dir).map_err(|err| Error::io_path(staging_dir, err))?;
    }
    fs::create_dir_all(staging_dir).map_err(|err| Error::io_path(staging_dir, err))?;
    restrict_dir(staging_dir)?;

    let mut candidates = Vec::new();
    for profile in profiles {
        candidates.extend(profile_candidates(profile));
    }
    for spec in extra {
        candidates.push(Candidate {
            profile: "custom".to_string(),
            host_path: expand_home(&spec.host_path),
            guest_relative_path: spec.guest_relative_path.clone(),
            optional: false,
            private_key: looks_like_private_key(&spec.host_path),
        });
    }

    if !yes {
        let private_keys: Vec<_> = candidates
            .iter()
            .filter(|candidate| candidate.private_key && candidate.host_path.exists())
            .map(|candidate| candidate.host_path.display().to_string())
            .collect();
        if !private_keys.is_empty() {
            return Err(Error::InvalidCredential(format!(
                "copying private key material requires --yes: {}",
                private_keys.join(", ")
            )));
        }
    }

    let mut copied = Vec::new();
    for candidate in candidates {
        if !candidate.host_path.exists() {
            if candidate.optional {
                continue;
            }
            return Err(Error::InvalidCredential(format!(
                "{} does not exist",
                candidate.host_path.display()
            )));
        }
        validate_credential_path(&candidate.host_path)?;
        let staged_relative = Path::new(&candidate.profile).join(&candidate.guest_relative_path);
        let staged_path = staging_dir.join(&staged_relative);
        copy_path(&candidate.host_path, &staged_path)?;
        copied.push(CredentialCopy {
            source_label: display_home(&candidate.host_path),
            staged_path: staged_relative,
            guest_path: format!(
                "/home/agent/{}",
                candidate
                    .guest_relative_path
                    .display()
                    .to_string()
                    .trim_start_matches('/')
            ),
            private_key: candidate.private_key,
        });
    }
    maybe_stage_generated_profile_credentials(staging_dir, profiles, env_names, &mut copied)?;

    let mut present_env = Vec::new();
    let mut staged_env = HashSet::new();
    let env_dir = staging_dir.join("env");
    for profile in profiles {
        for &name in profile_env_names(profile) {
            if staged_env.contains(name) {
                continue;
            }
            let Ok(value) = env::var(name) else {
                continue;
            };
            write_env_value(staging_dir, &env_dir, name, &value)?;
            staged_env.insert(name.to_string());
            present_env.push(name.to_string());
        }
    }
    for name in env_names {
        if staged_env.contains(name) {
            continue;
        }
        let value = env::var(name).map_err(|_| {
            Error::InvalidCredential(format!(
                "environment variable {name} was requested but is not set"
            ))
        })?;
        write_env_value(staging_dir, &env_dir, name, &value)?;
        staged_env.insert(name.clone());
        present_env.push(name.clone());
    }

    let manifest_path = staging_dir.join("manifest.json");
    fs::write(&manifest_path, render_manifest(&copied, &present_env))
        .map_err(|err| Error::io_path(&manifest_path, err))?;
    restrict_file(&manifest_path)?;

    Ok(PreparedCredentials {
        staging_dir: staging_dir.to_path_buf(),
        manifest_path,
        copied,
        env_names: present_env,
    })
}

fn profile_env_names(profile: &CredentialProfile) -> &'static [&'static str] {
    match profile {
        CredentialProfile::Claude => CLAUDE_PORTABLE_ENV_NAMES,
        CredentialProfile::Codex => CODEX_PORTABLE_ENV_NAMES,
        CredentialProfile::Git => &[],
    }
}

fn maybe_stage_generated_profile_credentials(
    staging_dir: &Path,
    profiles: &[CredentialProfile],
    requested_env_names: &[String],
    copied: &mut Vec<CredentialCopy>,
) -> Result<()> {
    if !profiles.contains(&CredentialProfile::Claude) {
        return Ok(());
    }
    if copied.iter().any(|copy| {
        copy.staged_path
            == Path::new(CredentialProfile::Claude.name()).join(CLAUDE_CREDENTIALS_RELATIVE_PATH)
    }) {
        return Ok(());
    }
    if CLAUDE_PORTABLE_ENV_NAMES.iter().any(|&name| {
        env::var_os(name).is_some()
            || requested_env_names
                .iter()
                .any(|requested| requested == name)
    }) {
        return Ok(());
    }

    let Some(credentials) = read_macos_claude_keychain_credentials()? else {
        return Ok(());
    };
    let staged_relative =
        Path::new(CredentialProfile::Claude.name()).join(CLAUDE_CREDENTIALS_RELATIVE_PATH);
    let staged_path = staging_dir.join(&staged_relative);
    write_generated_credential(&staged_path, &credentials)?;
    copied.push(CredentialCopy {
        source_label: format!("macOS Keychain: {CLAUDE_KEYCHAIN_SERVICE}"),
        staged_path: staged_relative,
        guest_path: format!("/home/agent/{CLAUDE_CREDENTIALS_RELATIVE_PATH}"),
        private_key: false,
    });
    Ok(())
}

fn write_generated_credential(path: &Path, value: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| Error::io_path(parent, err))?;
        restrict_dir(parent)?;
    }
    fs::write(path, value).map_err(|err| Error::io_path(path, err))?;
    restrict_file(path)
}

#[cfg(target_os = "macos")]
fn read_macos_claude_keychain_credentials() -> Result<Option<Vec<u8>>> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"])
        .output()
        .map_err(Error::io)?;
    if !output.status.success() {
        return Ok(None);
    }

    let mut secret = output.stdout;
    while matches!(secret.last().copied(), Some(b'\n' | b'\r')) {
        secret.pop();
    }
    if secret.is_empty() || !looks_like_claude_credentials_json(&secret) {
        return Ok(None);
    }
    Ok(Some(secret))
}

#[cfg(not(target_os = "macos"))]
fn read_macos_claude_keychain_credentials() -> Result<Option<Vec<u8>>> {
    Ok(None)
}

fn looks_like_claude_credentials_json(value: &[u8]) -> bool {
    std::str::from_utf8(value)
        .map(|text| text.contains("\"claudeAiOauth\""))
        .unwrap_or(false)
}

fn write_env_value(staging_dir: &Path, env_dir: &Path, name: &str, value: &str) -> Result<()> {
    validate_env_name(name)?;
    if !env_dir.exists() {
        fs::create_dir_all(env_dir).map_err(|err| Error::io_path(env_dir, err))?;
        restrict_dir(env_dir)?;
    }
    let path = env_dir.join(name);
    fs::write(&path, value).map_err(|err| Error::io_path(&path, err))?;
    restrict_file(&path)?;
    restrict_dir(staging_dir)?;
    Ok(())
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(Error::InvalidCredential(
            "environment variable name must not be empty".to_string(),
        ));
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
    {
        return Err(Error::InvalidCredential(format!(
            "invalid environment variable name {name}"
        )));
    }
    Ok(())
}

fn profile_candidates(profile: &CredentialProfile) -> Vec<Candidate> {
    match profile {
        CredentialProfile::Claude => vec![
            optional(profile, "~/.claude.json", ".claude.json", false),
            optional(
                profile,
                "~/.claude/settings.json",
                ".claude/settings.json",
                false,
            ),
            optional(
                profile,
                "~/.claude/.credentials.json",
                ".claude/.credentials.json",
                false,
            ),
            optional(
                profile,
                "~/.config/claude/config.json",
                ".config/claude/config.json",
                false,
            ),
            optional(
                profile,
                "~/.config/claude/settings.json",
                ".config/claude/settings.json",
                false,
            ),
        ],
        CredentialProfile::Codex => vec![
            optional(profile, "~/.codex/auth.json", ".codex/auth.json", false),
            optional(profile, "~/.codex/config.toml", ".codex/config.toml", false),
            optional(
                profile,
                "~/.config/codex/auth.json",
                ".config/codex/auth.json",
                false,
            ),
            optional(
                profile,
                "~/.config/codex/config.toml",
                ".config/codex/config.toml",
                false,
            ),
        ],
        CredentialProfile::Git => vec![
            optional(profile, "~/.gitconfig", ".gitconfig", false),
            optional(profile, "~/.ssh/config", ".ssh/config", false),
            optional(profile, "~/.ssh/known_hosts", ".ssh/known_hosts", false),
            optional(profile, "~/.ssh/id_ed25519", ".ssh/id_ed25519", true),
            optional(profile, "~/.ssh/id_rsa", ".ssh/id_rsa", true),
        ],
    }
}

fn optional(
    profile: &CredentialProfile,
    host_path: &str,
    guest_relative_path: &str,
    private_key: bool,
) -> Candidate {
    Candidate {
        profile: profile.name().to_string(),
        host_path: expand_home(Path::new(host_path)),
        guest_relative_path: PathBuf::from(guest_relative_path),
        optional: true,
        private_key,
    }
}

fn expand_home(path: &Path) -> PathBuf {
    let value = path.as_os_str().to_string_lossy();
    if value == "~" {
        home_dir().unwrap_or_else(|| path.to_path_buf())
    } else if let Some(rest) = value.strip_prefix("~/") {
        home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn display_home(path: &Path) -> String {
    if let Some(home) = home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

fn validate_credential_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|err| Error::io_path(path, err))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = fs::canonicalize(path).map_err(|err| Error::io_path(path, err))?;
        validate_credential_path(&target)?;
        return Ok(());
    }
    if !(file_type.is_file() || file_type.is_dir()) {
        return Err(Error::InvalidCredential(format!(
            "{} is not a regular file or directory",
            path.display()
        )));
    }
    if is_world_writable(&metadata) {
        return Err(Error::InvalidCredential(format!(
            "{} is world-writable",
            path.display()
        )));
    }
    Ok(())
}

fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(src).map_err(|err| Error::io_path(src, err))?;
    if metadata.file_type().is_symlink() {
        let target = match fs::canonicalize(src) {
            Ok(target) => target,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(Error::io_path(src, err)),
        };
        return copy_path(&target, dst);
    }
    if metadata.is_dir() {
        fs::create_dir_all(dst).map_err(|err| Error::io_path(dst, err))?;
        restrict_dir(dst)?;
        for entry in fs::read_dir(src).map_err(|err| Error::io_path(src, err))? {
            let entry = entry.map_err(Error::io)?;
            let file_name = entry.file_name();
            copy_path(&entry.path(), &dst.join(file_name))?;
        }
        return Ok(());
    }
    if metadata.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|err| Error::io_path(parent, err))?;
            restrict_dir(parent)?;
        }
        fs::copy(src, dst).map_err(|err| Error::io_path(dst, err))?;
        restrict_file(dst)?;
        return Ok(());
    }
    Err(Error::InvalidCredential(format!(
        "{} is not a regular file or directory",
        src.display()
    )))
}

fn render_manifest(copied: &[CredentialCopy], env_names: &[String]) -> String {
    let mut out = String::from("{\n  \"version\": 1,\n  \"entries\": [\n");
    for (index, copy) in copied.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str("    {\n");
        out.push_str(&format!(
            "      \"source_label\": \"{}\",\n",
            escape_json(&copy.source_label)
        ));
        out.push_str(&format!(
            "      \"staged_path\": \"{}\",\n",
            escape_json(&copy.staged_path.display().to_string())
        ));
        out.push_str(&format!(
            "      \"guest_path\": \"{}\",\n",
            escape_json(&copy.guest_path)
        ));
        out.push_str("      \"mode\": \"copy\"\n");
        out.push_str("    }");
    }
    out.push_str("\n  ],\n  \"env\": {");
    for (index, name) in env_names.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!("\n    \"{}\": \"present\"", escape_json(name)));
    }
    if !env_names.is_empty() {
        out.push('\n');
    }
    out.push_str("  }\n}\n");
    out
}

pub(crate) fn escape_json(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn looks_like_private_key(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name == "id_rsa" || name == "id_ed25519" || name.ends_with(".pem")
}

#[cfg(unix)]
fn is_world_writable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o002 != 0
}

#[cfg(not(unix))]
fn is_world_writable(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|err| Error::io_path(path, err))
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|err| Error::io_path(path, err))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_json() {
        assert_eq!(escape_json("a\"b\\c"), "a\\\"b\\\\c");
    }

    #[cfg(unix)]
    #[test]
    fn skips_dangling_symlinks_inside_credential_dirs() {
        use std::os::unix::fs::symlink;

        let base = env::temp_dir().join(format!(
            "tb-sandbox-credentials-test-{}",
            std::process::id()
        ));
        let src = base.join("src");
        let dst = base.join("dst");

        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("settings.json"), "{}").unwrap();
        symlink(base.join("missing-debug-log.txt"), src.join("latest")).unwrap();

        copy_path(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("settings.json")).unwrap(), "{}");
        assert!(!dst.join("latest").exists());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn built_in_agent_profiles_copy_only_small_auth_files() {
        let claude = profile_candidates(&CredentialProfile::Claude);
        let codex = profile_candidates(&CredentialProfile::Codex);

        assert!(claude
            .iter()
            .any(|candidate| candidate.guest_relative_path == Path::new(".claude.json")));
        assert!(codex
            .iter()
            .any(|candidate| candidate.guest_relative_path == Path::new(".codex/auth.json")));
        assert!(!claude
            .iter()
            .any(|candidate| candidate.guest_relative_path == Path::new(".claude")));
        assert!(!codex
            .iter()
            .any(|candidate| candidate.guest_relative_path == Path::new(".codex")));
    }

    #[test]
    fn built_in_agent_profiles_include_portable_auth_env_names() {
        assert!(profile_env_names(&CredentialProfile::Claude).contains(&"CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(profile_env_names(&CredentialProfile::Claude).contains(&"ANTHROPIC_AUTH_TOKEN"));
        assert!(profile_env_names(&CredentialProfile::Claude).contains(&"ANTHROPIC_API_KEY"));
        assert!(profile_env_names(&CredentialProfile::Codex).contains(&"OPENAI_API_KEY"));
    }

    #[test]
    fn detects_claude_keychain_credentials_shape() {
        assert!(looks_like_claude_credentials_json(
            br#"{"claudeAiOauth":{"accessToken":"redacted"}}"#
        ));
        assert!(!looks_like_claude_credentials_json(
            br#"{"other":{"accessToken":"redacted"}}"#
        ));
    }
}
