//! Configuration and secret resolution.
//!
//! Precedence: built-in defaults < JSON config file < environment variables.
//! Secrets are NEVER stored in config: a provider only records the NAME of the environment
//! variable that carries its key, and the key is read at call time.

use crate::error::{Result, RuntimeError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreBackend {
    /// Process-local, lost on restart. Great for tests and for the desktop demo.
    Memory,
    /// Durable, dependency-free append-only log + snapshot store under data_dir.
    File,
    /// Embedded key-value store (redb) under data_dir.
    Redb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Deepseek,
    Openai,
    Qwen,
    Local,
    /// Deterministic in-process provider used by tests and by offline demos.
    #[default]
    Mock,
}

/// Can this model be shown an image, judging by its name?
///
/// A guess, and deliberately a conservative one: it recognises the families that document image
/// input and answers "unknown" for everything else - ignorance is not a "no", or a local server
/// running a vision model with an uninformative name would stop working. The guess exists because
/// the alternative was measured: deepseek-chat given an image_url answers HTTP 400, the router
/// failed over to the placeholder, and the user got a confident description of a picture that no
/// model ever saw.
/// A configuration value that an operator can override without editing the default: the model
/// name and the endpoint both change between deployments, and hard-coding either is how a runtime
/// ends up pinned to a model that cannot do what the user just asked for.
fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

pub fn vision_of_model(model: &str) -> &'static str {
    let name = model.to_ascii_lowercase();
    // Families that document image input.
    const VISION_MARKERS: &[&str] = &[
        "-vl", "vl-", "vision", "gpt-4o", "gpt-4.1", "gpt-5", "claude-3", "claude-4", "gemini",
        "llava", "pixtral", "internvl", "minicpm-v", "glm-4v", "yi-vl", "step-1v", "deepseek-flash",
    ];
    if VISION_MARKERS.iter().any(|marker| name.contains(marker)) {
        return "yes";
    }
    // Families that document text only. Being wrong here costs a refused image; being wrong the
    // other way costs a 400 and a fallback that pretends to have seen the picture.
    const TEXT_ONLY_MARKERS: &[&str] = &[
        "deepseek-chat", "deepseek-reasoner", "deepseek-coder", "gpt-3.5", "text-davinci",
        "qwen-plus", "qwen-turbo", "qwen-max", "llama-3", "mistral", "mixtral", "codellama",
    ];
    if TEXT_ONLY_MARKERS.iter().any(|marker| name.contains(marker)) {
        return "no";
    }
    "unknown"
}

/// Default gives every added field a value, so a struct literal that predates the field still
/// compiles and still means what it meant: name and kind are the only fields a caller must state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    pub model: String,
    pub base_url: String,
    /// Name of the environment variable holding the API key. Never the key itself.
    pub api_key_env: String,
    pub enabled: bool,
    /// Relative preference for the router; higher wins ties.
    pub priority: i32,
    pub timeout_ms: u64,
    /// Whether this provider's model can be shown an image.
    ///
    /// Unknown by default, and then the model name decides (see `vision_of_model` below): the
    /// name is the only honest evidence available offline, and guessing "yes" is how a picture ends
    /// up at a text-only model as an HTTP 400.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
}

impl ProviderConfig {
    /// Resolve the secret at call time. Returns None when unset or blank.
    pub fn api_key(&self) -> Option<String> {
        std::env::var(&self.api_key_env).ok().filter(|k| !k.trim().is_empty())
    }

    pub fn is_configured(&self) -> bool {
        match self.kind {
            // Local servers and the mock provider need no key.
            ProviderKind::Local | ProviderKind::Mock => true,
            _ => self.api_key().is_some(),
        }
    }
}

/// Every field falls back to its default when a configuration file omits it, so a file written
/// for an older version keeps working after a field is added. A wrong value is still a hard error;
/// an absent one is a default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    /// Provider used when the router has no better opinion.
    pub default_provider: String,
    pub providers: Vec<ProviderConfig>,
    pub request_timeout_ms: u64,
    pub max_retries: u32,
}

impl ModelConfig {
    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn enabled_providers(&self) -> impl Iterator<Item = &ProviderConfig> {
        self.providers.iter().filter(|p| p.enabled)
    }
}

/// Every field falls back to its default when a configuration file omits it, so a file written
/// for an older version keeps working after a field is added. A wrong value is still a hard error;
/// an absent one is a default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    pub name: String,
    pub node_id: Option<String>,
    pub region: String,
}

/// Every field falls back to its default when a configuration file omits it, so a file written
/// for an older version keeps working after a field is added. A wrong value is still a hard error;
/// an absent one is a default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub http_addr: String,
    pub ws_path: String,
    pub grpc_addr: String,
    /// Static bearer token for v1. When None, auth is disabled and a warning is logged.
    pub auth_token_env: String,
    pub rate_limit_per_minute: u32,
    pub cors_allow_origin: String,
    pub request_timeout_ms: u64,
}

impl ApiConfig {
    pub fn auth_token(&self) -> Option<String> {
        std::env::var(&self.auth_token_env).ok().filter(|t| !t.trim().is_empty())
    }
    pub fn auth_required(&self) -> bool {
        self.auth_token().is_some()
    }
}

/// Every field falls back to its default when a configuration file omits it, so a file written
/// for an older version keeps working after a field is added. A wrong value is still a hard error;
/// an absent one is a default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub backend: StoreBackend,
    pub data_dir: PathBuf,
    /// How many events to keep hot in the in-memory ring before compaction.
    pub event_log_retention: usize,
}

/// Every field falls back to its default when a configuration file omits it, so a file written
/// for an older version keeps working after a field is added. A wrong value is still a hard error;
/// an absent one is a default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    /// The only directory any filesystem capability may touch. Path traversal is denied.
    pub workspace_root: PathBuf,
    /// Empty means "every registered capability is allowed".
    pub allowed_capabilities: Vec<String>,
    pub denied_capabilities: Vec<String>,
    /// Capabilities that require an explicit approval decision.
    pub approval_required: Vec<String>,
    pub max_steps_per_run: u32,
    /// How many previous transcript messages may be replayed into a model request.
    /// Zero switches history off: every goal is then answered from scratch.
    pub history_messages: usize,
    /// Character budget for that history. The oldest turns are dropped first, and the newest turn
    /// is truncated rather than dropped, so the immediate context is never lost.
    pub history_chars: usize,
    /// How long a capability call that needs an operator decision waits before giving up.
    pub approval_timeout_ms: u64,
    /// How many undecided approvals may pile up. A bound here is what stops a wall of prompts.
    pub max_pending_approvals: usize,
    /// Summarise the turns that fell out of the history window, so they are dropped from the
    /// prompt rather than from memory. Costs one model call per compaction.
    pub compaction_enabled: bool,
    /// Do not spend a model call on fewer dropped turns than this.
    pub compaction_min_messages: usize,
    /// Workspace files whose contents are prepended to every prompt as project instructions
    /// (AGENTS.md and friends). Read through the workspace jail; an empty list disables this.
    pub context_files: Vec<String>,
    /// Total character budget for those files, spent in the order configured.
    pub context_files_chars: usize,
    /// How many stored memories may be recalled per run. Zero switches recall off.
    pub memory_recall_limit: usize,
    /// Character budget for the recalled text that is injected into the prompt.
    pub memory_recall_chars: usize,
    pub max_concurrent_tasks: usize,
    pub capability_timeout_ms: u64,
    pub capability_retries: u32,
    pub allow_network_capabilities: bool,
    pub max_artifact_bytes: u64,
}

/// Every field falls back to its default when a configuration file omits it, so a file written
/// for an older version keeps working after a field is added. A wrong value is still a hard error;
/// an absent one is a default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct P2pConfig {
    pub enabled: bool,
    pub listen: Vec<String>,
    pub bootstrap: Vec<String>,
    pub mdns: bool,
    /// P2P is deliberately off the hot path: only discovery and control gossip use it.
    pub advertise_interval_ms: u64,
}

/// Every field falls back to its default when a configuration file omits it, so a file written
/// for an older version keeps working after a field is added. A wrong value is still a hard error;
/// an absent one is a default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservabilityConfig {
    pub log_level: String,
    pub log_format: LogFormat,
    pub metrics_enabled: bool,
    /// Reserved: OTLP exporter is not wired in v1.
    pub otlp_endpoint: Option<String>,
}

/// Every field falls back to its default when a configuration file omits it, so a file written
/// for an older version keeps working after a field is added. A wrong value is still a hard error;
/// an absent one is a default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeLimits {
    pub session_queue_capacity: usize,
    pub default_task_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
    /// Ticks per second used by the wasm epoch watchdog.
    pub epoch_tick_ms: u64,
    pub wasm_timeout_ms: u64,
    pub wasm_memory_limit_bytes: usize,
    pub wasm_max_instances: usize,
    pub actor_snapshot_interval_ms: u64,
    pub heartbeat_interval_ms: u64,
    /// A worker with no heartbeat for this long is considered lost.
    pub worker_lease_ms: u64,
}

/// How this node makes itself visible to other nodes, and finds them.
///
/// The default backend is a user-scoped directory on the local filesystem: every node writes a
/// small JSON advertisement there and reads everyone else's. It needs no network, no multicast and
/// no configuration, which is what makes "start a second workspace and see it immediately" work.
/// A libp2p/mDNS backend exists for cross-machine discovery and is selected by the composition
/// root, not by this configuration.
/// Every field falls back to its default when a configuration file omits it, so a file written
/// for an older version keeps working after a field is added. A wrong value is still a hard error;
/// an absent one is a default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    pub enabled: bool,
    /// Shared directory. Defaults to a per-user location; override with AGENTOS_DISCOVERY_DIR.
    pub dir: PathBuf,
    /// An advertisement older than this is considered gone.
    pub ttl_ms: u64,
    /// Write our own advertisement (false = discover others without being discoverable).
    pub advertise: bool,
}

/// Per-user default location for the node advertisements.
///
/// Deliberately outside the repository: two checkouts of the same project must still see each
/// other, and a repository is a per-instance working directory, not a machine-wide namespace.
pub fn default_discovery_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(base) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(base).join("agora-agent-os").join("nodes");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("agora-agent-os")
                .join("nodes");
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime).join("agora-agent-os").join("nodes");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("agora-agent-os")
                .join("nodes");
        }
    }
    std::env::temp_dir().join("agora-agent-os").join("nodes")
}

/// A configuration document may be partial: every section and every field falls back to its
/// default when absent. That is what makes a config file survive a runtime upgrade - adding a
/// field must not stop a deployment from starting - while a field that is present and wrong is
/// still refused outright.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub node: NodeConfig,
    pub api: ApiConfig,
    pub storage: StorageConfig,
    pub models: ModelConfig,
    pub policy: PolicyConfig,
    pub p2p: P2pConfig,
    pub observability: ObservabilityConfig,
    pub discovery: DiscoveryConfig,
    pub limits: RuntimeLimits,
    pub mcp: McpConfig,
    /// Values the environment supplied that could not be honoured.
    ///
    /// Environment overrides are applied before the log subscriber exists, so a warning emitted
    /// there goes nowhere. Collecting them means a typo in an override is reported by the runtime
    /// instead of being silently ignored - which is the failure mode this field exists to stop.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

/// External tool servers speaking the Model Context Protocol.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One MCP server. It runs as a child process of this runtime, so the command and its arguments
/// come from configuration an operator wrote, never from a model or a user message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    /// Short name used in capability names ("mcp.<name>.<tool>") and in policy entries.
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self { name: "agentos-local".into(), node_id: None, region: "local".into() }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            http_addr: "127.0.0.1:8788".into(),
            ws_path: "/v1/ws".into(),
            grpc_addr: "127.0.0.1:8789".into(),
            auth_token_env: "AGENTOS_AUTH_TOKEN".into(),
            rate_limit_per_minute: 600,
            cors_allow_origin: "*".into(),
            request_timeout_ms: 30_000,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: StoreBackend::File,
            data_dir: PathBuf::from("./data"),
            event_log_retention: 20_000,
        }
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            default_provider: "mock".into(),
            providers: vec![
                ProviderConfig {
                    name: "mock".into(),
                    kind: ProviderKind::Mock,
                    model: "agentos-mock-1".into(),
                    base_url: "inproc://mock".into(),
                    api_key_env: "AGENTOS_MOCK_API_KEY".into(),
                    enabled: true,
                    priority: 0,
                    timeout_ms: 5_000,
                    // The placeholder has no eyes; it says so rather than pretending.
                    vision: Some(false),
                },
                ProviderConfig {
                    name: "deepseek".into(),
                    kind: ProviderKind::Deepseek,
                    // The vision-capable model: deepseek-chat is text-only and answers HTTP 400 to
                    // an image_url, which then looked like "the picture was answered" because the
                    // router failed over. Set DEEPSEEK_MODEL to pin a deployment to another one.
                    model: env_or("DEEPSEEK_MODEL", "deepseek-flash"),
                    base_url: env_or("DEEPSEEK_BASE_URL", "https://api.deepseek.com"),
                    api_key_env: "DEEPSEEK_API_KEY".into(),
                    enabled: true,
                    priority: 30,
                    timeout_ms: 60_000,
                    // Documented as accepting images, so a screenshot is not refused here.
                    vision: Some(true),
                },
                ProviderConfig {
                    name: "openai".into(),
                    kind: ProviderKind::Openai,
                    // gpt-4o-mini is the text model of the 4o family; the full gpt-4o sees images.
                    model: env_or("OPENAI_MODEL", "gpt-4o-mini"),
                    base_url: env_or("OPENAI_BASE_URL", "https://api.openai.com"),
                    api_key_env: "OPENAI_API_KEY".into(),
                    enabled: true,
                    priority: 20,
                    timeout_ms: 60_000,
                    vision: None,
                },
                ProviderConfig {
                    name: "qwen".into(),
                    kind: ProviderKind::Qwen,
                    model: env_or("QWEN_MODEL", "qwen-plus"),
                    base_url: env_or("QWEN_BASE_URL", "https://dashscope.aliyuncs.com/compatible-mode"),
                    api_key_env: "DASHSCOPE_API_KEY".into(),
                    enabled: true,
                    priority: 15,
                    timeout_ms: 60_000,
                    vision: None,
                },
                ProviderConfig {
                    name: "local".into(),
                    kind: ProviderKind::Local,
                    model: env_or("AGENTOS_LOCAL_MODEL", "local-llm"),
                    base_url: env_or("AGENTOS_LOCAL_BASE_URL", "http://127.0.0.1:11434"),
                    api_key_env: "AGENTOS_LOCAL_API_KEY".into(),
                    enabled: true,
                    priority: 5,
                    timeout_ms: 120_000,
                    // A local server is whatever somebody is running there; the name decides.
                    vision: None,
                },
            ],
            request_timeout_ms: 60_000,
            max_retries: 2,
        }
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("./workspace"),
            allowed_capabilities: vec![],
            denied_capabilities: vec![],
            approval_required: vec![],
            max_steps_per_run: 12,
            history_messages: 20,
            history_chars: 8_000,
            approval_timeout_ms: 120_000,
            max_pending_approvals: 64,
            compaction_enabled: true,
            compaction_min_messages: 4,
            context_files: vec!["AGENTS.md".into()],
            context_files_chars: 8_000,
            memory_recall_limit: 5,
            memory_recall_chars: 1_200,
            max_concurrent_tasks: 16,
            capability_timeout_ms: 10_000,
            capability_retries: 2,
            allow_network_capabilities: false,
            max_artifact_bytes: 8 * 1024 * 1024,
        }
    }
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: vec!["/ip4/0.0.0.0/tcp/0".into()],
            bootstrap: vec![],
            mdns: true,
            advertise_interval_ms: 15_000,
        }
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            log_level: "info,agentos=debug".into(),
            log_format: LogFormat::Text,
            metrics_enabled: true,
            otlp_endpoint: None,
        }
    }
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self { enabled: true, dir: default_discovery_dir(), ttl_ms: 10_000, advertise: true }
    }
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            session_queue_capacity: 1024,
            default_task_timeout_ms: 60_000,
            shutdown_timeout_ms: 5_000,
            epoch_tick_ms: 50,
            wasm_timeout_ms: 2_000,
            wasm_memory_limit_bytes: 32 * 1024 * 1024,
            wasm_max_instances: 16,
            actor_snapshot_interval_ms: 30_000,
            heartbeat_interval_ms: 2_000,
            worker_lease_ms: 10_000,
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            node: NodeConfig::default(),
            api: ApiConfig::default(),
            storage: StorageConfig::default(),
            models: ModelConfig::default(),
            policy: PolicyConfig::default(),
            p2p: P2pConfig::default(),
            observability: ObservabilityConfig::default(),
            // On by default: "start a node, see it from the console" is the point of the feature.
            // It only writes a small file into a per-user directory, and AGENTOS_DISCOVERY=off
            // turns it off completely.
            discovery: DiscoveryConfig::default(),
            limits: RuntimeLimits::default(),
            mcp: McpConfig::default(),
            warnings: Vec::new(),
        }
    }
}


impl RuntimeConfig {
    /// Load defaults, then a JSON file (if present), then environment overrides.
    pub fn load() -> Result<Self> {
        let path = std::env::var("AGENTOS_CONFIG").ok().map(PathBuf::from);
        let mut cfg = match &path {
            Some(p) if p.exists() => {
                let raw = std::fs::read_to_string(p).map_err(|e| {
                    RuntimeError::invalid_input(format!("cannot read config {}: {e}", p.display()))
                })?;
                serde_json::from_str(&raw).map_err(|e| {
                    RuntimeError::invalid_input(format!("invalid config {}: {e}", p.display()))
                })?
            }
            _ => Self::default(),
        };
        cfg.apply_env();
        cfg.validate()?;
        Ok(cfg)
    }

    fn env_str(key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }

    /// Environment overrides. Every override is explicit so the effective configuration is
    /// reproducible from the process environment alone.
    pub fn apply_env(&mut self) {
        if let Some(v) = Self::env_str("AGENTOS_NODE_NAME") { self.node.name = v; }
        if let Some(v) = Self::env_str("AGENTOS_NODE_ID") { self.node.node_id = Some(v); }
        if let Some(v) = Self::env_str("AGENTOS_HTTP_ADDR") { self.api.http_addr = v; }
        if let Some(v) = Self::env_str("AGENTOS_GRPC_ADDR") { self.api.grpc_addr = v; }
        if let Some(v) = Self::env_str("AGENTOS_WS_PATH") { self.api.ws_path = v; }
        if let Some(v) = Self::env_str("AGENTOS_CORS_ORIGIN") { self.api.cors_allow_origin = v; }
        if let Some(v) = Self::env_str("AGENTOS_RATE_LIMIT") {
            if let Ok(n) = v.parse() { self.api.rate_limit_per_minute = n; }
        }
        if let Some(v) = Self::env_str("AGENTOS_DATA_DIR") { self.storage.data_dir = PathBuf::from(v); }
        if let Some(v) = Self::env_str("AGENTOS_STORE_BACKEND") {
            self.storage.backend = match v.to_ascii_lowercase().as_str() {
                "memory" => StoreBackend::Memory,
                "file" => StoreBackend::File,
                "redb" => StoreBackend::Redb,
                other => {
                    tracing::warn!(backend = other, "unknown store backend, keeping current value");
                    self.storage.backend
                }
            };
        }
        if let Some(v) = Self::env_str("AGENTOS_WORKSPACE_ROOT") { self.policy.workspace_root = PathBuf::from(v); }
        if let Some(v) = Self::env_str("AGENTOS_MAX_STEPS") {
            if let Ok(n) = v.parse() { self.policy.max_steps_per_run = n; }
        }
        // These were once nested inside the AGENTOS_MAX_STEPS block above, which meant the history
        // and recall switches only worked for deployments that also set a step budget. Keep them
        // at the top level, and see the test below that pins each switch.
        if let Some(v) = Self::env_str("AGENTOS_HISTORY_MESSAGES") {
            match v.parse::<usize>() {
                Ok(n) => self.policy.history_messages = n,
                Err(_) => self.warnings.push(format!("AGENTOS_HISTORY_MESSAGES={v} is not a number; ignored")),
            }
        }
        if let Some(v) = Self::env_str("AGENTOS_HISTORY_CHARS") {
            match v.parse::<usize>() {
                Ok(n) => self.policy.history_chars = n,
                Err(_) => self.warnings.push(format!("AGENTOS_HISTORY_CHARS={v} is not a number; ignored")),
            }
        }
        if let Some(v) = Self::env_str("AGENTOS_MEMORY_RECALL_LIMIT") {
            match v.parse::<usize>() {
                Ok(n) => self.policy.memory_recall_limit = n,
                Err(_) => self.warnings.push(format!("AGENTOS_MEMORY_RECALL_LIMIT={v} is not a number; ignored")),
            }
        }
        if let Some(v) = Self::env_str("AGENTOS_MEMORY_RECALL_CHARS") {
            match v.parse::<usize>() {
                Ok(n) => self.policy.memory_recall_chars = n,
                Err(_) => self.warnings.push(format!("AGENTOS_MEMORY_RECALL_CHARS={v} is not a number; ignored")),
            }
        }
        if let Some(v) = Self::env_str("AGENTOS_CONTEXT_FILES") {
            // Comma separated, so the common case of one extra file needs no JSON edit.
            self.policy.context_files = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(v) = Self::env_str("AGENTOS_CONTEXT_FILES_CHARS") {
            match v.parse::<usize>() {
                Ok(n) => self.policy.context_files_chars = n,
                Err(_) => self.warnings.push(format!("AGENTOS_CONTEXT_FILES_CHARS={v} is not a number; ignored")),
            }
        }
        if let Some(v) = Self::env_str("AGENTOS_MCP_SERVERS") {
            // A JSON array, because a list of commands with arguments is not expressible as a comma
            // separated string without inventing an escaping language nobody asked for.
            match serde_json::from_str::<Vec<McpServerConfig>>(&v) {
                Ok(servers) => self.mcp.servers = servers,
                Err(error) => self
                    .warnings
                    .push(format!("AGENTOS_MCP_SERVERS is not a JSON array of servers ({error}); ignored")),
            }
        }
        if let Some(v) = Self::env_str("AGENTOS_COMPACTION") {
            self.policy.compaction_enabled = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = Self::env_str("AGENTOS_COMPACTION_MIN_MESSAGES") {
            match v.parse::<usize>() {
                Ok(n) => self.policy.compaction_min_messages = n,
                Err(_) => self.warnings.push(format!("AGENTOS_COMPACTION_MIN_MESSAGES={v} is not a number; ignored")),
            }
        }
        if let Some(v) = Self::env_str("AGENTOS_MAX_CONCURRENT_TASKS") {
            if let Ok(n) = v.parse() { self.policy.max_concurrent_tasks = n; }
        }
        if let Some(v) = Self::env_str("AGENTOS_ALLOWED_CAPABILITIES") {
            self.policy.allowed_capabilities = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        if let Some(v) = Self::env_str("AGENTOS_DENIED_CAPABILITIES") {
            self.policy.denied_capabilities = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        if let Some(v) = Self::env_str("AGENTOS_MODEL_DEFAULT") { self.models.default_provider = v; }
        if let Some(v) = Self::env_str("AGENTOS_LOG") { self.observability.log_level = v; }
        if let Some(v) = Self::env_str("AGENTOS_LOG_FORMAT") {
            self.observability.log_format = if v.eq_ignore_ascii_case("json") { LogFormat::Json } else { LogFormat::Text };
        }
        if let Some(v) = Self::env_str("AGENTOS_DISCOVERY") {
            self.discovery.enabled = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = Self::env_str("AGENTOS_DISCOVERY_DIR") {
            self.discovery.dir = PathBuf::from(v);
        }
        if let Some(v) = Self::env_str("AGENTOS_DISCOVERY_TTL_MS") {
            if let Ok(n) = v.parse() {
                self.discovery.ttl_ms = n;
            }
        }
        if let Some(v) = Self::env_str("AGENTOS_DISCOVERY_ADVERTISE") {
            self.discovery.advertise = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = Self::env_str("AGENTOS_P2P_ENABLED") {
            self.p2p.enabled = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = Self::env_str("AGENTOS_P2P_LISTEN") { self.p2p.listen = vec![v]; }
        if let Some(v) = Self::env_str("AGENTOS_P2P_BOOTSTRAP") {
            self.p2p.bootstrap = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        // Per-provider model / base-url overrides: AGENTOS_MODEL_<NAME>_MODEL, _BASE_URL.
        let names: Vec<String> = self.models.providers.iter().map(|p| p.name.clone()).collect();
        for name in names {
            let upper = name.to_ascii_uppercase().replace('-', "_");
            let model_key = format!("AGENTOS_MODEL_{upper}_MODEL");
            let url_key = format!("AGENTOS_MODEL_{upper}_BASE_URL");
            let enabled_key = format!("AGENTOS_MODEL_{upper}_ENABLED");
            if let Some(v) = Self::env_str(&model_key) {
                if let Some(p) = self.models.providers.iter_mut().find(|p| p.name == name) { p.model = v; }
            }
            if let Some(v) = Self::env_str(&url_key) {
                if let Some(p) = self.models.providers.iter_mut().find(|p| p.name == name) { p.base_url = v; }
            }
            if let Some(v) = Self::env_str(&enabled_key) {
                let on = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
                if let Some(p) = self.models.providers.iter_mut().find(|p| p.name == name) { p.enabled = on; }
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.api.http_addr.trim().is_empty() {
            return Err(RuntimeError::invalid_input("api.http_addr must not be empty"));
        }
        // Two listeners on one address is a configuration error that used to surface as "the
        // process is up but the gateway is dead": the gRPC server won the bind and the gateway
        // failed silently. Catch it where it can still be explained.
        let http = self.api.http_addr.parse::<std::net::SocketAddr>().map_err(|e| {
            RuntimeError::invalid_input(format!("api.http_addr {:?} is not host:port: {e}", self.api.http_addr))
        })?;
        let grpc = self.api.grpc_addr.parse::<std::net::SocketAddr>().map_err(|e| {
            RuntimeError::invalid_input(format!("api.grpc_addr {:?} is not host:port: {e}", self.api.grpc_addr))
        })?;
        if http == grpc {
            return Err(RuntimeError::invalid_input(format!(
                "api.http_addr and api.grpc_addr must differ (both are {http})"
            )));
        }
        if self.limits.session_queue_capacity == 0 {
            return Err(RuntimeError::invalid_input("limits.session_queue_capacity must be > 0"));
        }
        let mut seen_names: Vec<&str> = Vec::new();
        for server in &self.mcp.servers {
            if server.name.trim().is_empty() {
                return Err(RuntimeError::invalid_input("mcp.servers[].name must not be empty"));
            }
            if server.command.trim().is_empty() {
                return Err(RuntimeError::invalid_input(format!(
                    "mcp server {} has no command",
                    server.name
                )));
            }
            // The name is part of every capability name it publishes, so duplicates would silently
            // shadow each other in the registry.
            if seen_names.contains(&server.name.as_str()) {
                return Err(RuntimeError::invalid_input(format!(
                    "mcp server {} is configured twice",
                    server.name
                )));
            }
            seen_names.push(server.name.as_str());
        }
        if !self.policy.approval_required.is_empty() && self.policy.approval_timeout_ms == 0 {
            return Err(RuntimeError::invalid_input(
                "policy.approval_timeout_ms must be > 0: an approval with no deadline is a hang",
            ));
        }
        if self.policy.max_pending_approvals == 0 {
            return Err(RuntimeError::invalid_input(
                "policy.max_pending_approvals must be > 0",
            ));
        }
        if self.policy.compaction_enabled && self.policy.history_messages == 0 {
            return Err(RuntimeError::invalid_input(
                "policy.compaction_enabled needs history_messages > 0: without a window there is nothing to compact",
            ));
        }
        if !self.policy.context_files.is_empty() && self.policy.context_files_chars == 0 {
            return Err(RuntimeError::invalid_input(
                "policy.context_files_chars must be > 0 when context_files is not empty",
            ));
        }
        if self.policy.memory_recall_limit > 0 && self.policy.memory_recall_chars == 0 {
            return Err(RuntimeError::invalid_input(
                "policy.memory_recall_chars must be > 0 when memory_recall_limit > 0",
            ));
        }
        if self.policy.history_messages > 0 && self.policy.history_chars == 0 {
            return Err(RuntimeError::invalid_input(
                "policy.history_chars must be > 0 when history_messages > 0",
            ));
        }
        if self.policy.max_steps_per_run == 0 {
            return Err(RuntimeError::invalid_input("policy.max_steps_per_run must be > 0"));
        }
        if self.models.default_provider.trim().is_empty() {
            return Err(RuntimeError::invalid_input("models.default_provider must not be empty"));
        }
        if self.models.provider(&self.models.default_provider).is_none() {
            return Err(RuntimeError::invalid_input(format!(
                "models.default_provider {} is not a declared provider",
                self.models.default_provider
            )));
        }
        Ok(())
    }

    /// The identity this node uses in events, the actor directory and worker records.
    /// An explicit AGENTOS_NODE_ID wins so identity survives a rename; otherwise the name is used,
    /// which keeps a single-instance setup zero-config.
    /// Resolve this node's identity once, at bootstrap.
    ///
    /// An explicit AGENTOS_NODE_ID always wins. Otherwise the identity is generated on first start
    /// and persisted next to the state, which makes it unique per instance and stable across
    /// restarts.
    ///
    /// Falling back to the node *name* - as an earlier version did - was a bug: every node defaults
    /// to the same name, so two checkouts advertised themselves under one identity, and because a
    /// node skips the advertisement carrying its own id, neither could see the other.
    pub fn resolve_node_identity(&mut self) -> Result<String> {
        if let Some(id) = self.node.node_id.clone().filter(|id| !id.trim().is_empty()) {
            return Ok(id);
        }
        let path = self.storage.data_dir.join("node.id");
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let existing = existing.trim();
            if !existing.is_empty() {
                self.node.node_id = Some(existing.to_string());
                return Ok(existing.to_string());
            }
        }
        std::fs::create_dir_all(&self.storage.data_dir)?;
        let generated = crate::ids::NodeId::new().to_string();
        std::fs::write(&path, format!("{generated}\n"))?;
        self.node.node_id = Some(generated.clone());
        Ok(generated)
    }

    pub fn effective_node_id(&self) -> String {
        self.node
            .node_id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| self.node.name.clone())
    }

    /// Ensure directories exist; called at bootstrap, not in constructors.
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.storage.data_dir)?;
        std::fs::create_dir_all(self.policy.workspace_root.clone())?;
        Ok(())
    }

    /// Provider summary safe to expose over HTTP / to the desktop UI: no secrets, ever.
    pub fn model_summary(&self) -> Vec<BTreeMap<String, serde_json::Value>> {
        self.models
            .providers
            .iter()
            .map(|p| {
                let mut m = BTreeMap::new();
                m.insert("name".into(), serde_json::json!(p.name));
                m.insert("kind".into(), serde_json::json!(p.kind));
                m.insert("model".into(), serde_json::json!(p.model));
                m.insert("enabled".into(), serde_json::json!(p.enabled));
                m.insert("key_env".into(), serde_json::json!(p.api_key_env));
                m.insert("configured".into(), serde_json::json!(p.is_configured()));
                // What this model is likely to accept, so a client can tell a user why an image
                // was refused before they attach one. Configured wins over the model name.
                m.insert(
                    "vision".into(),
                    serde_json::json!(match p.vision {
                        Some(true) => "yes",
                        Some(false) => "no",
                        None => vision_of_model(&p.model),
                    }),
                );
                m
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment variables are process-global, so any test that sets them has to own the process
    /// for the duration. Without this the cases below race each other and fail depending on order.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The guess the router routes on. Getting these wrong in either direction is a real failure:
    /// a "yes" that is wrong sends an image to a model that answers 400 and then to a fallback that
    /// describes nothing, and a "no" that is wrong refuses an image a model could have read.
    #[test]
    fn the_model_name_guess_knows_the_families_that_matter_here() {
        assert_eq!(vision_of_model("deepseek-flash"), "yes");
        assert_eq!(vision_of_model("qwen-vl-max"), "yes");
        assert_eq!(vision_of_model("gpt-4o"), "yes");
        assert_eq!(vision_of_model("gpt-4o-mini"), "yes", "the 4o family takes images at every size");
        assert_eq!(vision_of_model("llava:13b"), "yes");
        assert_eq!(vision_of_model("deepseek-chat"), "no", "the measured 400");
        assert_eq!(vision_of_model("deepseek-reasoner"), "no");
        assert_eq!(vision_of_model("qwen-plus"), "no");
        // Unknown stays unknown: a local model with a name nobody recognises is not refused.
        assert_eq!(vision_of_model("local-llm"), "unknown");
        assert_eq!(vision_of_model("my-finetune-v3"), "unknown");
    }

    #[test]
    fn the_default_deepseek_model_can_be_shown_an_image() {
        // The default used to be deepseek-chat, which answers HTTP 400 to an image_url. The
        // operator can pin another model, but the default has to work.
        let models = ModelConfig::default();
        let deepseek = models.provider("deepseek").expect("the default config has a deepseek provider");
        assert_eq!(
            vision_of_model(&deepseek.model),
            "yes",
            "the shipped default model must accept images: {}",
            deepseek.model
        );
        let mock = models.provider("mock").expect("the placeholder is registered");
        assert_eq!(mock.vision, Some(false), "the placeholder has no eyes and says so");
    }

    #[test]
    fn the_shipped_example_config_is_valid() {
        // A template that does not load is worse than no template: an operator copies it, the
        // runtime refuses to start, and the error names a field they never touched.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/agora-agent-os.example.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let config: RuntimeConfig = serde_json::from_str(&raw)
            .unwrap_or_else(|error| panic!("the example config does not parse: {error}"));
        config.validate().expect("the example config must validate");
    }

    #[test]
    fn a_configuration_file_from_an_older_version_still_loads() {
        // Fields added later fall back to their defaults, so upgrading the runtime does not
        // invalidate the configuration a deployment already has. A wrong value is still an error.
        let partial: RuntimeConfig = serde_json::from_str(r#"{ "api": { "http_addr": "127.0.0.1:9999" } }"#)
            .expect("a partial config must load");
        assert_eq!(partial.api.http_addr, "127.0.0.1:9999");
        assert_eq!(partial.api.ws_path, ApiConfig::default().ws_path, "missing fields default");
        assert_eq!(partial.policy.max_steps_per_run, PolicyConfig::default().max_steps_per_run);
        partial.validate().expect("and it is a usable configuration");

        let wrong = serde_json::from_str::<RuntimeConfig>(r#"{ "policy": { "max_steps_per_run": "many" } }"#);
        assert!(wrong.is_err(), "a wrong type is still refused, not defaulted away");
    }

    #[test]
    fn defaults_validate() {
        RuntimeConfig::default().validate().unwrap();
    }

    #[test]
    fn secrets_never_live_in_config() {
        let cfg = RuntimeConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("DEEPSEEK_API_KEY"), "we store the env var NAME");
        assert!(!json.contains("sk-"), "and never a literal key");
        let p = cfg.models.provider("deepseek").unwrap();
        assert_eq!(p.api_key_env, "DEEPSEEK_API_KEY");
    }

    #[test]
    fn mock_provider_needs_no_key() {
        let cfg = RuntimeConfig::default();
        assert!(cfg.models.provider("mock").unwrap().is_configured());
    }

    #[test]
    fn unknown_default_provider_is_rejected() {
        let mut cfg = RuntimeConfig::default();
        cfg.models.default_provider = "nope".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn an_explicit_node_id_wins() {
        let mut cfg = RuntimeConfig::default();
        cfg.node.node_id = Some("node-a-01".into());
        assert_eq!(cfg.resolve_node_identity().unwrap(), "node-a-01");
        assert_eq!(cfg.effective_node_id(), "node-a-01");
    }

    #[test]
    fn generated_identities_are_unique_per_instance_and_stable_across_restarts() {
        let dir = std::env::temp_dir().join(format!("agora-nodeid-{}", crate::now_ms()));
        let mut first = RuntimeConfig::default();
        first.storage.data_dir = dir.join("a");
        let mut second = RuntimeConfig::default();
        second.storage.data_dir = dir.join("b");

        let id_a = first.resolve_node_identity().unwrap();
        let id_b = second.resolve_node_identity().unwrap();
        assert_ne!(id_a, id_b, "two instances must not share an identity");
        assert!(!id_a.is_empty());
        assert!(dir.join("a").join("node.id").exists(), "identity is persisted with the state");

        // Restarting the same data directory keeps the identity.
        let mut restarted = RuntimeConfig::default();
        restarted.storage.data_dir = dir.join("a");
        assert_eq!(restarted.resolve_node_identity().unwrap(), id_a);

        // Two nodes with the *same name* still have different identities: the name is a label.
        assert_eq!(first.node.name, second.node.name);
        assert_ne!(first.effective_node_id(), second.effective_node_id());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Environment switches must work on their own, without another variable being set first.
    ///
    /// A previous nesting bug put four of these inside the AGENTOS_MAX_STEPS block, so they were
    /// silently ignored - exactly the kind of failure a test has to catch, because the feature
    /// simply looks switched off.
    #[test]
    fn an_override_that_cannot_be_honoured_is_reported() {
        let _guard = env_guard();
        // These are set before any log subscriber exists, so the only place they can be seen
        // later is the warning list. A silent typo in a deployment is worse than a noisy one.
        for key in [
            "AGENTOS_MCP_SERVERS",
            "AGENTOS_HISTORY_MESSAGES",
            "AGENTOS_MEMORY_RECALL_CHARS",
            "AGENTOS_COMPACTION_MIN_MESSAGES",
        ] {
            std::env::set_var(key, "definitely-not-what-this-wants");
        }
        let mut cfg = RuntimeConfig::default();
        cfg.apply_env();
        for key in [
            "AGENTOS_MCP_SERVERS",
            "AGENTOS_HISTORY_MESSAGES",
            "AGENTOS_MEMORY_RECALL_CHARS",
            "AGENTOS_COMPACTION_MIN_MESSAGES",
        ] {
            std::env::remove_var(key);
            assert!(
                cfg.warnings.iter().any(|warning| warning.contains(key)),
                "{key} must be reported, got {:?}",
                cfg.warnings
            );
        }
        // And nothing was silently half-applied.
        assert!(cfg.mcp.servers.is_empty());
    }

    #[test]
    fn context_switches_apply_from_the_environment_alone() {
        let _guard = env_guard();
        const KEYS: [&str; 6] = [
            "AGENTOS_HISTORY_MESSAGES",
            "AGENTOS_HISTORY_CHARS",
            "AGENTOS_MEMORY_RECALL_LIMIT",
            "AGENTOS_MEMORY_RECALL_CHARS",
            "AGENTOS_CONTEXT_FILES",
            "AGENTOS_CONTEXT_FILES_CHARS",
        ];
        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for key in KEYS {
                    std::env::remove_var(key);
                }
            }
        }
        let _cleanup = Cleanup;
        std::env::set_var("AGENTOS_HISTORY_MESSAGES", "7");
        std::env::set_var("AGENTOS_HISTORY_CHARS", "1234");
        std::env::set_var("AGENTOS_MEMORY_RECALL_LIMIT", "3");
        std::env::set_var("AGENTOS_MEMORY_RECALL_CHARS", "777");
        std::env::set_var("AGENTOS_CONTEXT_FILES", "AGENTS.md, NOTES.md ,");
        std::env::set_var("AGENTOS_CONTEXT_FILES_CHARS", "4096");

        let mut cfg = RuntimeConfig::default();
        cfg.apply_env();
        assert_eq!(cfg.policy.history_messages, 7);
        assert_eq!(cfg.policy.history_chars, 1234);
        assert_eq!(cfg.policy.memory_recall_limit, 3);
        assert_eq!(cfg.policy.memory_recall_chars, 777);
        assert_eq!(
            cfg.policy.context_files,
            vec!["AGENTS.md".to_string(), "NOTES.md".to_string()],
            "trimmed, split on commas, blanks dropped"
        );
        assert_eq!(cfg.policy.context_files_chars, 4096);
        assert_eq!(cfg.policy.max_steps_per_run, 12, "an unrelated switch stays at its default");
    }

    #[test]
    fn two_listeners_cannot_share_one_address() {
        let mut cfg = RuntimeConfig::default();
        cfg.api.grpc_addr = cfg.api.http_addr.clone();
        let err = cfg.validate().unwrap_err();
        assert!(err.message.contains("must differ"), "got: {err}");
    }

    #[test]
    fn bind_addresses_must_be_host_port() {
        let mut cfg = RuntimeConfig::default();
        cfg.api.http_addr = "not-an-address".into();
        assert!(cfg.validate().is_err());
    }
}
