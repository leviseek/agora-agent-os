//! Node discovery abstraction.
//!
//! The runtime only needs three things from discovery: who is out there, when a node joins and
//! when one goes away. libp2p fills this in when the p2p feature is enabled; the stub below
//! keeps single-node deployments working with zero network surface.

use agentos_core::{now_ms, NodeId, Timestamp};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeInfo {
    pub node_id: NodeId,
    pub name: String,
    /// HTTP endpoint a client (or another node) can reach this node on.
    pub address: String,
    pub grpc_endpoint: Option<String>,
    pub version: String,
    pub capabilities: Vec<String>,
    /// Whether this node demands a bearer token. The console uses it to tell a node it can simply
    /// switch to from one that will ask for a token first.
    #[serde(default)]
    pub auth_required: bool,
    /// Reserved for cross-machine discovery: how the node was found (file, mdns, bootstrap).
    #[serde(default)]
    pub transport: String,
    pub discovered_at: Timestamp,
    pub last_seen: Timestamp,
}

#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    Joined(Arc<NodeInfo>),
    Left(NodeId),
}

#[async_trait]
pub trait NodeDiscovery: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    async fn start(&self) -> agentos_core::error::Result<()>;
    async fn stop(&self) -> agentos_core::error::Result<()>;
    fn nodes(&self) -> Vec<Arc<NodeInfo>>;
    fn subscribe(&self) -> broadcast::Receiver<DiscoveryEvent>;

    /// Publish (or refresh) this node's own advertisement. Backends that announce themselves
    /// through their transport instead - libp2p/mDNS, for instance - accept and ignore it, which
    /// is why the default is a no-op rather than an error.
    fn advertise(&self, _info: NodeInfo) -> agentos_core::error::Result<()> {
        Ok(())
    }

    /// Poll for changes and return what changed since the previous call. Push-based backends
    /// return nothing and rely on subscribe() instead.
    fn refresh(&self) -> agentos_core::error::Result<Vec<DiscoveryEvent>> {
        Ok(vec![])
    }
}

/// No-op discovery. Single-node deployments use this and pay nothing.
pub struct StubDiscovery {
    local: RwLock<Option<Arc<NodeInfo>>>,
    tx: broadcast::Sender<DiscoveryEvent>,
}

impl StubDiscovery {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { local: RwLock::new(None), tx }
    }

    /// Register the local node so the topology view still has something to show.
    pub fn with_local(self, node: NodeInfo) -> Self {
        *self.local.write() = Some(Arc::new(node));
        self
    }
}

impl Default for StubDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NodeDiscovery for StubDiscovery {
    fn name(&self) -> &'static str {
        "stub"
    }

    async fn start(&self) -> agentos_core::error::Result<()> {
        Ok(())
    }

    async fn stop(&self) -> agentos_core::error::Result<()> {
        Ok(())
    }

    fn nodes(&self) -> Vec<Arc<NodeInfo>> {
        self.local.read().clone().into_iter().collect()
    }

    fn subscribe(&self) -> broadcast::Receiver<DiscoveryEvent> {
        self.tx.subscribe()
    }
}

/// A tiny in-memory registry used by tests to simulate a multi-node mesh.
pub struct MemoryDiscovery {
    nodes: RwLock<HashMap<NodeId, Arc<NodeInfo>>>,
    tx: RwLock<Option<broadcast::Sender<DiscoveryEvent>>>,
}

impl MemoryDiscovery {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { nodes: RwLock::new(HashMap::new()), tx: RwLock::new(Some(tx)) }
    }

    pub fn join(&self, mut node: NodeInfo) {
        node.last_seen = now_ms();
        if let Some(tx) = self.tx.read().as_ref() {
            let _ = tx.send(DiscoveryEvent::Joined(Arc::new(node.clone())));
        }
        self.nodes.write().insert(node.node_id.clone(), Arc::new(node));
    }

    pub fn leave(&self, id: &NodeId) {
        self.nodes.write().remove(id);
        if let Some(tx) = self.tx.read().as_ref() {
            let _ = tx.send(DiscoveryEvent::Left(id.clone()));
        }
    }
}

impl Default for MemoryDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NodeDiscovery for MemoryDiscovery {
    fn name(&self) -> &'static str {
        "memory"
    }
    async fn start(&self) -> agentos_core::error::Result<()> {
        Ok(())
    }
    async fn stop(&self) -> agentos_core::error::Result<()> {
        Ok(())
    }
    fn nodes(&self) -> Vec<Arc<NodeInfo>> {
        self.nodes.read().values().cloned().collect()
    }
    fn subscribe(&self) -> broadcast::Receiver<DiscoveryEvent> {
        match self.tx.read().as_ref() {
            Some(tx) => tx.subscribe(),
            None => broadcast::channel(1).0.subscribe(),
        }
    }
}

/// Same-machine discovery through a shared directory of small JSON advertisements.
///
/// Why a directory and not multicast: two checkouts of this repository are two processes owned by
/// the same user, so a per-user directory is a reliable rendezvous that needs no network, no
/// firewall exception and no configuration. It is also inspectable - the files can be read by a
/// human when something is not showing up.
///
/// The contract is deliberately weak (last writer wins, no locking): an advertisement is a hint,
/// and a stale one expires by TTL. Nothing on the request path depends on it.
pub struct LocalFileDiscovery {
    dir: PathBuf,
    ttl_ms: u64,
    local: RwLock<Option<Arc<NodeInfo>>>,
    known: RwLock<HashMap<String, Arc<NodeInfo>>>,
    tx: broadcast::Sender<DiscoveryEvent>,
}

/// Advertisements are named after the node id; anything outside this set is hashed away so a
/// hostile or careless id cannot escape the directory.
fn safe_file_name(node_id: &str) -> String {
    let ok = !node_id.is_empty()
        && node_id.len() <= 96
        && node_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok && !node_id.starts_with('.') {
        format!("{node_id}.json")
    } else {
        let mut hash = 0u64;
        for byte in node_id.as_bytes() {
            hash = hash.wrapping_mul(1099511628211).wrapping_add(u64::from(*byte));
        }
        format!("n{hash:016x}.json")
    }
}

impl LocalFileDiscovery {
    pub fn new(dir: impl Into<PathBuf>, ttl_ms: u64) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            dir: dir.into(),
            ttl_ms: ttl_ms.max(1_000),
            local: RwLock::new(None),
            known: RwLock::new(HashMap::new()),
            tx,
        }
    }

    /// Create the shared directory. Called once at bootstrap.
    pub fn open(dir: impl Into<PathBuf>, ttl_ms: u64) -> agentos_core::error::Result<Self> {
        let discovery = Self::new(dir, ttl_ms);
        std::fs::create_dir_all(&discovery.dir)?;
        Ok(discovery)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn ttl_ms(&self) -> u64 {
        self.ttl_ms
    }

    /// Publish (or refresh) our own advertisement. Atomic rename so a reader never sees a torn file.
    pub fn advertise(&self, mut info: NodeInfo) -> agentos_core::error::Result<()> {
        info.last_seen = now_ms();
        let path = self.dir.join(safe_file_name(info.node_id.as_str()));
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&info)?)?;
        std::fs::rename(&tmp, &path)?;
        *self.local.write() = Some(Arc::new(info));
        Ok(())
    }

    /// Remove our advertisement. Called on a clean shutdown so peers notice immediately instead of
    /// waiting for the TTL.
    pub fn withdraw(&self) -> agentos_core::error::Result<()> {
        let local = self.local.write().take();
        if let Some(info) = local {
            let path = self.dir.join(safe_file_name(info.node_id.as_str()));
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    /// Read everyone else's advertisement, expire what is stale, and report what changed.
    pub fn scan(&self) -> agentos_core::error::Result<Vec<DiscoveryEvent>> {
        let now = now_ms();
        let own_id = self.local.read().as_ref().map(|info| info.node_id.to_string());
        let mut seen: HashMap<String, Arc<NodeInfo>> = HashMap::new();

        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error.into()),
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(info) = serde_json::from_slice::<NodeInfo>(&bytes) else {
                // A half-written or older-format file is skipped, never fatal.
                continue;
            };
            if Some(info.node_id.to_string()) == own_id {
                continue;
            }
            let age = now.saturating_sub(info.last_seen);
            if age > self.ttl_ms {
                // Twice the TTL means nobody refreshed it for a long time: reclaim the file so the
                // directory does not accumulate advertisements from crashed processes.
                if age > self.ttl_ms.saturating_mul(2) {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            seen.insert(info.node_id.to_string(), Arc::new(info));
        }

        let mut events = Vec::new();
        {
            let mut known = self.known.write();
            for (id, info) in &seen {
                if !known.contains_key(id) {
                    events.push(DiscoveryEvent::Joined(info.clone()));
                }
            }
            for id in known.keys() {
                if !seen.contains_key(id) {
                    events.push(DiscoveryEvent::Left(NodeId::from_raw(id.clone())));
                }
            }
            *known = seen;
        }

        for event in &events {
            let _ = self.tx.send(event.clone());
        }
        Ok(events)
    }

    pub fn peers(&self) -> Vec<Arc<NodeInfo>> {
        let mut peers: Vec<Arc<NodeInfo>> = self.known.read().values().cloned().collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        peers
    }

    /// Alias kept for callers that think of it as "scan now".
    pub fn refresh_now(&self) -> agentos_core::error::Result<Vec<DiscoveryEvent>> {
        self.scan()
    }

    pub fn local(&self) -> Option<Arc<NodeInfo>> {
        self.local.read().clone()
    }
}

#[async_trait]
impl NodeDiscovery for LocalFileDiscovery {
    fn name(&self) -> &'static str {
        "local-file"
    }

    async fn start(&self) -> agentos_core::error::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(())
    }

    async fn stop(&self) -> agentos_core::error::Result<()> {
        self.withdraw()
    }

    fn nodes(&self) -> Vec<Arc<NodeInfo>> {
        self.peers()
    }

    fn subscribe(&self) -> broadcast::Receiver<DiscoveryEvent> {
        self.tx.subscribe()
    }

    fn advertise(&self, info: NodeInfo) -> agentos_core::error::Result<()> {
        LocalFileDiscovery::advertise(self, info)
    }

    fn refresh(&self) -> agentos_core::error::Result<Vec<DiscoveryEvent>> {
        self.scan()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str) -> NodeInfo {
        NodeInfo {
            node_id: NodeId::new(),
            name: name.into(),
            address: format!("/ip4/127.0.0.1/tcp/0/p2p/{name}"),
            grpc_endpoint: Some("http://127.0.0.1:8789".into()),
            auth_required: false,
            transport: "test".into(),
            version: "0.1.0".into(),
            capabilities: vec!["echo".into()],
            discovered_at: now_ms(),
            last_seen: now_ms(),
        }
    }

    #[tokio::test]
    async fn stub_discovery_exposes_only_the_local_node() {
        let d = StubDiscovery::new().with_local(node("self"));
        d.start().await.unwrap();
        assert_eq!(d.nodes().len(), 1);
        assert_eq!(d.name(), "stub");
    }

    #[tokio::test]
    async fn memory_discovery_emits_join_and_leave() {
        let d = MemoryDiscovery::new();
        let mut rx = d.subscribe();
        let n = node("peer-1");
        let id = n.node_id.clone();
        d.join(n);
        assert!(matches!(rx.recv().await.unwrap(), DiscoveryEvent::Joined(_)));
        assert_eq!(d.nodes().len(), 1);
        d.leave(&id);
        assert!(matches!(rx.recv().await.unwrap(), DiscoveryEvent::Left(_)));
        assert!(d.nodes().is_empty());
    }

    // --- local file discovery ---------------------------------------------------------------

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agora-discovery-{label}-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn info(id: &str, name: &str, endpoint: &str) -> NodeInfo {
        let mut n = node(name);
        n.node_id = NodeId::from_raw(id);
        n.address = endpoint.to_string();
        n
    }

    #[test]
    fn two_nodes_in_one_directory_see_each_other() {
        let dir = temp_dir("pair");
        let a = LocalFileDiscovery::open(&dir, 5_000).unwrap();
        let b = LocalFileDiscovery::open(&dir, 5_000).unwrap();

        a.advertise(info("node-a", "agora-a", "http://127.0.0.1:8788")).unwrap();
        b.advertise(info("node-b", "agora-b", "http://127.0.0.1:8791")).unwrap();

        // Each node sees the other, and never itself.
        let events_a = a.scan().unwrap();
        assert_eq!(events_a.len(), 1, "a sees exactly one join");
        assert!(matches!(&events_a[0], DiscoveryEvent::Joined(info) if info.name == "agora-b"));
        assert_eq!(a.peers().len(), 1);
        assert_eq!(a.peers()[0].address, "http://127.0.0.1:8791");

        let events_b = b.scan().unwrap();
        assert_eq!(events_b.len(), 1);
        assert!(matches!(&events_b[0], DiscoveryEvent::Joined(info) if info.name == "agora-a"));

        // Re-scanning is idempotent: no duplicate joins.
        assert!(a.scan().unwrap().is_empty());
        assert_eq!(a.peers().len(), 1);
    }

    #[test]
    fn a_withdrawn_node_is_reported_as_left() {
        let dir = temp_dir("leave");
        let a = LocalFileDiscovery::open(&dir, 5_000).unwrap();
        let b = LocalFileDiscovery::open(&dir, 5_000).unwrap();
        a.advertise(info("node-a", "agora-a", "http://127.0.0.1:8788")).unwrap();
        b.advertise(info("node-b", "agora-b", "http://127.0.0.1:8791")).unwrap();

        assert_eq!(a.scan().unwrap().len(), 1);
        b.withdraw().unwrap();
        let events = a.scan().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], DiscoveryEvent::Left(id) if id.as_str() == "node-b"));
        assert!(a.peers().is_empty());
    }

    #[test]
    fn stale_advertisements_expire_and_are_reclaimed() {
        let dir = temp_dir("stale");
        let a = LocalFileDiscovery::open(&dir, 1_000).unwrap();

        let mut old = info("node-dead", "agora-dead", "http://127.0.0.1:9999");
        old.last_seen = now_ms().saturating_sub(30_000);
        let path = dir.join("node-dead.json");
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        assert!(a.scan().unwrap().is_empty(), "an expired node is not announced");
        assert!(a.peers().is_empty());
        assert!(!path.exists(), "and its file is reclaimed");
    }

    #[test]
    fn a_corrupt_advertisement_never_breaks_the_scan() {
        let dir = temp_dir("corrupt");
        let a = LocalFileDiscovery::open(&dir, 5_000).unwrap();
        std::fs::write(dir.join("broken.json"), b"{ not json").unwrap();
        a.advertise(info("node-a", "agora-a", "http://127.0.0.1:8788")).unwrap();
        assert!(a.scan().unwrap().is_empty());
    }

    #[test]
    fn hostile_node_ids_cannot_escape_the_directory() {
        let dir = temp_dir("safe");
        let a = LocalFileDiscovery::open(&dir, 5_000).unwrap();
        a.advertise(info("../../../etc/passwd", "evil", "http://127.0.0.1:1")).unwrap();
        let files: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(files.len(), 1);
        assert!(files[0].starts_with('n') || files[0].contains("passwd") == false);
        assert!(dir.join(&files[0]).starts_with(&dir));
    }
}
