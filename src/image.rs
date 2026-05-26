use crate::error::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AgentImage {
    pub root: PathBuf,
    pub architecture: String,
    pub kernel: PathBuf,
    pub initramfs: Option<PathBuf>,
    pub root_disk: PathBuf,
    pub kernel_format: u32,
    pub kernel_cmdline: Option<String>,
    pub root_device: String,
    pub root_fstype: String,
    pub root_options: String,
    pub init: String,
    pub console: String,
    pub guest_user: String,
    pub installed_tools: Vec<String>,
}

impl AgentImage {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let root =
            fs::canonicalize(path.as_ref()).map_err(|err| Error::io_path(path.as_ref(), err))?;
        if !root.is_dir() {
            return Err(Error::InvalidImage(format!(
                "{} is not a directory",
                root.display()
            )));
        }

        let manifest_path = root.join("image.json");
        let manifest = match fs::read_to_string(&manifest_path) {
            Ok(contents) => Some(contents),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(Error::io_path(manifest_path, err)),
        };

        let kernel = resolve_file(
            &root,
            manifest
                .as_deref()
                .and_then(|json| json_string(json, "kernel"))
                .as_deref()
                .unwrap_or("kernel"),
            "kernel",
        )?;
        let root_disk = resolve_file(
            &root,
            manifest
                .as_deref()
                .and_then(|json| json_string(json, "root_disk"))
                .as_deref()
                .unwrap_or("root.raw"),
            "root disk",
        )?;
        let initramfs = manifest
            .as_deref()
            .and_then(|json| json_string(json, "initramfs"))
            .map(|value| resolve_optional_file(&root, &value, "initramfs"))
            .transpose()?
            .or_else(|| {
                let candidate = root.join("initramfs");
                candidate.is_file().then_some(candidate)
            });

        let host_arch = std::env::consts::ARCH;
        let default_arch = match host_arch {
            "aarch64" => "aarch64",
            "x86_64" => "x86_64",
            other => other,
        };

        let json = manifest.as_deref().unwrap_or("");
        let image = Self {
            root,
            architecture: json_string(json, "architecture")
                .unwrap_or_else(|| default_arch.to_string()),
            kernel,
            initramfs,
            root_disk,
            kernel_format: json_number(json, "kernel_format").unwrap_or(2) as u32,
            kernel_cmdline: json_string(json, "kernel_cmdline"),
            root_device: json_string(json, "root_device").unwrap_or_else(|| "/dev/vda".to_string()),
            root_fstype: json_string(json, "root_fstype").unwrap_or_else(|| "ext4".to_string()),
            root_options: json_string(json, "root_options").unwrap_or_else(|| "rw".to_string()),
            init: json_string(json, "init").unwrap_or_else(|| "/sbin/init".to_string()),
            console: json_string(json, "console").unwrap_or_else(|| {
                if host_arch == "aarch64" {
                    "ttyAMA0".to_string()
                } else {
                    "ttyS0".to_string()
                }
            }),
            guest_user: json_string(json, "guest_user").unwrap_or_else(|| "agent".to_string()),
            installed_tools: json_string_array(json, "installed_tools").unwrap_or_default(),
        };

        image.validate_host_arch()?;
        Ok(image)
    }

    fn validate_host_arch(&self) -> Result<()> {
        let host = std::env::consts::ARCH;
        if self.architecture == host {
            return Ok(());
        }
        if self.architecture == "aarch64" && host == "arm" {
            return Ok(());
        }
        Err(Error::InvalidImage(format!(
            "image architecture {} does not match host architecture {}",
            self.architecture, host
        )))
    }
}

fn resolve_file(root: &Path, value: &str, label: &str) -> Result<PathBuf> {
    let path = root.join(value);
    if path.is_file() {
        Ok(path)
    } else {
        Err(Error::InvalidImage(format!(
            "{label} {} does not exist or is not a file",
            path.display()
        )))
    }
}

fn resolve_optional_file(root: &Path, value: &str, label: &str) -> Result<PathBuf> {
    if value.is_empty() {
        return Err(Error::InvalidImage(format!("{label} path is empty")));
    }
    resolve_file(root, value, label)
}

pub(crate) fn json_string(json: &str, key: &str) -> Option<String> {
    let value = raw_json_value(json, key)?;
    parse_json_string(value).map(|(parsed, _)| parsed)
}

pub(crate) fn json_number(json: &str, key: &str) -> Option<u64> {
    let value = raw_json_value(json, key)?;
    let mut digits = String::new();
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            break;
        }
    }
    digits.parse().ok()
}

pub(crate) fn json_bool(json: &str, key: &str) -> Option<bool> {
    let value = raw_json_value(json, key)?;
    if value.starts_with("true") {
        Some(true)
    } else if value.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

pub(crate) fn json_string_array(json: &str, key: &str) -> Option<Vec<String>> {
    let mut value = raw_json_value(json, key)?.trim_start();
    if !value.starts_with('[') {
        return None;
    }
    value = &value[1..];
    let mut result = Vec::new();
    loop {
        value = value.trim_start();
        if value.starts_with(']') {
            return Some(result);
        }
        let (item, rest) = parse_json_string(value)?;
        result.push(item);
        value = rest.trim_start();
        if value.starts_with(',') {
            value = &value[1..];
        } else if value.starts_with(']') {
            return Some(result);
        } else {
            return None;
        }
    }
}

fn raw_json_value<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let index = json.find(&needle)?;
    let after_key = &json[index + needle.len()..];
    let colon = after_key.find(':')?;
    Some(after_key[colon + 1..].trim_start())
}

fn parse_json_string(input: &str) -> Option<(String, &str)> {
    let mut chars = input.chars();
    if chars.next()? != '"' {
        return None;
    }
    let mut parsed = String::new();
    let mut escaped = false;
    for (index, ch) in input[1..].char_indices() {
        if escaped {
            match ch {
                '"' => parsed.push('"'),
                '\\' => parsed.push('\\'),
                '/' => parsed.push('/'),
                'n' => parsed.push('\n'),
                'r' => parsed.push('\r'),
                't' => parsed.push('\t'),
                other => parsed.push(other),
            }
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some((parsed, &input[index + 2..])),
            other => parsed.push(other),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_json_values() {
        let json = r#"{"kernel":"kernel","kernel_format":2,"installed_tools":["claude","codex"]}"#;
        assert_eq!(json_string(json, "kernel").as_deref(), Some("kernel"));
        assert_eq!(json_number(json, "kernel_format"), Some(2));
        assert_eq!(
            json_string_array(json, "installed_tools").unwrap(),
            vec!["claude".to_string(), "codex".to_string()]
        );
    }
}
