//! In-process store. Used by tests, by the desktop demo and as a warm cache.

use crate::store::{Store, StoreHealth};
use agentos_core::error::Result;
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

#[derive(Default)]
struct Inner {
    collections: HashMap<String, BTreeMap<String, Value>>,
    logs: HashMap<String, BTreeMap<u64, Value>>,
    seq: HashMap<String, u64>,
}

#[derive(Default)]
pub struct MemoryStore {
    inner: RwLock<Inner>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for MemoryStore {
    fn backend_name(&self) -> &'static str {
        "memory"
    }

    async fn put(&self, collection: &str, key: &str, value: Value) -> Result<()> {
        self.inner
            .write()
            .collections
            .entry(collection.to_string())
            .or_default()
            .insert(key.to_string(), value);
        Ok(())
    }

    async fn get(&self, collection: &str, key: &str) -> Result<Option<Value>> {
        Ok(self
            .inner
            .read()
            .collections
            .get(collection)
            .and_then(|c| c.get(key))
            .cloned())
    }

    async fn delete(&self, collection: &str, key: &str) -> Result<bool> {
        Ok(self
            .inner
            .write()
            .collections
            .get_mut(collection)
            .and_then(|c| c.remove(key))
            .is_some())
    }

    async fn list(&self, collection: &str, limit: usize) -> Result<Vec<(String, Value)>> {
        let guard = self.inner.read();
        Ok(guard
            .collections
            .get(collection)
            .map(|c| c.iter().take(limit).map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    }

    async fn scan_prefix(&self, collection: &str, prefix: &str, limit: usize) -> Result<Vec<(String, Value)>> {
        let guard = self.inner.read();
        Ok(guard
            .collections
            .get(collection)
            .map(|c| {
                c.iter()
                    .filter(|(k, _)| k.starts_with(prefix))
                    .take(limit)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn append_event(&self, log: &str, entry: Value) -> Result<u64> {
        let mut guard = self.inner.write();
        let next = guard.seq.entry(log.to_string()).or_insert(0);
        *next += 1;
        let seq = *next;
        guard.logs.entry(log.to_string()).or_default().insert(seq, entry);
        Ok(seq)
    }

    async fn read_events(&self, log: &str, from_seq: u64, limit: usize) -> Result<Vec<(u64, Value)>> {
        let guard = self.inner.read();
        Ok(guard
            .logs
            .get(log)
            .map(|l| {
                l.range(from_seq..)
                    .take(limit)
                    .map(|(k, v)| (*k, v.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn last_seq(&self, log: &str) -> Result<u64> {
        Ok(self.inner.read().seq.get(log).copied().unwrap_or(0))
    }

    async fn flush(&self) -> Result<()> {
        Ok(())
    }

    async fn health(&self) -> Result<StoreHealth> {
        let guard = self.inner.read();
        Ok(StoreHealth {
            backend: "memory".into(),
            ok: true,
            collections: guard.collections.len(),
            entries: guard.collections.values().map(|c| c.len() as u64).sum(),
            logs: guard.logs.len(),
            last_seq: guard.seq.values().copied().max().unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn crud_roundtrip() {
        let s = MemoryStore::new();
        s.put("c", "a", json!({"v":1})).await.unwrap();
        assert_eq!(s.get("c", "a").await.unwrap().unwrap()["v"], 1);
        assert_eq!(s.list("c", 10).await.unwrap().len(), 1);
        assert!(s.delete("c", "a").await.unwrap());
        assert!(s.get("c", "a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn logs_are_monotonic_and_range_readable() {
        let s = MemoryStore::new();
        assert_eq!(s.append_event("l", json!(1)).await.unwrap(), 1);
        assert_eq!(s.append_event("l", json!(2)).await.unwrap(), 2);
        assert_eq!(s.last_seq("l").await.unwrap(), 2);
        let rows = s.read_events("l", 2, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 2);
    }
}
