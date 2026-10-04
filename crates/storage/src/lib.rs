//! agentos-storage - the persistence contract and its implementations.
//!
//! Boundaries: no other crate may know which backend is in use. Everything above this crate
//! talks to the Store, BlobStore and ArtifactStore traits only.
//!
//! Layout of a store:
//! * collections - keyed documents (sessions, actors, tasks, workers, capabilities, memory).
//! * logs - append-only, monotonic sequences (events, actor mailbox journal). This is what
//!   makes replay-based actor recovery possible.
//! * blobs - content addressed bytes (artifacts).

pub mod artifact;
pub mod blob;
pub mod file;
pub mod memory;
#[cfg(feature = "redb-backend")]
pub mod redb_store;
pub mod store;

pub use artifact::{ArtifactStore, StoreArtifactStore};
pub use blob::{BlobStore, FsBlobStore, MemoryBlobStore};
pub use file::FileStore;
pub use memory::MemoryStore;
#[cfg(feature = "redb-backend")]
pub use redb_store::RedbStore;
pub use store::{
    collections, Collection, Store, StoreHealth,
};

use agentos_core::config::{StorageConfig, StoreBackend};
use agentos_core::error::{Result, RuntimeError};
use std::sync::Arc;

/// Open the configured backend. This is the only place that maps configuration to an
/// implementation, which keeps the rest of the runtime backend-agnostic.
pub async fn open_store(cfg: &StorageConfig) -> Result<Arc<dyn Store>> {
    match cfg.backend {
        StoreBackend::Memory => Ok(Arc::new(MemoryStore::new())),
        StoreBackend::File => Ok(Arc::new(FileStore::open(cfg.data_dir.join("store")).await?)),
        StoreBackend::Redb => {
            #[cfg(feature = "redb-backend")]
            {
                Ok(Arc::new(RedbStore::open(cfg.data_dir.join("store.redb"))?))
            }
            #[cfg(not(feature = "redb-backend"))]
            {
                Err(RuntimeError::invalid_input(
                    "store backend redb requires the agentos-storage/redb-backend feature",
                ))
            }
        }
    }
}

/// Always-available blob backend for artifacts, rooted at the data directory.
pub async fn open_blobs(cfg: &StorageConfig) -> Result<Arc<dyn BlobStore>> {
    match cfg.backend {
        StoreBackend::Memory => Ok(Arc::new(MemoryBlobStore::new())),
        _ => Ok(Arc::new(FsBlobStore::open(cfg.data_dir.join("blobs")).await?)),
    }
}
