//! Embedded key-value backend (redb). Enabled with the redb-backend feature.

use crate::store::{Store, StoreHealth};
use agentos_core::error::{Result, RuntimeError};
use async_trait::async_trait;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// collection -> (key -> json envelope)
const DOCS: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("docs");
/// log -> (seq -> json)
const LOGS: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("logs");
/// log -> last seq
const SEQS: TableDefinition<&str, u64> = TableDefinition::new("seqs");

pub struct RedbStore {
    db: Arc<Database>,
    path: PathBuf,
    /// Last known sequence per log, mirrored in memory to avoid a read per append.
    seq_cache: Mutex<HashMap<String, AtomicU64>>,
}

impl RedbStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(&path)
            .map_err(|e| RuntimeError::storage(format!("open redb {}: {e}", path.display())))?;
        // Touch the tables so an empty database is still well formed.
        {
            let tx = db.begin_write().map_err(map_db)?;
            tx.open_table(DOCS).map_err(map_db)?;
            tx.open_table(LOGS).map_err(map_db)?;
            tx.open_table(SEQS).map_err(map_db)?;
            tx.commit().map_err(map_db)?;
        }
        Ok(Self { db: Arc::new(db), path, seq_cache: Mutex::new(HashMap::new()) })
    }

    fn cached_seq(&self, log: &str) -> Result<u64> {
        if let Some(a) = self.seq_cache.lock().unwrap().get(log) {
            return Ok(a.load(Ordering::SeqCst));
        }
        let tx = self.db.begin_read().map_err(map_db)?;
        let table = tx.open_table(SEQS).map_err(map_db)?;
        let current = table
            .get(log)
            .map_err(map_db)?
            .map(|v| v.value())
            .unwrap_or(0);
        self.seq_cache
            .lock()
            .unwrap()
            .insert(log.to_string(), AtomicU64::new(current));
        Ok(current)
    }
}

fn map_db<E: std::fmt::Display>(e: E) -> RuntimeError {
    RuntimeError::storage(format!("redb: {e}"))
}

#[async_trait]
impl Store for RedbStore {
    fn backend_name(&self) -> &'static str {
        "redb"
    }

    async fn put(&self, collection: &str, key: &str, value: Value) -> Result<()> {
        let bytes = serde_json::to_vec(&value)?;
        let tx = self.db.begin_write().map_err(map_db)?;
        {
            let mut table = tx.open_table(DOCS).map_err(map_db)?;
            table.insert((collection, key), bytes.as_slice()).map_err(map_db)?;
        }
        tx.commit().map_err(map_db)?;
        Ok(())
    }

    async fn get(&self, collection: &str, key: &str) -> Result<Option<Value>> {
        let tx = self.db.begin_read().map_err(map_db)?;
        let table = tx.open_table(DOCS).map_err(map_db)?;
        match table.get((collection, key)).map_err(map_db)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    async fn delete(&self, collection: &str, key: &str) -> Result<bool> {
        let tx = self.db.begin_write().map_err(map_db)?;
        let removed = {
            let mut table = tx.open_table(DOCS).map_err(map_db)?;
            table.remove((collection, key)).map_err(map_db)?.is_some()
        };
        tx.commit().map_err(map_db)?;
        Ok(removed)
    }

    async fn list(&self, collection: &str, limit: usize) -> Result<Vec<(String, Value)>> {
        let tx = self.db.begin_read().map_err(map_db)?;
        let table = tx.open_table(DOCS).map_err(map_db)?;
        let mut out = Vec::new();
        for row in table.iter().map_err(map_db)? {
            let (k, v) = row.map_err(map_db)?;
            let (c, key) = k.value();
            if c == collection {
                out.push((key.to_string(), serde_json::from_slice(v.value())?));
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    async fn scan_prefix(&self, collection: &str, prefix: &str, limit: usize) -> Result<Vec<(String, Value)>> {
        Ok(self
            .list(collection, usize::MAX.min(100_000))
            .await?
            .into_iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .take(limit)
            .collect())
    }

    async fn append_event(&self, log: &str, entry: Value) -> Result<u64> {
        let seq = self.cached_seq(log)? + 1;
        let bytes = serde_json::to_vec(&entry)?;
        let tx = self.db.begin_write().map_err(map_db)?;
        {
            let mut logs = tx.open_table(LOGS).map_err(map_db)?;
            logs.insert((log, seq), bytes.as_slice()).map_err(map_db)?;
            let mut seqs = tx.open_table(SEQS).map_err(map_db)?;
            seqs.insert(log, seq).map_err(map_db)?;
        }
        tx.commit().map_err(map_db)?;
        if let Some(a) = self.seq_cache.lock().unwrap().get(log) {
            a.store(seq, Ordering::SeqCst);
        }
        Ok(seq)
    }

    async fn read_events(&self, log: &str, from_seq: u64, limit: usize) -> Result<Vec<(u64, Value)>> {
        let tx = self.db.begin_read().map_err(map_db)?;
        let table = tx.open_table(LOGS).map_err(map_db)?;
        let mut out = Vec::new();
        for row in table.range((log, from_seq)..).map_err(map_db)? {
            let (k, v) = row.map_err(map_db)?;
            let (l, seq) = k.value();
            if l != log {
                break;
            }
            out.push((seq, serde_json::from_slice(v.value())?));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    async fn last_seq(&self, log: &str) -> Result<u64> {
        self.cached_seq(log)
    }

    async fn flush(&self) -> Result<()> {
        // redb commits durably on every transaction; nothing to do.
        Ok(())
    }

    async fn health(&self) -> Result<StoreHealth> {
        let tx = self.db.begin_read().map_err(map_db)?;
        let docs = tx.open_table(DOCS).map_err(map_db)?;
        let entries = docs.len().map_err(map_db)?;
        let logs = tx.open_table(LOGS).map_err(map_db)?;
        let log_rows = logs.len().map_err(map_db)?;
        Ok(StoreHealth {
            backend: "redb".into(),
            ok: true,
            collections: 1,
            entries,
            logs: log_rows as usize,
            last_seq: 0,
        })
    }
}

impl std::fmt::Debug for RedbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RedbStore({})", self.path.display())
    }
}
