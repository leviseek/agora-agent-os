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
    fn node_id_falls_back_to_the_name() {
        let mut cfg = RuntimeConfig::default();
        cfg.node.name = "agora-a".into();
        assert_eq!(cfg.effective_node_id(), "agora-a", "name doubles as id when unset");

        cfg.node.node_id = Some("   ".into());
        assert_eq!(cfg.effective_node_id(), "agora-a", "blank ids are ignored");

        cfg.node.node_id = Some("node-a-01".into());
        assert_eq!(cfg.effective_node_id(), "node-a-01", "explicit identity wins");
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
