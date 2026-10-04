//! Cross-cutting discovery tests.
//!
//! The bug this file exists for: identity used to fall back to the node *name*, and every node
//! defaults to the same name. Two checkouts therefore advertised themselves under one identity,
//! skipped each other's advertisement as "my own", and neither could see the other. Every earlier
//! test passed because every earlier test set an explicit node id.

use agentos_core::config::{RuntimeConfig, StoreBackend};
use agentos_kernel::Kernel;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct Fixture {
    root: PathBuf,
    discovery_dir: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!("agora-disc-{label}-{}", agentos_core::now_ms()));
        let discovery_dir = root.join("nodes");
        std::fs::create_dir_all(&discovery_dir).unwrap();
        Self { root, discovery_dir }
    }

    /// A node configured exactly the way a fresh checkout is: no node id, no node name.
    async fn node(&self, instance: &str) -> Arc<Kernel> {
        let dir = self.root.join(instance);
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = RuntimeConfig::default();
        config.storage.backend = StoreBackend::Memory;
        config.storage.data_dir = dir.join("data");
        config.policy.workspace_root = dir.join("workspace");
        config.observability.log_level = "error".into();
        config.discovery.dir = self.discovery_dir.clone();
        config.discovery.ttl_ms = 1_500;
        Kernel::bootstrap(config).await.unwrap()
    }
}

async fn wait_until<F>(mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    for _ in 0..40 {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// The regression: two nodes with identical configuration, started from different working
/// directories, must still be distinct and see each other.
#[tokio::test]
async fn two_default_nodes_are_distinct_and_see_each_other() {
    let fixture = Fixture::new("defaults");
    let a = fixture.node("a").await;
    let b = fixture.node("b").await;

    assert_eq!(a.config.node.name, b.config.node.name, "both use the default name");
    assert!(
        a.config.effective_node_id() != b.config.effective_node_id(),
        "identity must not be derived from the shared default name"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("a").join("data").join("node.id"))
            .unwrap()
            .trim(),
        a.config.effective_node_id(),
        "identity is persisted next to the state, so it survives a restart"
    );

    let seen = wait_until(|| !a.peers().is_empty() && !b.peers().is_empty()).await;
    assert!(seen, "each node should discover the other without any configuration");

    let peer_of_a = &a.peers()[0];
    assert_eq!(peer_of_a.node_id.as_str(), b.config.effective_node_id());
    assert_eq!(a.peers().len(), 1, "and exactly once");
    assert_eq!(b.peers()[0].node_id.as_str(), a.config.effective_node_id());

    a.shutdown().await;
    b.shutdown().await;
}

/// A node must never list itself, however many nodes are running.
#[tokio::test]
async fn a_node_never_lists_itself_and_forgets_the_dead() {
    let fixture = Fixture::new("forget");
    let a = fixture.node("a").await;
    let b = fixture.node("b").await;

    assert!(wait_until(|| a.peers().len() == 1).await, "b becomes visible");
    let self_id = a.config.effective_node_id();
    assert!(a.peers().iter().all(|peer| peer.node_id.as_str() != self_id));

    // A clean shutdown withdraws the advertisement immediately - no TTL wait.
    b.shutdown().await;
    assert!(
        wait_until(|| a.peers().is_empty()).await,
        "a withdrawn node disappears from the list"
    );

    a.shutdown().await;
}
