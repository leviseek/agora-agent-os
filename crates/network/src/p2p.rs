//! libp2p peer-to-peer node: discovery, connectivity and a control-message channel.
//!
//! Scope for v1, deliberately narrow:
//!   * mDNS discovery so nodes on the same network find each other with no configuration,
//!   * identify + ping so a peer can be described and liveness-checked,
//!   * a gossipsub topic for CONTROL messages only (membership, capability advertisements).
//!
//! No business message travels over P2P. Session work stays on the RPC plane, which is what the
//! "P2P is not on the hot path" rule means in practice.

use crate::discovery::{DiscoveryEvent, NodeDiscovery, NodeInfo};
use agentos_core::error::{Result, RuntimeError};
use agentos_core::{now_ms, NodeId};
use async_trait::async_trait;
use futures::StreamExt;
use libp2p::gossipsub::{self, IdentTopic};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{identify, mdns, noise, ping, tcp, yamux, Multiaddr, PeerId, SwarmBuilder};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// The one topic used for control-plane gossip in v1.
pub const CONTROL_TOPIC: &str = "agentos/control/v1";

#[derive(Debug, Clone)]
pub struct P2pConfig {
    pub node_name: String,
    pub listen: Vec<String>,
    pub bootstrap: Vec<String>,
    pub mdns: bool,
    pub advertise_interval_ms: u64,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            node_name: "agentos".into(),
            listen: vec!["/ip4/0.0.0.0/tcp/0".into()],
            bootstrap: vec![],
            mdns: true,
            advertise_interval_ms: 15_000,
        }
    }
}

#[derive(Debug, Clone)]
pub enum P2pCommand {
    Publish { topic: String, payload: Vec<u8> },
    Dial(String),
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct ControlMessage {
    pub topic: String,
    pub from: PeerId,
    pub payload: Vec<u8>,
}

#[derive(NetworkBehaviour)]
struct AgentOsBehaviour {
    /// Toggle lets mDNS be switched off by configuration without a second behaviour type.
    mdns: Toggle<mdns::tokio::Behaviour>,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    gossip: gossipsub::Behaviour,
}

pub struct Libp2pNode {
    local_peer: PeerId,
    listen_addrs: RwLock<Vec<String>>,
    peers: RwLock<HashMap<PeerId, Arc<NodeInfo>>>,
    discovery_tx: broadcast::Sender<DiscoveryEvent>,
    control_tx: broadcast::Sender<ControlMessage>,
    commands: mpsc::Sender<P2pCommand>,
    cancellation: CancellationToken,
}

impl Libp2pNode {
    /// Start the swarm. Returns the node handle; the swarm itself runs in a background task until
    /// the cancellation token fires.
    pub async fn start(cfg: P2pConfig) -> Result<Arc<Self>> {
        let (commands_tx, commands_rx) = mpsc::channel(256);
        let (discovery_tx, _) = broadcast::channel(256);
        let (control_tx, _) = broadcast::channel(256);
        let cancellation = CancellationToken::new();

        let bootstrap: Vec<String> = cfg.bootstrap.clone();
        let listen: Vec<String> = cfg.listen.clone();
        let mdns_enabled = cfg.mdns;
        let node_name = cfg.node_name.clone();

        let mut swarm = SwarmBuilder::with_new_identity()
            .with_tokio()
            .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)
            .map_err(|e| RuntimeError::network(format!("cannot build the tcp transport: {e}")))?
            .with_behaviour(|key| {
                let mdns = if mdns_enabled {
                    Some(
                        mdns::tokio::Behaviour::new(mdns::Config::default(), key.public().to_peer_id())
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?,
                    )
                } else {
                    None
                };
                let identify = identify::Behaviour::new(identify::Config::new(
                    "/agentos/0.1.0".to_string(),
                    key.public(),
                ));
                let ping = ping::Behaviour::new(ping::Config::new());
                let gossip = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossipsub::Config::default(),
                )
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
                Ok(AgentOsBehaviour { mdns: Toggle::from(mdns), identify, ping, gossip })
            })
            .map_err(|e| RuntimeError::network(format!("cannot build the swarm behaviour: {e}")))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(30)))
            .build();

        let topic = IdentTopic::new(CONTROL_TOPIC);
        swarm
            .behaviour_mut()
            .gossip
            .subscribe(&topic)
            .map_err(|e| RuntimeError::network(format!("cannot subscribe to {CONTROL_TOPIC}: {e}")))?;
        for addr in &listen {
            let multi: Multiaddr = addr
                .parse()
                .map_err(|e| RuntimeError::invalid_input(format!("bad multiaddr {addr}: {e}")))?;
            swarm
                .listen_on(multi)
                .map_err(|e| RuntimeError::network(format!("cannot listen on {addr}: {e}")))?;
        }
        for addr in &bootstrap {
            if let Ok(multi) = addr.parse::<Multiaddr>() {
                if let Err(e) = swarm.dial(multi.clone()) {
                    tracing::warn!(%multi, error = %e, "bootstrap dial failed");
                }
            }
        }

        let local_peer = *swarm.local_peer_id();
        let node = Arc::new(Self {
            local_peer,
            listen_addrs: RwLock::new(vec![]),
            peers: RwLock::new(HashMap::new()),
            discovery_tx: discovery_tx.clone(),
            control_tx: control_tx.clone(),
            commands: commands_tx,
            cancellation: cancellation.clone(),
        });

        let node_for_task = node.clone();
        tokio::spawn(async move {
            let mut commands = commands_rx;
            loop {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    command = commands.recv() => match command {
                        Some(P2pCommand::Publish { topic, payload }) => {
                            let topic = IdentTopic::new(topic);
                            if let Err(e) = swarm.behaviour_mut().gossip.publish(topic, payload) {
                                tracing::warn!(error = %e, "gossip publish failed");
                            }
                        }
                        Some(P2pCommand::Dial(addr)) => {
                            if let Ok(multi) = addr.parse::<Multiaddr>() {
                                let _ = swarm.dial(multi);
                            }
                        }
                        Some(P2pCommand::Shutdown) | None => break,
                    },
                    event = swarm.select_next_some() => {
                        match event {
                            SwarmEvent::NewListenAddr { address, .. } => {
                                let text = address.to_string();
                                tracing::info!(addr = %text, "p2p listening");
                                node_for_task.listen_addrs.write().push(text);
                            }
                            SwarmEvent::Behaviour(AgentOsBehaviourEvent::Mdns(mdns::Event::Discovered(peers))) => {
                                for (peer_id, addr) in peers {
                                    let info = NodeInfo {
                                        node_id: NodeId::from_raw(peer_id.to_string()),
                                        name: peer_id.to_string(),
                                        address: addr.to_string(),
                                        grpc_endpoint: None,
                                        version: "0.1.0".into(),
                                        capabilities: vec![],
                                        auth_required: false,
                                        transport: "mdns".into(),
                                        discovered_at: now_ms(),
                                        last_seen: now_ms(),
                                    };
                                    node_for_task.peers.write().insert(peer_id, Arc::new(info.clone()));
                                    let _ = node_for_task.discovery_tx.send(DiscoveryEvent::Joined(Arc::new(info)));
                                    let _ = swarm.dial(addr);
                                }
                            }
                            SwarmEvent::Behaviour(AgentOsBehaviourEvent::Mdns(mdns::Event::Expired(peers))) => {
                                for (peer_id, _) in peers {
                                    node_for_task.peers.write().remove(&peer_id);
                                    let _ = node_for_task
                                        .discovery_tx
                                        .send(DiscoveryEvent::Left(NodeId::from_raw(peer_id.to_string())));
                                }
                            }
                            SwarmEvent::Behaviour(AgentOsBehaviourEvent::Gossip(gossipsub::Event::Message {
                                propagation_source,
                                message,
                                ..
                            })) => {
                                let _ = node_for_task.control_tx.send(ControlMessage {
                                    topic: message.topic.to_string(),
                                    from: propagation_source,
                                    payload: message.data,
                                });
                            }
                            SwarmEvent::Behaviour(AgentOsBehaviourEvent::Identify(identify::Event::Received {
                                peer_id,
                                info,
                                ..
                            })) => {
                                if let Some(existing) = node_for_task.peers.write().get_mut(&peer_id) {
                                    let mut updated = (**existing).clone();
                                    updated.address = info.listen_addrs.first().map(|a| a.to_string()).unwrap_or_default();
                                    updated.last_seen = now_ms();
                                    *existing = Arc::new(updated);
                                }
                            }
                            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                                tracing::debug!(peer = %peer_id, "p2p connected");
                            }
                            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                                tracing::debug!(peer = %peer_id, "p2p disconnected");
                            }
                            _ => {}
                        }
                    }
                }
            }
            tracing::info!(node = %node_name, "p2p node stopped");
        });

        Ok(node)
    }

    pub fn local_peer(&self) -> PeerId {
        self.local_peer
    }

    pub fn listen_addrs(&self) -> Vec<String> {
        self.listen_addrs.read().clone()
    }

    /// Publish a control message on a topic.
    pub async fn publish(&self, topic: &str, payload: &[u8]) -> Result<()> {
        self.commands
            .send(P2pCommand::Publish { topic: topic.to_string(), payload: payload.to_vec() })
            .await
            .map_err(|_| RuntimeError::unavailable("the p2p node is not running"))
    }

    pub async fn dial(&self, address: &str) -> Result<()> {
        self.commands
            .send(P2pCommand::Dial(address.to_string()))
            .await
            .map_err(|_| RuntimeError::unavailable("the p2p node is not running"))
    }

    pub fn subscribe_control(&self) -> broadcast::Receiver<ControlMessage> {
        self.control_tx.subscribe()
    }

    pub async fn shutdown(&self) {
        self.cancellation.cancel();
        let _ = self.commands.send(P2pCommand::Shutdown).await;
    }
}

#[async_trait]
impl NodeDiscovery for Libp2pNode {
    fn name(&self) -> &'static str {
        "libp2p"
    }

    async fn start(&self) -> Result<()> {
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown().await;
        Ok(())
    }

    fn nodes(&self) -> Vec<Arc<NodeInfo>> {
        self.peers.read().values().cloned().collect()
    }

    fn subscribe(&self) -> broadcast::Receiver<DiscoveryEvent> {
        self.discovery_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two nodes on the loopback interface must discover each other through mDNS.
    #[tokio::test]
    async fn nodes_discover_each_other_over_mdns() {
        let a = Libp2pNode::start(P2pConfig {
            node_name: "a".into(),
            listen: vec!["/ip4/127.0.0.1/tcp/0".into()],
            bootstrap: vec![],
            mdns: true,
            advertise_interval_ms: 1000,
        })
        .await
        .expect("node a starts");
        let b = Libp2pNode::start(P2pConfig {
            node_name: "b".into(),
            listen: vec!["/ip4/127.0.0.1/tcp/0".into()],
            bootstrap: vec![],
            mdns: true,
            advertise_interval_ms: 1000,
        })
        .await
        .expect("node b starts");

        let mut rx = a.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut discovered = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(DiscoveryEvent::Joined(info))) => {
                    if info.node_id.as_str() == b.local_peer().to_string() {
                        discovered = true;
                        break;
                    }
                }
                _ => continue,
            }
        }
        a.shutdown().await;
        b.shutdown().await;
        assert!(discovered, "mDNS discovery did not find the peer within the deadline");
    }
}
