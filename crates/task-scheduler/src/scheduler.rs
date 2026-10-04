//! Dependency-aware parallel scheduler with retries, timeouts and cancellation.

use crate::graph::topological_order;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{EventFilter, EventKind, EventRecord, NewEvent, TaskGraphRecord, TaskRecord};
use agentos_core::state::{StateMachine, TaskState};
use agentos_core::telemetry::{metric_names, metrics, Correlation};
use agentos_core::{now_ms, SessionId, TaskId};
use agentos_event_bus::EventBus;
use agentos_storage::store::{collections, Collection, Store};
use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub max_concurrency: usize,
    pub default_max_attempts: u32,
    pub default_timeout_ms: u64,
    pub retry_backoff_ms: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self { max_concurrency: 8, default_max_attempts: 2, default_timeout_ms: 30_000, retry_backoff_ms: 50 }
    }
}

/// Everything a runner needs. Note that outputs of dependencies are passed by value: a task
/// never reaches back into the scheduler.
#[derive(Debug, Clone)]
pub struct TaskContext {
    pub session_id: SessionId,
    pub graph_id: TaskId,
    pub task_id: TaskId,
    pub attempt: u32,
    pub correlation: Correlation,
    pub cancellation: CancellationToken,
    pub dependency_outputs: BTreeMap<TaskId, serde_json::Value>,
}

impl TaskContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

/// Executes one task. Implementations are stateless with respect to the scheduler.
#[async_trait]
pub trait TaskRunner: Send + Sync + 'static {
    async fn run(&self, node: &TaskRecord, ctx: TaskContext) -> Result<serde_json::Value>;

    /// Called before a retry, so external state can be reset.
    async fn on_retry(&self, _node: &TaskRecord, _attempt: u32) -> Result<()> {
        Ok(())
    }

    /// Called when a task is cancelled (cancellation cascade or shutdown).
    async fn on_cancel(&self, _node: &TaskRecord) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct GraphOutcome {
    pub graph_id: TaskId,
    pub session_id: SessionId,
    pub state: TaskState,
    pub nodes: Vec<TaskRecord>,
    pub duration_ms: u64,
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
}

pub struct Scheduler {
    store: Arc<dyn Store>,
    bus: Arc<dyn EventBus>,
    runner: Arc<dyn TaskRunner>,
    cfg: SchedulerConfig,
}

impl Scheduler {
    pub fn new(
        store: Arc<dyn Store>,
        bus: Arc<dyn EventBus>,
        runner: Arc<dyn TaskRunner>,
        cfg: SchedulerConfig,
    ) -> Self {
        Self { store, bus, runner, cfg }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.cfg
    }

    /// Run a graph to completion. Sessions and graphs never block each other: only max_concurrency
    /// bounds how many nodes of THIS graph run at once.
    pub async fn run(&self, graph: TaskGraphRecord) -> Result<GraphOutcome> {
        self.run_with_cancel(graph, CancellationToken::new()).await
    }

    pub async fn run_with_cancel(
        &self,
        graph: TaskGraphRecord,
        cancellation: CancellationToken,
    ) -> Result<GraphOutcome> {
        let started = now_ms();
        let graph_id = graph.id.clone();
        let session_id = graph.session_id.clone();
        let title = graph.title.clone();

        // Fail fast on malformed graphs.
        topological_order(&graph.nodes)?;

        let mut nodes: BTreeMap<TaskId, TaskRecord> = graph
            .nodes
            .iter()
            .map(|n| {
                let mut n = n.clone();
                if n.max_attempts == 0 {
                    n.max_attempts = self.cfg.default_max_attempts;
                }
                if n.timeout_ms == 0 {
                    n.timeout_ms = self.cfg.default_timeout_ms;
                }
                (n.id.clone(), n)
            })
            .collect();

        self.bus
            .publish(
                NewEvent::new(EventKind::TaskCreated, format!("task graph queued: {title}"))
                    .session(session_id.clone())
                    .task(graph_id.clone())
                    .payload(serde_json::json!({ "nodes": nodes.len() })),
            )
            .await?;

        for node in nodes.values_mut() {
            node.state = node.state.transition(TaskState::Ready).unwrap_or(node.state);
        }
        self.persist_graph(&graph_id, &session_id, &nodes).await?;

        let mut running: JoinSet<(TaskId, Result<serde_json::Value>, u64)> = JoinSet::new();
        let mut retry_after: HashMap<TaskId, u64> = HashMap::new();
        let mut cancelled_any = false;

        loop {
            // 1. Cascade: nodes whose dependencies can never succeed are cancelled.
            let terminal_failed: Vec<TaskId> = nodes
                .values()
                .filter(|n| matches!(n.state, TaskState::Failed | TaskState::Cancelled))
                .map(|n| n.id.clone())
                .collect();
            let mut newly_cancelled = Vec::new();
            for node in nodes.values_mut() {
                if !node.state.is_terminal() && node.deps.iter().any(|d| terminal_failed.contains(d)) {
                    if let Ok(next) = node.state.transition(TaskState::Cancelled) {
                        node.state = next;
                        node.finished_at = Some(now_ms());
                        node.error = Some("dependency failed or was cancelled".into());
                        newly_cancelled.push(node.id.clone());
                    }
                }
            }
            for id in newly_cancelled {
                self.persist_node(&nodes[&id]).await?;
                self.bus
                    .publish(
                        NewEvent::new(EventKind::TaskCancelled, "task cancelled by dependency cascade")
                            .session(session_id.clone())
                            .task(id.clone())
                            .graph_event(&graph_id),
                    )
                    .await?;
            }

            // 2. Fill the execution window.
            while running.len() < self.cfg.max_concurrency {
                let now = now_ms();
                let next = nodes
                    .values()
                    .find(|n| {
                        matches!(n.state, TaskState::Ready | TaskState::Retrying)
                            && n.deps.iter().all(|d| {
                                nodes.get(d).map(|x| x.state == TaskState::Succeeded).unwrap_or(false)
                            })
                            && retry_after.get(&n.id).map(|t| *t <= now).unwrap_or(true)
                    })
                    .cloned();
                let Some(mut node) = next else { break };
                if node.state == TaskState::Retrying {
                    node.state = node.state.transition(TaskState::Running).unwrap_or(node.state);
                } else {
                    node.state = node.state.transition(TaskState::Running).unwrap_or(node.state);
                }
                node.attempts += 1;
                node.started_at = Some(now_ms());
                let attempt = node.attempts;
                nodes.insert(node.id.clone(), node.clone());
                self.persist_node(&node).await?;
                self.bus
                    .publish(
                        NewEvent::new(EventKind::TaskStarted, format!("task started: {}", node.title))
                            .session(session_id.clone())
                            .task(node.id.clone())
                            .payload(serde_json::json!({ "attempt": attempt, "kind": node.kind.as_str() }))
                            .graph_event(&graph_id),
                    )
                    .await?;

                let dependency_outputs: BTreeMap<TaskId, serde_json::Value> = node
                    .deps
                    .iter()
                    .filter_map(|d| nodes.get(d).and_then(|n| n.result.clone().map(|r| (d.clone(), r))))
                    .collect();
                let ctx = TaskContext {
                    session_id: session_id.clone(),
                    graph_id: graph_id.clone(),
                    task_id: node.id.clone(),
                    attempt,
                    correlation: Correlation::new().with_session(&session_id).with_task(&node.id),
                    cancellation: cancellation.clone(),
                    dependency_outputs,
                };
                let runner = self.runner.clone();
                let timeout = Duration::from_millis(node.timeout_ms);
                running.spawn(async move {
                    let started = now_ms();
                    let fut = runner.run(&node, ctx);
                    let result = match tokio::time::timeout(timeout, fut).await {
                        Ok(r) => r,
                        Err(_) => Err(RuntimeError::timeout(format!(
                            "task {} exceeded {} ms",
                            node.id, node.timeout_ms
                        ))),
                    };
                    (node.id.clone(), result, now_ms().saturating_sub(started))
                });
            }

            if running.is_empty() {
                break;
            }

            // 3. Wait for the next completion, but stay responsive to cancellation.
            let joined = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    cancelled_any = true;
                    None
                }
                r = running.join_next() => r,
            };

            let Some(joined) = joined else {
                running.abort_all();
                break;
            };

            let (task_id, result, elapsed) = match joined {
                Ok(v) => v,
                Err(join_err) => {
                    tracing::error!(error = %join_err, "task runner crashed");
                    continue;
                }
            };

            let Some(node) = nodes.get_mut(&task_id) else { continue };
            metrics().observe(metric_names::TASK_LATENCY_MS, elapsed as f64);
            match result {
                Ok(output) => {
                    node.state = node.state.transition(TaskState::Succeeded).unwrap_or(TaskState::Succeeded);
                    node.result = Some(output);
                    node.error = None;
                    node.finished_at = Some(now_ms());
                    metrics().inc_by(metric_names::TASK_STATE, &[("state", "succeeded")], 1);
                    self.persist_node(node).await?;
                    self.bus
                        .publish(
                            NewEvent::new(EventKind::TaskCompleted, format!("task completed: {}", node.title))
                                .session(session_id.clone())
                                .task(task_id.clone())
                                .payload(serde_json::json!({ "duration_ms": elapsed }))
                                .graph_event(&graph_id),
                        )
                        .await?;
                }
                Err(err) => {
                    if err.kind == agentos_core::ErrorKind::Cancelled {
                        node.state = node.state.transition(TaskState::Cancelled).unwrap_or(TaskState::Cancelled);
                        node.error = Some(err.to_string());
                        node.finished_at = Some(now_ms());
                        let _ = self.runner.on_cancel(node).await;
                        self.persist_node(node).await?;
                        self.bus
                            .publish(
                                NewEvent::new(EventKind::TaskCancelled, format!("task cancelled: {}", node.title))
                                    .warn()
                                    .session(session_id.clone())
                                    .task(task_id.clone())
                                    .graph_event(&graph_id),
                            )
                            .await?;
                        continue;
                    }
                    let retryable = err.is_retryable() && node.attempts < node.max_attempts;
                    if retryable {
                        node.state = node.state.transition(TaskState::Retrying).unwrap_or(TaskState::Retrying);
                        node.error = Some(err.to_string());
                        let _ = self.runner.on_retry(node, node.attempts).await;
                        let backoff = self.cfg.retry_backoff_ms * node.attempts as u64;
                        retry_after.insert(task_id.clone(), now_ms() + backoff);
                        metrics().inc_by(metric_names::TASK_STATE, &[("state", "retrying")], 1);
                        self.persist_node(node).await?;
                        self.bus
                            .publish(
                                NewEvent::new(EventKind::TaskRetrying, format!("task retrying: {}", node.title))
                                    .warn()
                                    .session(session_id.clone())
                                    .task(task_id.clone())
                                    .payload(serde_json::json!({
                                        "attempt": node.attempts,
                                        "max_attempts": node.max_attempts,
                                        "error": err.to_string(),
                                        "backoff_ms": backoff,
                                    }))
                                    .graph_event(&graph_id),
                            )
                            .await?;
                    } else {
                        node.state = node.state.transition(TaskState::Failed).unwrap_or(TaskState::Failed);
                        node.error = Some(err.to_string());
                        node.finished_at = Some(now_ms());
                        metrics().inc_by(metric_names::TASK_STATE, &[("state", "failed")], 1);
                        self.persist_node(node).await?;
                        self.bus
                            .publish(
                                NewEvent::new(EventKind::TaskFailed, format!("task failed: {}", node.title))
                                    .error()
                                    .session(session_id.clone())
                                    .task(task_id.clone())
                                    .payload(serde_json::json!({
                                        "attempts": node.attempts,
                                        "error": err.to_string(),
                                        "code": err.code(),
                                    }))
                                    .graph_event(&graph_id),
                            )
                            .await?;
                    }
                }
            }
        }

        if cancelled_any {
            for node in nodes.values_mut() {
                if !node.state.is_terminal() {
                    node.state = node.state.transition(TaskState::Cancelled).unwrap_or(node.state);
                    node.finished_at = Some(now_ms());
                    node.error = Some("graph cancelled".into());
                }
            }
        }

        let nodes_vec: Vec<TaskRecord> = nodes.values().cloned().collect();
        let failed = nodes_vec.iter().filter(|n| n.state == TaskState::Failed).count();
        let cancelled = nodes_vec.iter().filter(|n| n.state == TaskState::Cancelled).count();
        let succeeded = nodes_vec.iter().filter(|n| n.state == TaskState::Succeeded).count();
        let state = if cancelled > 0 {
            TaskState::Cancelled
        } else if failed > 0 {
            TaskState::Failed
        } else {
            TaskState::Succeeded
        };

        self.persist_graph(&graph_id, &session_id, &nodes).await?;
        Ok(GraphOutcome {
            graph_id,
            session_id,
            state,
            nodes: nodes_vec,
            duration_ms: now_ms().saturating_sub(started),
            succeeded,
            failed,
            cancelled,
        })
    }

    /// Replay the durable event history of a graph. Useful for the UI and for post-mortems.
    pub async fn graph_events(&self, graph_id: &TaskId, limit: usize) -> Result<Vec<EventRecord>> {
        let window = limit.max(1000);
        let events = self.bus.replay(EventFilter { limit: window, ..Default::default() }).await?;
        Ok(events
            .into_iter()
            .filter(|e| {
                e.payload.get("graph_id").and_then(|v| v.as_str()) == Some(graph_id.as_str())
            })
            .take(limit)
            .collect())
    }

    async fn persist_node(&self, node: &TaskRecord) -> Result<()> {
        let collection: Collection<TaskRecord> = Collection::new(collections::TASKS);
        collection.save(self.store.as_ref(), node.id.as_str(), node).await
    }

    async fn persist_graph(
        &self,
        graph_id: &TaskId,
        session_id: &SessionId,
        nodes: &BTreeMap<TaskId, TaskRecord>,
    ) -> Result<()> {
        let mut graph = TaskGraphRecord::new(session_id.clone(), "graph");
        graph.id = graph_id.clone();
        graph.nodes = nodes.values().cloned().collect();
        graph.state = if nodes.values().any(|n| n.state == TaskState::Failed) {
            TaskState::Failed
        } else if nodes.values().all(|n| n.state.is_terminal()) {
            TaskState::Succeeded
        } else {
            TaskState::Running
        };
        graph.updated_at = now_ms();
        let collection: Collection<TaskGraphRecord> = Collection::new(collections::GRAPHS);
        collection.save(self.store.as_ref(), graph_id.as_str(), &graph).await
    }
}

/// Merges graph_id into an event payload so a whole graph can be reconstructed from the log.
trait GraphEventExt {
    fn graph_event(self, graph_id: &TaskId) -> Self;
}

impl GraphEventExt for NewEvent {
    fn graph_event(mut self, graph_id: &TaskId) -> Self {
        let mut map = match self.payload {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        map.insert("graph_id".into(), serde_json::json!(graph_id.as_str()));
        self.payload = serde_json::Value::Object(map);
        self
    }
}
