use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AgentImageRef {
    pub path: PathBuf,
}

impl AgentImageRef {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceMount {
    pub tag: String,
    pub host_path: PathBuf,
    pub guest_path: String,
    pub read_only: bool,
}

impl WorkspaceMount {
    pub fn workspace(path: impl Into<PathBuf>) -> Self {
        Self {
            tag: "workspace".to_string(),
            host_path: path.into(),
            guest_path: "/workspace".to_string(),
            read_only: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialProfile {
    Claude,
    Codex,
    Git,
}

impl CredentialProfile {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "git" => Some(Self::Git),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Git => "git",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CredentialSpec {
    pub host_path: PathBuf,
    pub guest_relative_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub guest_user: String,
    pub workspace: String,
    pub shell: String,
    pub initial_command: Option<String>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            guest_user: "agent".to_string(),
            workspace: "/workspace".to_string(),
            shell: "/bin/bash".to_string(),
            initial_command: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetworkProxyConfig {
    pub enabled: bool,
    pub tls_mitm: bool,
    pub header_rules: Vec<ProxyHeaderRule>,
}

impl Default for NetworkProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tls_mitm: true,
            header_rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyHeaderRule {
    pub host: String,
    pub header: String,
    pub value: String,
    pub value_from_env: bool,
}

impl ProxyHeaderRule {
    pub fn encode(&self) -> String {
        if self.value_from_env {
            format!("{}:{}=@env:{}", self.host, self.header, self.value)
        } else {
            format!("{}:{}={}", self.host, self.header, self.value)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkBackend {
    None,
    Passt { socket: PathBuf },
    Gvproxy { socket: PathBuf },
    GvproxyAuto { executable: Option<PathBuf> },
}

impl NetworkBackend {
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn encode(&self) -> String {
        match self {
            Self::None => "none".to_string(),
            Self::Passt { socket } => format!("passt:{}", socket.display()),
            Self::Gvproxy { socket } => format!("gvproxy:{}", socket.display()),
            Self::GvproxyAuto { executable } => executable
                .as_ref()
                .map(|path| format!("gvproxy-auto:{}", path.display()))
                .unwrap_or_else(|| "gvproxy-auto".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    pub fn as_krun(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Error => 1,
            Self::Warn => 2,
            Self::Info => 3,
            Self::Debug => 4,
            Self::Trace => 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub name: Option<String>,
    pub image: AgentImageRef,
    pub cpus: u8,
    pub memory_mib: u32,
    /// Resize ceiling: the VM boots with this much RAM and the guest's usable
    /// memory can be grown up to it at runtime. When greater than `memory_mib`,
    /// the virtio-balloon starts inflated and runtime resizing is enabled.
    pub memory_max_mib: u32,
    /// Floor for runtime memory reclaim, in MiB.
    pub memory_min_mib: u32,
    pub workspace_mounts: Vec<WorkspaceMount>,
    pub credential_profiles: Vec<CredentialProfile>,
    pub extra_credentials: Vec<CredentialSpec>,
    pub credential_env: Vec<String>,
    pub shell: ShellConfig,
    pub network_proxy: NetworkProxyConfig,
    pub network_backend: NetworkBackend,
    pub ephemeral: bool,
    pub yes: bool,
    pub log_level: LogLevel,
}

impl SandboxConfig {
    pub fn new(image: AgentImageRef) -> Self {
        Self {
            name: None,
            image,
            cpus: 2,
            memory_mib: 2048,
            // Default ceiling == initial, i.e. runtime resizing disabled.
            memory_max_mib: 2048,
            memory_min_mib: 512,
            workspace_mounts: Vec::new(),
            credential_profiles: Vec::new(),
            extra_credentials: Vec::new(),
            credential_env: Vec::new(),
            shell: ShellConfig::default(),
            network_proxy: NetworkProxyConfig::default(),
            network_backend: NetworkBackend::None,
            ephemeral: false,
            yes: false,
            log_level: LogLevel::Error,
        }
    }

    /// Runtime memory resizing is enabled when the ceiling exceeds the initial
    /// memory, leaving headroom the balloon can grow into.
    pub fn resize_enabled(&self) -> bool {
        self.memory_max_mib > self.memory_mib
    }
}
