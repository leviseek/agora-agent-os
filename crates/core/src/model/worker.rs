use crate::ids::{NodeId, WorkerId};
use crate::state::WorkerState;
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WorkerCapacity {
    pub max_actors: u32,
    pub max_tasks: u32,
    pub memory_bytes: u64,
}

impl Default for WorkerCapacity {
    fn default() -> Self {
        Self { max_actors: 256, max_tasks: 64, memory_bytes: 4 * 1024 * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerLoad {
    pub actors: u32,
    pub running_tasks: u32,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
}

impl WorkerLoad {
    /// Placement score in [0, 1]: lower is better. Bounded so no single signal dominates.
    pub fn pressure(&self, cap: &WorkerCapacity) -> f32 {
        let actor_p = self.actors as f32 / cap.max_actors.max(1) as f32;
        let task_p = self.running_tasks as f32 / cap.max_tasks.max(1) as f32;
        let cpu_p = (self.cpu_percent / 100.0).clamp(0.0, 1.0);
        (actor_p * 0.4 + task_p * 0.4 + cpu_p * 0.2).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub id: WorkerId,
    pub node_id: NodeId,
    pub name: String,
    pub state: WorkerState,
    pub addr: String,
    pub capacity: WorkerCapacity,
    pub load: WorkerLoad,
    pub labels: BTreeMap<String, String>,
    pub capabilities: Vec<String>,
    pub version: String,
    pub registered_at: Timestamp,
    pub last_heartbeat: Timestamp,
}

impl WorkerRecord {
    pub fn new(name: impl Into<String>, addr: impl Into<String>) -> Self {
        let now = crate::now_ms();
        Self {
            id: WorkerId::new(),
            node_id: NodeId::new(),
            name: name.into(),
            state: WorkerState::Joining,
            addr: addr.into(),
            capacity: WorkerCapacity::default(),
            load: WorkerLoad::default(),
            labels: BTreeMap::new(),
            capabilities: vec![],
            version: crate::DOMAIN_VERSION.to_string(),
            registered_at: now,
            last_heartbeat: now,
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.state.is_terminal()
    }

    pub fn expired(&self, now: Timestamp, lease_ms: u64) -> bool {
        now.saturating_sub(self.last_heartbeat) > lease_ms
    }
}
