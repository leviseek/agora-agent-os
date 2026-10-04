//! Checkpoint persistence: the durable half of actor migration and recovery.

use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{Checkpoint, CheckpointMeta};
use agentos_core::{ActorId, CheckpointId};
use agentos_storage::store::{collections, Collection, Store};
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait CheckpointStore: Send + Sync + 'static {
    async fn save(&self, checkpoint: &Checkpoint) -> Result<()>;
    async fn load(&self, id: &CheckpointId) -> Result<Option<Checkpoint>>;
    /// Most recent checkpoint for an actor, which is what recovery starts from.
    async fn latest(&self, actor: &ActorId) -> Result<Option<Checkpoint>>;
    async fn history(&self, actor: &ActorId, limit: usize) -> Result<Vec<CheckpointMeta>>;
}

pub struct StoreCheckpointStore {
    store: Arc<dyn Store>,
}

impl StoreCheckpointStore {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    fn checkpoints(&self) -> Collection<Checkpoint> {
        Collection::new(collections::SNAPSHOTS)
    }

    fn metas(&self) -> Collection<CheckpointMeta> {
        Collection::new(collections::SNAPSHOT_META)
    }

    fn data_key(actor: &ActorId, id: &CheckpointId) -> String {
        format!("data:{}:{}", actor.as_str(), id.as_str())
    }

    fn latest_key(actor: &ActorId) -> String {
        format!("latest:{}", actor.as_str())
    }

    fn meta_key(actor: &ActorId, id: &CheckpointId) -> String {
        format!("meta:{}:{}", actor.as_str(), id.as_str())
    }
}

#[async_trait]
impl CheckpointStore for StoreCheckpointStore {
    async fn save(&self, checkpoint: &Checkpoint) -> Result<()> {
        let actor = &checkpoint.meta.actor_id;
        let id = &checkpoint.meta.id;
        self.checkpoints().save(self.store.as_ref(), &Self::data_key(actor, id), checkpoint).await?;
        self.metas().save(self.store.as_ref(), &Self::meta_key(actor, id), &checkpoint.meta).await?;
        self.metas().save(self.store.as_ref(), &Self::latest_key(actor), &checkpoint.meta).await?;
        Ok(())
    }

    async fn load(&self, id: &CheckpointId) -> Result<Option<Checkpoint>> {
        // Checkpoint ids are globally unique, so a scan is acceptable here and keeps the
        // interface free of an extra actor argument.
        let rows = self.checkpoints().list(self.store.as_ref(), 10_000).await?;
        Ok(rows.into_iter().find(|c| &c.meta.id == id))
    }

    async fn latest(&self, actor: &ActorId) -> Result<Option<Checkpoint>> {
        let key = Self::latest_key(actor);
        let meta = self.metas().load(self.store.as_ref(), &key).await?;
        match meta {
            None => Ok(None),
            Some(meta) => {
                let data_key = Self::data_key(actor, &meta.id);
                match self.checkpoints().load(self.store.as_ref(), &data_key).await? {
                    Some(cp) => Ok(Some(cp)),
                    None => Err(RuntimeError::storage(format!(
                        "checkpoint {} is indexed but its payload is missing",
                        meta.id
                    ))),
                }
            }
        }
    }

    async fn history(&self, actor: &ActorId, limit: usize) -> Result<Vec<CheckpointMeta>> {
        let prefix = format!("meta:{}:", actor.as_str());
        let mut metas = self.metas().scan_prefix(self.store.as_ref(), &prefix, limit).await?;
        metas.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(metas)
    }
}
