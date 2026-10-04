use crate::ids::{AgentId, SessionId, TaskId};
use crate::state::TaskState;
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Invoke a capability by name.
    Capability,
    /// Ask a model to reason.
    Model,
    /// Nested agent run.
    Agent,
    /// Aggregation node: joins the output of its dependencies.
    Join,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Capability => "capability",
            TaskKind::Model => "model",
            TaskKind::Agent => "agent",
            TaskKind::Join => "join",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskPayload {
    Capability { capability: String, version: Option<String>, input: serde_json::Value },
    Model { prompt: String, model_hint: Option<String> },
    Agent { goal: String, spec_name: String },
    Join { template: String },
}

/// A node of a task graph. Dependencies are expressed as TaskIds, which makes the graph a DAG
/// that can be scheduled, checkpointed and replayed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub graph_id: TaskId,
    pub session_id: SessionId,
    pub agent_id: Option<AgentId>,
    pub title: String,
    pub kind: TaskKind,
    pub payload: TaskPayload,
    pub deps: Vec<TaskId>,
    pub state: TaskState,
    pub attempts: u32,
    pub max_attempts: u32,
    pub timeout_ms: u64,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub labels: BTreeMap<String, String>,
}

impl TaskRecord {
    pub fn new(graph_id: TaskId, session_id: SessionId, title: impl Into<String>, kind: TaskKind, payload: TaskPayload) -> Self {
        Self {
            id: TaskId::new(),
            graph_id,
            session_id,
            agent_id: None,
            title: title.into(),
            kind,
            payload,
            deps: Vec::new(),
            state: TaskState::Pending,
            attempts: 0,
            max_attempts: 2,
            timeout_ms: 30_000,
            result: None,
            error: None,
            created_at: crate::now_ms(),
            started_at: None,
            finished_at: None,
            labels: BTreeMap::new(),
        }
    }

    pub fn with_deps(mut self, deps: Vec<TaskId>) -> Self {
        self.deps = deps;
        self
    }

    pub fn with_max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    pub fn duration_ms(&self) -> u64 {
        match (self.started_at, self.finished_at) {
            (Some(s), Some(f)) => f.saturating_sub(s),
            (Some(s), None) => crate::now_ms().saturating_sub(s),
            _ => 0,
        }
    }
}

/// Public alias so callers do not need to import the record name for graph nodes.
pub type TaskNode = TaskRecord;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGraphRecord {
    pub id: TaskId,
    pub session_id: SessionId,
    pub agent_id: Option<AgentId>,
    pub title: String,
    pub state: TaskState,
    pub nodes: Vec<TaskRecord>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl TaskGraphRecord {
    pub fn new(session_id: SessionId, title: impl Into<String>) -> Self {
        let now = crate::now_ms();
        Self {
            id: TaskId::new(),
            session_id,
            agent_id: None,
            title: title.into(),
            state: TaskState::Pending,
            nodes: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn node(&self, id: &TaskId) -> Option<&TaskRecord> {
        self.nodes.iter().find(|n| &n.id == id)
    }

    pub fn add(&mut self, node: TaskRecord) {
        self.nodes.push(node);
        self.updated_at = crate::now_ms();
    }

    pub fn succeeded(&self) -> usize {
        self.nodes.iter().filter(|n| n.state == TaskState::Succeeded).count()
    }

    pub fn failed(&self) -> usize {
        self.nodes.iter().filter(|n| n.state == TaskState::Failed).count()
    }
}
