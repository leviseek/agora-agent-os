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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Deepseek,
    Openai,
    Qwen,
    Local,
    /// Deterministic in-process provider used by tests and by offline demos.
    Mock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub name: String,
    pub node_id: Option<String>,
    pub region: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub backend: StoreBackend,
    pub data_dir: PathBuf,
    /// How many events to keep hot in the in-memory ring before compaction.
    pub event_log_retention: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2pConfig {
    pub enabled: bool,
    pub listen: Vec<String>,
    pub bootstrap: Vec<String>,
    pub mdns: bool,
    /// P2P is deliberately off the hot path: only discovery and control gossip use it.
    pub advertise_interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    pub log_level: String,
    pub log_format: LogFormat,
    pub metrics_enabled: bool,
    /// Reserved: OTLP exporter is not wired in v1.
    pub otlp_endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            node: NodeConfig {
                name: "agentos-local".into(),
                node_id: None,
                region: "local".into(),
            },
            api: ApiConfig {
                http_addr: "127.0.0.1:8788".into(),
                ws_path: "/v1/ws".into(),
                grpc_addr: "127.0.0.1:8789".into(),
                auth_token_env: "AGENTOS_AUTH_TOKEN".into(),
                rate_limit_per_minute: 600,
                cors_allow_origin: "*".into(),
                request_timeout_ms: 30_000,
            },
            storage: StorageConfig {
                backend: StoreBackend::File,
                data_dir: PathBuf::from("./data"),
                event_log_retention: 20_000,
            },
            models: ModelConfig {
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
                    },
                    ProviderConfig {
                        name: "deepseek".into(),
                        kind: ProviderKind::Deepseek,
                        model: "deepseek-chat".into(),
                        base_url: "https://api.deepseek.com".into(),
                        api_key_env: "DEEPSEEK_API_KEY".into(),
                        enabled: true,
                        priority: 30,
                        timeout_ms: 60_000,
                    },
                    ProviderConfig {
                        name: "openai".into(),
                        kind: ProviderKind::Openai,
                        model: "gpt-4o-mini".into(),
                        base_url: "https://api.openai.com".into(),
                        api_key_env: "OPENAI_API_KEY".into(),
                        enabled: true,
                        priority: 20,
                        timeout_ms: 60_000,
                    },
                    ProviderConfig {
                        name: "qwen".into(),
                        kind: ProviderKind::Qwen,
                        model: "qwen-plus".into(),
                        base_url: "https://dashscope.aliyuncs.com/compatible-mode".into(),
                        api_key_env: "DASHSCOPE_API_KEY".into(),
                        enabled: true,
                        priority: 15,
                        timeout_ms: 60_000,
                    },
                    ProviderConfig {
                        name: "local".into(),
                        kind: ProviderKind::Local,
                        model: "local-llm".into(),
                        base_url: "http://127.0.0.1:11434".into(),
                        api_key_env: "AGENTOS_LOCAL_API_KEY".into(),
                        enabled: true,
                        priority: 5,
                        timeout_ms: 120_000,
                    },
                ],
                request_timeout_ms: 60_000,
                max_retries: 2,
            },
            policy: PolicyConfig {
                workspace_root: PathBuf::from("./workspace"),
                allowed_capabilities: vec![],
                denied_capabilities: vec![],
                approval_required: vec![],
                max_steps_per_run: 12,
                history_messages: 20,
                history_chars: 8_000,
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
            },
            p2p: P2pConfig {
                enabled: false,
                listen: vec!["/ip4/0.0.0.0/tcp/0".into()],
                bootstrap: vec![],
                mdns: true,
                advertise_interval_ms: 15_000,
            },
            observability: ObservabilityConfig {
                log_level: "info,agentos=debug".into(),
                log_format: LogFormat::Text,
                metrics_enabled: true,
                otlp_endpoint: None,
            },
            // On by default: "start a node, see it from the console" is the point of the feature.
            // It only writes a small file into a per-user directory, and AGENTOS_DISCOVERY=off
            // turns it off completely.
            discovery: DiscoveryConfig {
                enabled: true,
                dir: default_discovery_dir(),
                ttl_ms: 10_000,
                advertise: true,
            },
            limits: RuntimeLimits {
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
            },
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
            if let Ok(n) = v.parse() { self.policy.history_messages = n; }
        }
        if let Some(v) = Self::env_str("AGENTOS_HISTORY_CHARS") {
            if let Ok(n) = v.parse() { self.policy.history_chars = n; }
        }
        if let Some(v) = Self::env_str("AGENTOS_MEMORY_RECALL_LIMIT") {
            if let Ok(n) = v.parse() { self.policy.memory_recall_limit = n; }
        }
        if let Some(v) = Self::env_str("AGENTOS_MEMORY_RECALL_CHARS") {
            if let Ok(n) = v.parse() { self.policy.memory_recall_chars = n; }
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
            if let Ok(n) = v.parse() { self.policy.context_files_chars = n; }
        }
        if let Some(v) = Self::env_str("AGENTOS_COMPACTION") {
            self.policy.compaction_enabled = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = Self::env_str("AGENTOS_COMPACTION_MIN_MESSAGES") {
            if let Ok(n) = v.parse() { self.policy.compaction_min_messages = n; }
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
                m
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn context_switches_apply_from_the_environment_alone() {
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
