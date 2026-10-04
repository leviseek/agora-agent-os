//! Durable, dependency-free file store.
//!
//! Layout:
//!   <root>/<collection>/<safe-key>.json   envelope: {"key":..,"value":..,"updated_at":..}
//!   <root>/logs/<log>.jsonl               one JSON record per line, {"seq":n,"value":..}
//!
//! Keys that are not filesystem-safe are hashed, and the original key travels inside the
//! envelope, so listing always returns real keys.

use crate::store::{Store, StoreHealth};
use agentos_core::error::{Result, RuntimeError};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

struct LogState {
    seq: AtomicU64,
    write_lock: tokio::sync::Mutex<()>,
}

pub struct FileStore {
    root: PathBuf,
    logs: tokio::sync::Mutex<HashMap<String, Arc<LogState>>>,
    /// Serializes document writes per collection to keep rename-based writes tidy.
    write_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

fn safe_component(s: &str) -> String {
    let ok = !s.is_empty()
        && s.len() <= 120
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok && !s.starts_with('.') {
        s.to_string()
    } else {
        let mut h = Sha256::new();
        h.update(s.as_bytes());
        format!("h_{}", hex::encode(&h.finalize()[..16]))
    }
}

fn safe_collection(s: &str) -> Result<String> {
    if s.is_empty() || s.len() > 64 || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(RuntimeError::invalid_input(format!("illegal collection name: {s:?}")));
    }
    Ok(s.to_string())
}

impl FileStore {
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&root).await?;
        tokio::fs::create_dir_all(root.join("logs")).await?;
        Ok(Self {
            root,
            logs: tokio::sync::Mutex::new(HashMap::new()),
            write_locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn collection_dir(&self, collection: &str) -> Result<PathBuf> {
        Ok(self.root.join(safe_collection(collection)?))
    }

    fn doc_path(&self, collection: &str, key: &str) -> Result<PathBuf> {
        Ok(self.collection_dir(collection)?.join(format!("{}.json", safe_component(key))))
    }

    fn log_path(&self, log: &str) -> Result<PathBuf> {
        Ok(self.root.join("logs").join(format!("{}.jsonl", safe_collection(log)?)))
    }

    async fn lock_for(&self, collection: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.write_locks.lock();
        map.entry(collection.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn log_state(&self, log: &str) -> Result<Arc<LogState>> {
        let mut logs = self.logs.lock().await;
        if let Some(s) = logs.get(log) {
            return Ok(s.clone());
        }
        let path = self.log_path(log)?;
        let mut max = 0u64;
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if let Some(seq) = v.get("seq").and_then(|s| s.as_u64()) {
                        max = max.max(seq);
                    }
                }
            }
        }
        let state = Arc::new(LogState { seq: AtomicU64::new(max), write_lock: tokio::sync::Mutex::new(()) });
        logs.insert(log.to_string(), state.clone());
        Ok(state)
    }
}

#[async_trait]
impl Store for FileStore {
    fn backend_name(&self) -> &'static str {
        "file"
    }

    async fn put(&self, collection: &str, key: &str, value: Value) -> Result<()> {
        let path = self.doc_path(collection, key)?;
        let lock = self.lock_for(collection).await;
        let _guard = lock.lock().await;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let envelope = serde_json::json!({
            "key": key,
            "updated_at": agentos_core::now_ms(),
            "value": value,
        });
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(&envelope)?;
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    async fn get(&self, collection: &str, key: &str) -> Result<Option<Value>> {
        let path = self.doc_path(collection, key)?;
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let env: Value = serde_json::from_slice(&bytes)?;
                Ok(env.get("value").cloned())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, collection: &str, key: &str) -> Result<bool> {
        let path = self.doc_path(collection, key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn list(&self, collection: &str, limit: usize) -> Result<Vec<(String, Value)>> {
        let dir = self.collection_dir(collection)?;
        let mut out = Vec::new();
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        let mut keys = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") && !name.ends_with(".json.tmp") {
                keys.push(name);
            }
        }
        keys.sort();
        for name in keys.into_iter().take(limit) {
            let path = dir.join(&name);
            if let Ok(bytes) = tokio::fs::read(&path).await {
                if let Ok(env) = serde_json::from_slice::<Value>(&bytes) {
                    let key = env.get("key").and_then(|k| k.as_str()).unwrap_or(&name).to_string();
                    if let Some(v) = env.get("value") {
                        out.push((key, v.clone()));
                    }
                }
            }
        }
        Ok(out)
    }

    async fn scan_prefix(&self, collection: &str, prefix: &str, limit: usize) -> Result<Vec<(String, Value)>> {
        let all = self.list(collection, usize::MAX.min(100_000)).await?;
        Ok(all.into_iter().filter(|(k, _)| k.starts_with(prefix)).take(limit).collect())
    }

    async fn append_event(&self, log: &str, entry: Value) -> Result<u64> {
        let state = self.log_state(log).await?;
        let _guard = state.write_lock.lock().await;
        let seq = state.seq.load(Ordering::SeqCst) + 1;
        let path = self.log_path(log)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let line = serde_json::to_string(&serde_json::json!({ "seq": seq, "value": entry }))?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
        state.seq.store(seq, Ordering::SeqCst);
        Ok(seq)
    }

    async fn read_events(&self, log: &str, from_seq: u64, limit: usize) -> Result<Vec<(u64, Value)>> {
        let path = self.log_path(log)?;
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let seq = v.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
            if seq >= from_seq {
                out.push((seq, v.get("value").cloned().unwrap_or(Value::Null)));
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    async fn last_seq(&self, log: &str) -> Result<u64> {
        Ok(self.log_state(log).await?.seq.load(Ordering::SeqCst))
    }

    async fn flush(&self) -> Result<()> {
        Ok(())
    }

    async fn health(&self) -> Result<StoreHealth> {
        let mut collections = 0usize;
        let mut entries = 0u64;
        if let Ok(mut rd) = tokio::fs::read_dir(&self.root).await {
            while let Some(e) = rd.next_entry().await? {
                if e.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name == "logs" {
                        continue;
                    }
                    collections += 1;
                    let mut inner = tokio::fs::read_dir(e.path()).await?;
                    while inner.next_entry().await?.is_some() {
                        entries += 1;
                    }
                }
            }
        }
        let logs = self.logs.lock().await;
        Ok(StoreHealth {
            backend: "file".into(),
            ok: true,
            collections,
            entries,
            logs: logs.len(),
            last_seq: logs.values().map(|l| l.seq.load(Ordering::SeqCst)).max().unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn tmp_store() -> (FileStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("agentos-file-{}", uuid_like()));
        let s = FileStore::open(&dir).await.unwrap();
        (s, dir)
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!("{n}")
    }

    #[tokio::test]
    async fn persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("agentos-file-{}", uuid_like()));
        {
            let s = FileStore::open(&dir).await.unwrap();
            s.put("sessions", "ses_1", json!({"a": 1})).await.unwrap();
            s.append_event("events", json!({"k": "x"})).await.unwrap();
        }
        let s2 = FileStore::open(&dir).await.unwrap();
        assert_eq!(s2.get("sessions", "ses_1").await.unwrap().unwrap()["a"], 1);
        assert_eq!(s2.last_seq("events").await.unwrap(), 1);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn keys_with_separators_are_hashed_not_escaped() {
        let (s, dir) = tmp_store().await;
        s.put("c", "../../etc/passwd", json!({"v": 1})).await.unwrap();
        let rows = s.list("c", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "../../etc/passwd");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn illegal_collection_names_are_rejected() {
        let (s, _dir) = tmp_store().await;
        assert!(s.put("../escape", "k", json!(1)).await.is_err());
    }

    #[tokio::test]
    async fn event_sequences_survive_restart() {
        let dir = std::env::temp_dir().join(format!("agentos-file-{}", uuid_like()));
        {
            let s = FileStore::open(&dir).await.unwrap();
            assert_eq!(s.append_event("events", json!(1)).await.unwrap(), 1);
            assert_eq!(s.append_event("events", json!(2)).await.unwrap(), 2);
        }
        let s2 = FileStore::open(&dir).await.unwrap();
        assert_eq!(s2.append_event("events", json!(3)).await.unwrap(), 3);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
