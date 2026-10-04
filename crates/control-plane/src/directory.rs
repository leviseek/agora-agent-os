//! Actor Directory: where does the actor for this session live right now?
//!
//! Backed by the Store for durability and by an in-process cache for the hot path.

use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::ActorRecord;
use agentos_core::state::ActorState;
use agentos_core::telemetry::{metric_names, metrics};
use agentos_core::{now_ms, ActorId, SessionId, Timestamp, WorkerId};
use agentos_storage::store::{collections, Collection, Store};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirectoryEntry {
    pub session_id: SessionId,
    pub actor_id: ActorId,
    pub kind: String,
    pub worker_id: Option<WorkerId>,
    pub node_id: Option<String>,
    pub generation: u64,
    pub state: ActorState,
    /// Reserved: transport endpoints where this actor can be reached.
    pub endpoints: Vec<String>,
    pub updated_at: Timestamp,
}

impl DirectoryEntry {
    pub fn from_record(record: &ActorRecord, node_id: Option<String>) -> Self {
        Self {
            session_id: record.session_id.clone(),
            actor_id: record.id.clone(),
            kind: record.kind.clone(),
            worker_id: record.worker_id.clone(),
            node_id,
            generation: record.generation,
            state: record.state,
            endpoints: vec![],
            updated_at: now_ms(),
        }
    }

    pub fn is_routable(&self) -> bool {
        !matches!(self.state, ActorState::Stopped | ActorState::Failed)
    }
}

pub struct ActorDirectory {
    store: Arc<dyn Store>,
    cache: RwLock<HashMap<SessionId, DirectoryEntry>>,
    node_id: String,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ActorDirectory {
    pub fn new(store: Arc<dyn Store>, node_id: impl Into<String>) -> Self {
        Self {
            store,
            cache: RwLock::new(HashMap::new()),
            node_id: node_id.into(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    fn collection(&self) -> Collection<DirectoryEntry> {
        Collection::new(collections::DIRECTORY)
    }

    pub async fn register(&self, entry: DirectoryEntry) -> Result<DirectoryEntry> {
        let mut entry = entry;
        entry.updated_at = now_ms();
        if entry.node_id.is_none() {
            entry.node_id = Some(self.node_id.clone());
        }
        self.collection().save(self.store.as_ref(), entry.session_id.as_str(), &entry).await?;
        self.cache.write().insert(entry.session_id.clone(), entry.clone());
        Ok(entry)
    }

    /// Hot-path lookup. Cache first; the store is only touched on a miss, which is exactly the
    /// "control plane is off the per-message path" requirement.
    pub async fn lookup(&self, session: &SessionId) -> Result<Option<DirectoryEntry>> {
        if let Some(entry) = self.cache.read().get(session).cloned() {
            self.hits.fetch_add(1, Ordering::Relaxed);
            metrics().inc(metric_names::CACHE_HITS, 1);
            return Ok(Some(entry));
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        metrics().inc(metric_names::CACHE_MISSES, 1);
        let loaded = self.collection().load(self.store.as_ref(), session.as_str()).await?;
        if let Some(entry) = &loaded {
            self.cache.write().insert(session.clone(), entry.clone());
        }
        Ok(loaded)
    }

    pub async fn lookup_actor(&self, actor: &ActorId) -> Result<Option<DirectoryEntry>> {
        if let Some(entry) = self.cache.read().values().find(|e| &e.actor_id == actor).cloned() {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Some(entry));
        }
        let all = self.collection().list(self.store.as_ref(), 10_000).await?;
        let found = all.into_iter().find(|e| &e.actor_id == actor);
        if let Some(entry) = &found {
            self.cache.write().insert(entry.session_id.clone(), entry.clone());
        }
        Ok(found)
    }

    pub async fn unregister(&self, session: &SessionId) -> Result<bool> {
        self.cache.write().remove(session);
        self.collection().delete(self.store.as_ref(), session.as_str()).await
    }

    pub async fn heartbeat(&self, actor: &ActorId, state: ActorState) -> Result<bool> {
        let mut entries = self.cache.write();
        let key = entries.iter().find(|(_, e)| &e.actor_id == actor).map(|(k, _)| k.clone());
        let Some(key) = key else {
            return Ok(false);
        };
        if let Some(entry) = entries.get_mut(&key) {
            entry.state = state;
            entry.updated_at = now_ms();
            let snapshot = entry.clone();
            drop(entries);
            self.collection().save(self.store.as_ref(), snapshot.session_id.as_str(), &snapshot).await?;
            return Ok(true);
        }
        Ok(false)
    }

    pub async fn list(&self) -> Result<Vec<DirectoryEntry>> {
        let mut entries = self.collection().list(self.store.as_ref(), 10_000).await?;
        entries.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        Ok(entries)
    }

    pub async fn by_worker(&self, worker: &WorkerId) -> Result<Vec<DirectoryEntry>> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .filter(|e| e.worker_id.as_ref() == Some(worker))
            .collect())
    }

    /// Load every entry into the cache. Called once at bootstrap so the first message of an
    /// existing session is already a cache hit.
    pub async fn warm(&self) -> Result<usize> {
        let entries = self.list().await?;
        let mut cache = self.cache.write();
        for entry in &entries {
            cache.insert(entry.session_id.clone(), entry.clone());
        }
        Ok(entries.len())
    }

    pub fn cache_len(&self) -> usize {
        self.cache.read().len()
    }

    pub fn cache_stats(&self) -> (u64, u64) {
        (self.hits.load(Ordering::Relaxed), self.misses.load(Ordering::Relaxed))
    }

    /// Sessions whose actor is not routable any more; used by recovery.
    pub async fn unreachable(&self) -> Result<Vec<DirectoryEntry>> {
        Ok(self.list().await?.into_iter().filter(|e| !e.is_routable()).collect())
    }

    pub fn require(entry: Option<DirectoryEntry>, session: &SessionId) -> Result<DirectoryEntry> {
        entry.ok_or_else(|| {
            RuntimeError::not_found(format!("no actor is registered for session {session}"))
                .with_detail("session_id", session.as_str())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_storage::memory::MemoryStore;

    fn entry(session: SessionId) -> DirectoryEntry {
        DirectoryEntry {
            session_id: session,
            actor_id: ActorId::new(),
            kind: "session".into(),
            worker_id: None,
            node_id: None,
            generation: 0,
            state: ActorState::Active,
            endpoints: vec![],
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn register_lookup_unregister_cycle() {
        let store = Arc::new(MemoryStore::new());
        let dir = ActorDirectory::new(store, "node-1");
        let session = SessionId::new();
        dir.register(entry(session.clone())).await.unwrap();
        let found = dir.lookup(&session).await.unwrap().unwrap();
        assert_eq!(found.session_id, session);
        assert!(dir.unregister(&session).await.unwrap());
        assert!(dir.lookup(&session).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cache_is_used_before_the_store() {
        let store = Arc::new(MemoryStore::new());
        let dir = ActorDirectory::new(store, "node-1");
        let session = SessionId::new();
        dir.register(entry(session.clone())).await.unwrap();
        for _ in 0..5 {
            dir.lookup(&session).await.unwrap();
        }
        let (hits, misses) = dir.cache_stats();
        assert_eq!(hits, 5);
        assert_eq!(misses, 0);
    }

    #[tokio::test]
    async fn cold_lookup_reads_through_and_warms_the_cache() {
        let store = Arc::new(MemoryStore::new());
        let session = SessionId::new();
        {
            let dir = ActorDirectory::new(store.clone(), "node-1");
            dir.register(entry(session.clone())).await.unwrap();
        }
        let dir2 = ActorDirectory::new(store, "node-1");
        assert!(dir2.lookup(&session).await.unwrap().is_some());
        let (_, misses) = dir2.cache_stats();
        assert_eq!(misses, 1);
        assert_eq!(dir2.cache_len(), 1);
    }

    #[tokio::test]
    async fn heartbeat_updates_state() {
        let store = Arc::new(MemoryStore::new());
        let dir = ActorDirectory::new(store, "node-1");
        let e = entry(SessionId::new());
        let actor = e.actor_id.clone();
        dir.register(e).await.unwrap();
        assert!(dir.heartbeat(&actor, ActorState::Idle).await.unwrap());
        assert_eq!(dir.lookup_actor(&actor).await.unwrap().unwrap().state, ActorState::Idle);
    }
}
