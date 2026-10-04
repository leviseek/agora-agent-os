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

/// The goal line of a turn record, if the content has one.
///
/// Turn records are written as "goal: .../answer: ..." precisely so this is a parse rather than a
/// guess: the goal is what identifies a turn, and recall needs to know which turns the recent
/// history window already shows.
pub fn goal_of(content: &str) -> Option<&str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("goal: "))
        .map(str::trim)
        .filter(|goal| !goal.is_empty())
}

/// Choose the memories worth injecting into a prompt.
///
/// The recent-history window is the first line of defence and it is always more faithful than a
/// summary, so recall only adds what that window can no longer show: a record whose goal is still
/// visible in the conversation is dropped. What survives is capped by characters, newest first as
/// the store returned it.
pub fn recall_context(
    records: &[MemoryRecord],
    history_texts: &[String],
    max_chars: usize,
) -> Option<String> {
    if records.is_empty() || max_chars == 0 {
        return None;
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for record in records {
        let content = record.content.trim();
        if content.is_empty() {
            continue;
        }
        if let Some(goal) = goal_of(content) {
            // Already in the conversation: the model can read the real thing, not a note about it.
            if history_texts.iter().any(|text| text.trim() == goal) {
                continue;
            }
        }
        let cost = content.chars().count();
        if used + cost > max_chars {
            break;
        }
        used += cost;
        kept.push(content);
    }
    if kept.is_empty() {
        return None;
    }
    let mut out = String::from("Earlier in this session, outside the recent turns you can see:\n");
    for content in kept {
        out.push_str("- ");
        out.push_str(content);
        out.push('\n');
    }
    Some(out)
}

/// Convenience constructors used by the agent loop.
pub fn episode(session: SessionId, content: impl Into<String>, tags: &[&str]) -> MemoryRecord {
    let mut record = MemoryRecord::new(session, MemoryKind::Episode, content);
    record.tags = tags.iter().map(|t| t.to_string()).collect();
    record.importance = 0.6;
    record
}

/// A compaction summary: one record standing in for a range of turns.
///
/// Importance is raised above ordinary turns on purpose - the store sorts by it, so a summary of
/// ten dropped turns reaches the prompt before any single one of them.
pub fn summary(session: SessionId, content: impl Into<String>) -> MemoryRecord {
    let mut record = MemoryRecord::new(session, MemoryKind::Episode, content);
    record.tags = vec!["session".into(), "summary".into()];
    record.importance = 0.95;
    record
}

pub fn semantic(session: SessionId, content: impl Into<String>, tags: &[&str]) -> MemoryRecord {
    let mut record = MemoryRecord::new(session, MemoryKind::Semantic, content);
    record.tags = tags.iter().map(|t| t.to_string()).collect();
    record.importance = 0.8;
    record
}

#[cfg(test)]
mod recall_tests {
    use super::*;

    fn turn(goal: &str, answer: &str) -> MemoryRecord {
        let session = SessionId::new();
        episode(session, format!("goal: {goal}\nanswer: {answer}"), &["session", "turn"])
    }

    #[test]
    fn parses_the_goal_line_of_a_turn_record() {
        assert_eq!(goal_of("goal: what is 6*7?\nanswer: 42"), Some("what is 6*7?"));
        assert_eq!(goal_of("no goal line here"), None);
        assert_eq!(goal_of("goal:   "), None, "a blank goal is not a goal");
    }

    #[test]
    fn keeps_only_what_the_history_window_no_longer_shows() {
        let records = vec![turn("old question", "old answer"), turn("recent question", "recent answer")];
        let history = vec!["recent question".to_string(), "recent answer".to_string()];
        let context = recall_context(&records, &history, 1_000).unwrap();
        assert!(context.contains("old question"), "the dropped turn must come back");
        assert!(
            !context.contains("recent question"),
            "a turn the conversation still shows must not be repeated: {context}"
        );
    }

    #[test]
    fn a_fully_redundant_recall_injects_nothing() {
        let records = vec![turn("only question", "only answer")];
        let history = vec!["only question".to_string()];
        assert!(recall_context(&records, &history, 1_000).is_none());
        assert!(recall_context(&[], &history, 1_000).is_none());
        assert!(recall_context(&records, &[], 0).is_none());
    }

    #[test]
    fn the_character_budget_is_respected() {
        let records = vec![turn("a", &"x".repeat(200)), turn("b", &"y".repeat(200))];
        let context = recall_context(&records, &[], 260).unwrap();
        assert!(context.chars().count() < 300, "budget blew up: {}", context.chars().count());
        assert!(context.contains("goal: a"));
        assert!(!context.contains("goal: b"), "the second record must not fit: {context}");
    }

    #[test]
    fn content_without_a_goal_line_is_kept_because_it_cannot_be_deduped() {
        let mut note = MemoryRecord::new(SessionId::new(), MemoryKind::Semantic, "the user prefers tables");
        note.tags = vec!["preference".into()];
        let context = recall_context(&[note], &["anything".into()], 1_000).unwrap();
        assert!(context.contains("prefers tables"));
    }
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
