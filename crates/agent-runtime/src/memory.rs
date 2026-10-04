//! Memory: short term, episodic, semantic and knowledge records behind one contract.

use agentos_core::error::Result;
use agentos_core::model::{MemoryKind, MemoryQuery, MemoryRecord};
use agentos_core::{MemoryId, SessionId};
use agentos_storage::store::{collections, Collection, Store};
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait MemoryStore: Send + Sync + 'static {
    async fn write(&self, record: MemoryRecord) -> Result<MemoryId>;
    async fn recall(&self, query: MemoryQuery) -> Result<Vec<MemoryRecord>>;
    async fn forget(&self, id: &MemoryId) -> Result<bool>;
    async fn recent(&self, session: &SessionId, limit: usize) -> Result<Vec<MemoryRecord>>;
}

/// Local implementation over the Store. Ranking is deliberately simple and deterministic:
/// explicit tag matches first, then importance, then recency. A vector store can be dropped in
/// behind the same trait without the agent loop noticing.
pub struct StoreMemoryStore {
    store: Arc<dyn Store>,
}

impl StoreMemoryStore {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    fn collection(&self) -> Collection<MemoryRecord> {
        Collection::new(collections::MEMORY)
    }
}

#[async_trait]
impl MemoryStore for StoreMemoryStore {
    async fn write(&self, record: MemoryRecord) -> Result<MemoryId> {
        self.collection().save(self.store.as_ref(), record.id.as_str(), &record).await?;
        Ok(record.id)
    }

    async fn recall(&self, query: MemoryQuery) -> Result<Vec<MemoryRecord>> {
        let limit = if query.limit == 0 { 20 } else { query.limit };
        let mut all = self.collection().list(self.store.as_ref(), 10_000).await?;
        all.retain(|m| {
            if let Some(s) = &query.session_id {
                if &m.session_id != s {
                    return false;
                }
            }
            if !query.kinds.is_empty() && !query.kinds.contains(&m.kind) {
                return false;
            }
            if !query.tags.is_empty() && !query.tags.iter().any(|t| m.tags.contains(t)) {
                return false;
            }
            if let Some(text) = &query.text {
                let needle = text.to_ascii_lowercase();
                if !needle.is_empty() && !m.content.to_ascii_lowercase().contains(&needle) {
                    return false;
                }
            }
            true
        });
        all.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        all.truncate(limit);
        Ok(all)
    }

    async fn forget(&self, id: &MemoryId) -> Result<bool> {
        self.collection().delete(self.store.as_ref(), id.as_str()).await
    }

    async fn recent(&self, session: &SessionId, limit: usize) -> Result<Vec<MemoryRecord>> {
        let mut all = self.collection().list(self.store.as_ref(), 10_000).await?;
        all.retain(|m| &m.session_id == session);
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        all.truncate(limit);
        Ok(all)
    }
}

/// Convenience constructors used by the agent loop.
pub fn episode(session: SessionId, content: impl Into<String>, tags: &[&str]) -> MemoryRecord {
    let mut record = MemoryRecord::new(session, MemoryKind::Episode, content);
    record.tags = tags.iter().map(|t| t.to_string()).collect();
    record.importance = 0.6;
    record
}

pub fn semantic(session: SessionId, content: impl Into<String>, tags: &[&str]) -> MemoryRecord {
    let mut record = MemoryRecord::new(session, MemoryKind::Semantic, content);
    record.tags = tags.iter().map(|t| t.to_string()).collect();
    record.importance = 0.8;
    record
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_storage::memory::MemoryStore as RawStore;

    fn store() -> StoreMemoryStore {
        StoreMemoryStore::new(Arc::new(RawStore::new()))
    }

    #[tokio::test]
    async fn write_then_recall_by_text_and_kind() {
        let m = store();
        let session = SessionId::new();
        m.write(episode(session.clone(), "user asked for a sum", &["math"])).await.unwrap();
        m.write(semantic(session.clone(), "prefers short answers", &["preference"])).await.unwrap();

        let hits = m
            .recall(MemoryQuery { session_id: Some(session.clone()), text: Some("sum".into()), limit: 5, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("sum"));

        let prefs = m
            .recall(MemoryQuery {
                session_id: Some(session),
                kinds: vec![MemoryKind::Semantic],
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(prefs.len(), 1);
    }

    #[tokio::test]
    async fn recall_is_scoped_to_one_session() {
        let m = store();
        let a = SessionId::new();
        let b = SessionId::new();
        m.write(episode(a.clone(), "a-only", &[])).await.unwrap();
        let hits = m.recall(MemoryQuery::session(b, 10)).await.unwrap();
        assert!(hits.is_empty());
    }
}
