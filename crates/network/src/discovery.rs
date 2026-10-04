//! Node discovery abstraction.
//!
//! The runtime only needs three things from discovery: who is out there, when a node joins and
//! when one goes away. libp2p fills this in when the p2p feature is enabled; the stub below
//! keeps single-node deployments working with zero network surface.

use agentos_core::{now_ms, NodeId, Timestamp};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeInfo {
    pub node_id: NodeId,
    pub name: String,
    /// libp2p multiaddr or an RPC endpoint.
    pub address: String,
    pub grpc_endpoint: Option<String>,
    pub version: String,
    pub capabilities: Vec<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str) -> NodeInfo {
        NodeInfo {
            node_id: NodeId::new(),
            name: name.into(),
            address: format!("/ip4/127.0.0.1/tcp/0/p2p/{name}"),
            grpc_endpoint: Some("http://127.0.0.1:8789".into()),
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
}
