//! The Store trait: keyed documents plus append-only logs.

use agentos_core::error::{Result, RuntimeError};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::marker::PhantomData;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoreHealth {
    pub backend: String,
    pub ok: bool,
    pub collections: usize,
    pub entries: u64,
    pub logs: usize,
    pub last_seq: u64,
}

/// Backend-agnostic persistence. Implementations must be safe for concurrent use.
#[async_trait]
pub trait Store: Send + Sync + 'static {
    fn backend_name(&self) -> &'static str;

    async fn put(&self, collection: &str, key: &str, value: Value) -> Result<()>;
    async fn get(&self, collection: &str, key: &str) -> Result<Option<Value>>;
    async fn delete(&self, collection: &str, key: &str) -> Result<bool>;
    async fn list(&self, collection: &str, limit: usize) -> Result<Vec<(String, Value)>>;
    async fn scan_prefix(&self, collection: &str, prefix: &str, limit: usize) -> Result<Vec<(String, Value)>>;

    /// Append to an ordered log and return the assigned sequence number (1-based, monotonic).
    async fn append_event(&self, log: &str, entry: Value) -> Result<u64>;
    async fn read_events(&self, log: &str, from_seq: u64, limit: usize) -> Result<Vec<(u64, Value)>>;
    async fn last_seq(&self, log: &str) -> Result<u64>;

    async fn flush(&self) -> Result<()>;
    async fn health(&self) -> Result<StoreHealth>;
}

/// Well-known collection and log names, so typos cannot silently create a new namespace.
pub mod collections {
    pub const WORKSPACES: &str = "workspaces";
    pub const SESSIONS: &str = "sessions";
    pub const ACTORS: &str = "actors";
    pub const AGENTS: &str = "agents";
    pub const RUNS: &str = "runs";
    pub const TASKS: &str = "tasks";
    pub const GRAPHS: &str = "task_graphs";
    pub const WORKERS: &str = "workers";
    pub const CAPABILITIES: &str = "capabilities";
    pub const ARTIFACTS: &str = "artifacts";
    pub const MEMORY: &str = "memory";
    pub const SNAPSHOTS: &str = "snapshots";
    pub const SNAPSHOT_META: &str = "snapshot_meta";
    pub const DIRECTORY: &str = "directory";
    pub const POLICIES: &str = "policies";

    pub const LOG_EVENTS: &str = "events";
    pub const LOG_ACTOR_JOURNAL: &str = "actor_journal";
}

/// Typed view over one collection. Keeps serde noise out of the planes.
pub struct Collection<T> {
    name: &'static str,
    _t: PhantomData<T>,
}

impl<T> Clone for Collection<T> {
    fn clone(&self) -> Self {
        Self { name: self.name, _t: PhantomData }
    }
}
impl<T> Copy for Collection<T> {}

impl<T> Collection<T>
where
    T: Serialize + DeserializeOwned + Send + Sync,
{
    pub const fn new(name: &'static str) -> Self {
        Self { name, _t: PhantomData }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub async fn save(&self, store: &dyn Store, key: &str, value: &T) -> Result<()> {
        let json = serde_json::to_value(value)
            .map_err(|e| RuntimeError::storage(format!("serialize {}: {e}", self.name)))?;
        store.put(self.name, key, json).await
    }

    pub async fn load(&self, store: &dyn Store, key: &str) -> Result<Option<T>> {
        match store.get(self.name, key).await? {
            Some(v) => Ok(Some(serde_json::from_value(v).map_err(|e| {
                RuntimeError::storage(format!("deserialize {}: {e}", self.name))
            })?)),
            None => Ok(None),
        }
    }

    pub async fn list(&self, store: &dyn Store, limit: usize) -> Result<Vec<T>> {
        let rows = store.list(self.name, limit).await?;
        rows.into_iter().map(|(_, v)| self.decode(v)).collect()
    }

    pub async fn scan_prefix(&self, store: &dyn Store, prefix: &str, limit: usize) -> Result<Vec<T>> {
        let rows = store.scan_prefix(self.name, prefix, limit).await?;
        rows.into_iter().map(|(_, v)| self.decode(v)).collect()
    }

    pub async fn delete(&self, store: &dyn Store, key: &str) -> Result<bool> {
        store.delete(self.name, key).await
    }

    fn decode(&self, v: Value) -> Result<T> {
        serde_json::from_value(v)
            .map_err(|e| RuntimeError::storage(format!("deserialize {}: {e}", self.name)))
    }
}
