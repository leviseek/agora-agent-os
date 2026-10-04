//! Artifact system: immutable outputs with metadata, stored behind two traits.

use crate::blob::{sha256_hex, BlobStore};
use crate::store::{collections, Collection, Store};
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{ArtifactKind, ArtifactRecord};
use agentos_core::{ArtifactId, SessionId};
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait ArtifactStore: Send + Sync + 'static {
    async fn put(
        &self,
        session_id: SessionId,
        name: &str,
        kind: ArtifactKind,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<ArtifactRecord>;
    async fn get(&self, id: &ArtifactId) -> Result<Option<ArtifactRecord>>;
    async fn read(&self, id: &ArtifactId) -> Result<Option<Vec<u8>>>;
    async fn list(&self, session_id: &SessionId, limit: usize) -> Result<Vec<ArtifactRecord>>;
    async fn delete(&self, id: &ArtifactId) -> Result<bool>;
}

/// Metadata in the Store, bytes in the BlobStore. Artifacts are immutable: a new content hash
/// means a new artifact record.
pub struct StoreArtifactStore {
    store: Arc<dyn Store>,
    blobs: Arc<dyn BlobStore>,
    max_bytes: u64,
}

impl StoreArtifactStore {
    pub fn new(store: Arc<dyn Store>, blobs: Arc<dyn BlobStore>, max_bytes: u64) -> Self {
        Self { store, blobs, max_bytes }
    }

    fn collection(&self) -> Collection<ArtifactRecord> {
        Collection::new(collections::ARTIFACTS)
    }
}

#[async_trait]
impl ArtifactStore for StoreArtifactStore {
    async fn put(
        &self,
        session_id: SessionId,
        name: &str,
        kind: ArtifactKind,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<ArtifactRecord> {
        if bytes.len() as u64 > self.max_bytes {
            return Err(RuntimeError::invalid_input(format!(
                "artifact {name} exceeds the {} byte limit",
                self.max_bytes
            ))
            .with_detail("size", bytes.len() as u64));
        }
        let digest = sha256_hex(bytes);
        let reference = self.blobs.put(bytes).await?;
        let mut record = ArtifactRecord::new(session_id, name, kind, bytes.len() as u64);
        record.sha256 = digest;
        record.content_type = content_type.to_string();
        record.storage_ref = format!("{}:{}", self.blobs.backend_name(), reference);
        if bytes.len() <= 512 {
            record.preview = Some(String::from_utf8_lossy(bytes).to_string());
        }
        self.collection().save(self.store.as_ref(), record.id.as_str(), &record).await?;
        Ok(record)
    }

    async fn get(&self, id: &ArtifactId) -> Result<Option<ArtifactRecord>> {
        self.collection().load(self.store.as_ref(), id.as_str()).await
    }

    async fn read(&self, id: &ArtifactId) -> Result<Option<Vec<u8>>> {
        match self.get(id).await? {
            None => Ok(None),
            Some(rec) => {
                let reference = rec
                    .storage_ref
                    .split_once(':')
                    .map(|(_, r)| r.to_string())
                    .ok_or_else(|| RuntimeError::storage("artifact reference is malformed"))?;
                Ok(Some(self.blobs.get(&reference).await?))
            }
        }
    }

    async fn list(&self, session_id: &SessionId, limit: usize) -> Result<Vec<ArtifactRecord>> {
        let all = self.collection().list(self.store.as_ref(), 10_000).await?;
        Ok(all.into_iter().filter(|a| &a.session_id == session_id).take(limit).collect())
    }

    async fn delete(&self, id: &ArtifactId) -> Result<bool> {
        match self.get(id).await? {
            None => Ok(false),
            Some(rec) => {
                if let Some((_, reference)) = rec.storage_ref.split_once(':') {
                    let _ = self.blobs.delete(reference).await;
                }
                self.collection().delete(self.store.as_ref(), id.as_str()).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::MemoryBlobStore;
    use crate::memory::MemoryStore;

    fn store() -> StoreArtifactStore {
        StoreArtifactStore::new(Arc::new(MemoryStore::new()), Arc::new(MemoryBlobStore::new()), 1024)
    }

    #[tokio::test]
    async fn artifact_roundtrip_has_digest_and_bytes() {
        let s = store();
        let session = SessionId::new();
        let rec = s.put(session.clone(), "out.txt", ArtifactKind::Text, "text/plain", b"hello").await.unwrap();
        assert_eq!(rec.size, 5);
        assert_eq!(rec.sha256.len(), 64);
        assert_eq!(s.read(&rec.id).await.unwrap().unwrap(), b"hello");
        assert_eq!(s.list(&session, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn oversized_artifacts_are_rejected() {
        let s = store();
        let big = vec![0u8; 2048];
        let err = s
            .put(SessionId::new(), "big.bin", ArtifactKind::Binary, "application/octet-stream", &big)
            .await
            .unwrap_err();
        assert_eq!(err.kind, agentos_core::ErrorKind::InvalidInput);
    }
}
