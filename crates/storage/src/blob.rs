//! Content-addressed blob storage for artifacts.

use agentos_core::error::{Result, RuntimeError};
use async_trait::async_trait;
use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Address bytes by their hash. Returns a storage reference usable with get/delete.
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    fn backend_name(&self) -> &'static str;
    async fn put(&self, bytes: &[u8]) -> Result<String>;
    async fn get(&self, reference: &str) -> Result<Vec<u8>>;
    async fn delete(&self, reference: &str) -> Result<bool>;
    async fn size(&self, reference: &str) -> Result<Option<u64>>;
}

/// Filesystem blobs, sharded by the first two hex characters of the digest.
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, reference: &str) -> Result<PathBuf> {
        // References are digests we produced. Anything else is treated as hostile input.
        if reference.len() != 64 || !reference.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(RuntimeError::invalid_input(format!("illegal blob reference: {reference:?}")));
        }
        Ok(self.root.join(&reference[..2]).join(reference))
    }
}

#[async_trait]
impl BlobStore for FsBlobStore {
    fn backend_name(&self) -> &'static str {
        "fs"
    }

    async fn put(&self, bytes: &[u8]) -> Result<String> {
        let digest = sha256_hex(bytes);
        let path = self.path_for(&digest)?;
        if path.exists() {
            return Ok(digest);
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(digest)
    }

    async fn get(&self, reference: &str) -> Result<Vec<u8>> {
        let path = self.path_for(reference)?;
        tokio::fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RuntimeError::not_found(format!("blob {reference} not found"))
            } else {
                e.into()
            }
        })
    }

    async fn delete(&self, reference: &str) -> Result<bool> {
        let path = self.path_for(reference)?;
        match tokio::fs::remove_file(&path).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn size(&self, reference: &str) -> Result<Option<u64>> {
        let path = self.path_for(reference)?;
        match tokio::fs::metadata(&path).await {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Default)]
pub struct MemoryBlobStore {
    blobs: RwLock<HashMap<String, Vec<u8>>>,
}

impl MemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl BlobStore for MemoryBlobStore {
    fn backend_name(&self) -> &'static str {
        "memory"
    }

    async fn put(&self, bytes: &[u8]) -> Result<String> {
        let digest = sha256_hex(bytes);
        self.blobs.write().insert(digest.clone(), bytes.to_vec());
        Ok(digest)
    }

    async fn get(&self, reference: &str) -> Result<Vec<u8>> {
        self.blobs
            .read()
            .get(reference)
            .cloned()
            .ok_or_else(|| RuntimeError::not_found(format!("blob {reference} not found")))
    }

    async fn delete(&self, reference: &str) -> Result<bool> {
        Ok(self.blobs.write().remove(reference).is_some())
    }

    async fn size(&self, reference: &str) -> Result<Option<u64>> {
        Ok(self.blobs.read().get(reference).map(|b| b.len() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn content_addressed_roundtrip() {
        let s = MemoryBlobStore::new();
        let r1 = s.put(b"hello").await.unwrap();
        let r2 = s.put(b"hello").await.unwrap();
        assert_eq!(r1, r2, "same bytes must map to the same reference");
        assert_eq!(s.get(&r1).await.unwrap(), b"hello");
        assert_eq!(s.size(&r1).await.unwrap(), Some(5));
    }

    #[tokio::test]
    async fn fs_store_rejects_traversal_references() {
        let dir = std::env::temp_dir().join("agentos-blob-test");
        let s = FsBlobStore::open(&dir).await.unwrap();
        assert!(s.get("../../windows/win.ini").await.is_err());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
