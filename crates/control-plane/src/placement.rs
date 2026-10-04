//! Placement Service and Worker Registry.
//!
//! v1 strategy: least pressure, where pressure blends actor count, task count and CPU. The
//! registry keeps the worker lease bookkeeping that makes "offline" and "lost" distinct states.

use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{EventKind, NewEvent, WorkerLoad, WorkerRecord};
use agentos_core::state::{StateMachine, WorkerState};
use agentos_core::telemetry::{metric_names, metrics};
use agentos_core::{now_ms, ActorId, SessionId, WorkerId};
use agentos_event_bus::EventBus;
use agentos_storage::store::{collections, Collection, Store};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementStrategy {
    LeastLoaded,
    RoundRobin,
    Pinned,
}

#[derive(Debug, Clone)]
pub struct PlacementPolicy {
    pub strategy: PlacementStrategy,
    /// Pressure above which a worker is considered full.
    pub max_pressure: f32,
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        Self { strategy: PlacementStrategy::LeastLoaded, max_pressure: 0.9 }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlacementDecision {
    pub worker_id: WorkerId,
    pub score: f32,
    pub reason: String,
    pub alternates: Vec<(WorkerId, f32)>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MigrationPlan {
    pub actor_id: ActorId,
    pub session_id: SessionId,
    pub from: WorkerId,
    pub to: WorkerId,
    pub reason: String,
}

pub struct WorkerRegistry {
    store: Arc<dyn Store>,
    workers: RwLock<HashMap<WorkerId, WorkerRecord>>,
    lease_ms: u64,
    node_id: String,
}

impl WorkerRegistry {
    pub fn new(store: Arc<dyn Store>, lease_ms: u64, node_id: impl Into<String>) -> Self {
        Self { store, workers: RwLock::new(HashMap::new()), lease_ms, node_id: node_id.into() }
    }

    fn collection(&self) -> Collection<WorkerRecord> {
        Collection::new(collections::WORKERS)
    }

    pub async fn register(&self, mut record: WorkerRecord) -> Result<WorkerRecord> {
        record.state = record.state.transition(WorkerState::Ready).unwrap_or(WorkerState::Ready);
        record.last_heartbeat = now_ms();
        record.registered_at = now_ms();
        self.collection().save(self.store.as_ref(), record.id.as_str(), &record).await?;
        self.workers.write().insert(record.id.clone(), record.clone());
        metrics().gauge(metric_names::WORKERS_ONLINE, self.online() as f64);
        Ok(record)
    }

    pub async fn heartbeat(&self, id: &WorkerId, load: WorkerLoad) -> Result<WorkerRecord> {
        // Scope the lock so no guard is ever held across an await: the future must stay Send.
        let snapshot = {
            let mut workers = self.workers.write();
            let record = workers
                .get_mut(id)
                .ok_or_else(|| RuntimeError::not_found(format!("worker {id} is not registered")))?;
            record.load = load;
            record.last_heartbeat = now_ms();
            if record.state == WorkerState::Joining {
                record.state = WorkerState::Ready;
            }
            record.clone()
        };
        self.collection().save(self.store.as_ref(), id.as_str(), &snapshot).await?;
        Ok(snapshot)
    }

    pub async fn deregister(&self, id: &WorkerId) -> Result<bool> {
        let removed = {
            let mut workers = self.workers.write();
            workers.remove(id)
        };
        if let Some(mut record) = removed {
            record.state = record.state.transition(WorkerState::Offline).unwrap_or(WorkerState::Offline);
            self.collection().save(self.store.as_ref(), id.as_str(), &record).await?;
            metrics().gauge(metric_names::WORKERS_ONLINE, self.online() as f64);
            return Ok(true);
        }
        Ok(false)
    }

    pub fn get(&self, id: &WorkerId) -> Option<WorkerRecord> {
        self.workers.read().get(id).cloned()
    }

    pub fn list(&self) -> Vec<WorkerRecord> {
        let mut out: Vec<WorkerRecord> = self.workers.read().values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn online(&self) -> usize {
        self.workers.read().values().filter(|w| w.is_alive()).count()
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Mark workers whose lease expired. Returns the ones that were marked, so the caller can
    /// fail over the actors they hosted.
    pub async fn reap_expired(&self) -> Vec<WorkerRecord> {
        let now = now_ms();
        let lease = self.lease_ms;
        let expired: Vec<WorkerRecord> = {
            let workers = self.workers.read();
            workers
                .values()
                .filter(|w| w.is_alive() && w.expired(now, lease))
                .cloned()
                .collect()
        };
        let mut marked = Vec::new();
        for mut record in expired {
            record.state = record.state.transition(WorkerState::Lost).unwrap_or(WorkerState::Lost);
            if let Err(e) = self.collection().save(self.store.as_ref(), record.id.as_str(), &record).await {
                tracing::warn!(worker = %record.id, error = %e, "failed to persist lost worker");
            }
            self.workers.write().insert(record.id.clone(), record.clone());
            marked.push(record);
        }
        if !marked.is_empty() {
            metrics().gauge(metric_names::WORKERS_ONLINE, self.online() as f64);
        }
        marked
    }

    /// Load persisted workers at bootstrap.
    pub async fn restore(&self) -> Result<usize> {
        let records = self.collection().list(self.store.as_ref(), 1000).await?;
        let mut workers = self.workers.write();
        let mut n = 0;
        for mut record in records {
            // A process that is gone cannot still be ready.
            if record.state == WorkerState::Ready {
                record.state = WorkerState::Offline;
            }
            workers.insert(record.id.clone(), record);
            n += 1;
        }
        Ok(n)
    }
}

pub struct PlacementService {
    registry: Arc<WorkerRegistry>,
    policy: PlacementPolicy,
    bus: Arc<dyn EventBus>,
    round_robin: RwLock<usize>,
}

impl PlacementService {
    pub fn new(registry: Arc<WorkerRegistry>, policy: PlacementPolicy, bus: Arc<dyn EventBus>) -> Self {
        Self { registry, policy, bus, round_robin: RwLock::new(0) }
    }

    pub fn registry(&self) -> Arc<WorkerRegistry> {
        self.registry.clone()
    }

    pub fn policy(&self) -> &PlacementPolicy {
        &self.policy
    }

    /// Choose a worker for a new actor. Deterministic given the current load snapshot, so
    /// placement is reproducible in tests.
    pub async fn place_actor(
        &self,
        session_id: &SessionId,
        actor_id: &ActorId,
        kind: &str,
    ) -> Result<PlacementDecision> {
        let mut candidates: Vec<(WorkerRecord, f32)> = self
            .registry
            .list()
            .into_iter()
            .filter(|w| w.is_alive())
            .map(|w| {
                let score = w.load.pressure(&w.capacity);
                (w, score)
            })
            .filter(|(_, score)| *score <= self.policy.max_pressure)
            .collect();

        if candidates.is_empty() {
            // Oversubscribe only if the policy allows it and some worker is alive at all.
            let alive: Vec<WorkerRecord> = self.registry.list().into_iter().filter(|w| w.is_alive()).collect();
            if alive.is_empty() {
                return Err(RuntimeError::unavailable("no worker is available for placement")
                    .with_detail("session_id", session_id.as_str()));
            }
            candidates = alive
                .into_iter()
                .map(|w| {
                    let score = w.load.pressure(&w.capacity);
                    (w, score)
                })
                .collect();
        }

        candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let decision = match self.policy.strategy {
            PlacementStrategy::LeastLoaded => {
                let (worker, score) = candidates[0].clone();
                PlacementDecision {
                    worker_id: worker.id.clone(),
                    score,
                    reason: format!("least loaded (pressure {:.2})", score),
                    alternates: candidates.iter().skip(1).take(3).map(|(w, s)| (w.id.clone(), *s)).collect(),
                }
            }
            PlacementStrategy::RoundRobin => {
                let mut idx = self.round_robin.write();
                let chosen = candidates[*idx % candidates.len()].clone();
                *idx = idx.wrapping_add(1);
                PlacementDecision {
                    worker_id: chosen.0.id.clone(),
                    score: chosen.1,
                    reason: "round robin".into(),
                    alternates: vec![],
                }
            }
            PlacementStrategy::Pinned => {
                let (worker, score) = candidates[0].clone();
                PlacementDecision {
                    worker_id: worker.id.clone(),
                    score,
                    reason: "pinned to the least loaded worker".into(),
                    alternates: vec![],
                }
            }
        };

        self.bus
            .publish(
                NewEvent::new(EventKind::ActorSpawned, format!("placed {kind} actor"))
                    .actor(actor_id.clone())
                    .session(session_id.clone())
                    .worker(decision.worker_id.clone())
                    .payload(serde_json::json!({
                        "score": decision.score,
                        "reason": decision.reason,
                        "strategy": format!("{:?}", self.policy.strategy),
                    })),
            )
            .await?;
        Ok(decision)
    }

    /// Suggest moves away from the most loaded worker. v1 only reports; the caller decides.
    pub async fn rebalance(&self, placement: &HashMap<SessionId, WorkerId>) -> Vec<MigrationPlan> {
        let workers = self.registry.list();
        if workers.len() < 2 {
            return vec![];
        }
        let mut scored: Vec<(WorkerId, f32)> = workers
            .iter()
            .filter(|w| w.is_alive())
            .map(|w| (w.id.clone(), w.load.pressure(&w.capacity)))
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let (Some(lightest), Some(heaviest)) = (scored.first().cloned(), scored.last().cloned()) else {
            return vec![];
        };
        if heaviest.1 - lightest.1 < 0.25 {
            return vec![];
        }
        placement
            .iter()
            .filter(|(_, w)| **w == heaviest.0)
            .take(1)
            .map(|(session, from)| MigrationPlan {
                actor_id: ActorId::new(),
                session_id: session.clone(),
                from: from.clone(),
                to: lightest.0.clone(),
                reason: format!("rebalance {:.2} -> {:.2}", heaviest.1, lightest.1),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_event_bus::LocalEventBus;
    use agentos_storage::memory::MemoryStore;

    async fn registry() -> (Arc<WorkerRegistry>, Arc<dyn EventBus>) {
        let store = Arc::new(MemoryStore::new());
        let bus: Arc<dyn EventBus> = Arc::new(LocalEventBus::new(store.clone(), 128, "n1").await);
        (Arc::new(WorkerRegistry::new(store, 1000, "n1")), bus)
    }

    #[tokio::test]
    async fn worker_lifecycle_register_heartbeat_deregister() {
        let (reg, _bus) = registry().await;
        let w = reg.register(WorkerRecord::new("w1", "127.0.0.1:0")).await.unwrap();
        assert_eq!(w.state, WorkerState::Ready);
        let updated = reg
            .heartbeat(&w.id, WorkerLoad { actors: 3, running_tasks: 1, cpu_percent: 10.0, memory_bytes: 0 })
            .await
            .unwrap();
        assert_eq!(updated.load.actors, 3);
        assert!(reg.deregister(&w.id).await.unwrap());
        assert_eq!(reg.online(), 0);
    }

    #[tokio::test]
    async fn heartbeat_for_unknown_worker_is_not_found() {
        let (reg, _bus) = registry().await;
        let err = reg
            .heartbeat(&WorkerId::new(), WorkerLoad::default())
            .await
            .unwrap_err();
        assert_eq!(err.kind, agentos_core::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn placement_picks_the_least_loaded_worker() {
        let (reg, bus) = registry().await;
        let busy = reg.register(WorkerRecord::new("busy", "a")).await.unwrap();
        let idle = reg.register(WorkerRecord::new("idle", "b")).await.unwrap();
        reg.heartbeat(&busy.id, WorkerLoad { actors: 200, running_tasks: 50, cpu_percent: 90.0, memory_bytes: 0 })
            .await
            .unwrap();
        reg.heartbeat(&idle.id, WorkerLoad { actors: 1, running_tasks: 0, cpu_percent: 2.0, memory_bytes: 0 })
            .await
            .unwrap();
        let svc = PlacementService::new(reg.clone(), PlacementPolicy::default(), bus);
        let decision = svc.place_actor(&SessionId::new(), &ActorId::new(), "session").await.unwrap();
        assert_eq!(decision.worker_id, idle.id);
        assert!(decision.reason.contains("least loaded"));
    }

    #[tokio::test]
    async fn placement_without_workers_is_unavailable() {
        let (reg, bus) = registry().await;
        let svc = PlacementService::new(reg, PlacementPolicy::default(), bus);
        let err = svc.place_actor(&SessionId::new(), &ActorId::new(), "session").await.unwrap_err();
        assert_eq!(err.kind, agentos_core::ErrorKind::Unavailable);
    }

    #[tokio::test]
    async fn expired_workers_are_marked_lost() {
        let store = Arc::new(MemoryStore::new());
        let bus: Arc<dyn EventBus> = Arc::new(LocalEventBus::new(store.clone(), 16, "n1").await);
        let reg = Arc::new(WorkerRegistry::new(store, 0, "n1"));
        let w = reg.register(WorkerRecord::new("short-lived", "a")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let lost = reg.reap_expired().await;
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].id, w.id);
        assert_eq!(lost[0].state, WorkerState::Lost);
        let _ = bus;
    }
}
