//! The Session Actor: one logical actor per user session.
//!
//! Guarantees:
//!   * messages inside one session are processed strictly in order (single mailbox),
//!   * different sessions run on different actor tasks, so one slow session never blocks another,
//!   * the whole state (transcript, runs, task graphs) is serializable, which is what makes
//!     checkpoint, restore and migration work.

use crate::agent_loop::{history_for_model, AgentLoop};
use crate::memory::{episode, MemoryStore};
use agentos_actor_runtime::actor::{Actor, ActorContext, ErasedActor, TypedActor};
use agentos_actor_runtime::checkpoint::CheckpointStore;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{
    AgentRun, AgentSpec, EventKind, EventRecord, NewEvent, SessionMessage as TranscriptMessage,
    TokenUsage,
    SessionRecord, TaskGraphRecord,
};
use agentos_core::state::{AgentRunState, SessionState, StateMachine};
use agentos_core::telemetry::Correlation;
use agentos_core::{now_ms, SessionId};
use agentos_event_bus::EventBus;
use agentos_model_router::ModelRouter;
use agentos_capability_runtime::mesh::CapabilityMesh;
use agentos_capability_runtime::workspace::Workspace;
use agentos_storage::artifact::ArtifactStore;
use agentos_storage::store::{collections, Collection, Store};
use agentos_task_scheduler::scheduler::Scheduler;
use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Messages accepted by a session actor. Typed on purpose: the mailbox is the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionMessage {
    /// The user's goal. Runs the agent loop to completion.
    UserGoal { text: String, correlation: Option<Correlation> },
    /// Cooperative cancellation of the run currently in flight.
    Cancel { reason: String },
    /// Inspect the session without mutating it.
    Status,
    /// Inspect the most recent run only.
    LastRun,
    /// Read the conversation: user goals and assistant replies, oldest first.
    Transcript { limit: Option<usize> },
}

/// Everything the session actor is allowed to use. All of it is an interface.
pub struct SessionDeps {
    pub store: Arc<dyn Store>,
    pub bus: Arc<dyn EventBus>,
    pub models: Arc<ModelRouter>,
    pub mesh: Arc<CapabilityMesh>,
    pub scheduler: Arc<Scheduler>,
    pub memory: Arc<dyn MemoryStore>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub checkpoints: Arc<dyn CheckpointStore>,
    pub workspace: Arc<Workspace>,
    pub spec: AgentSpec,
    pub node_id: String,
    pub run_timeout_ms: u64,
    /// Conversation budget for model requests (see PolicyConfig history_messages).
    pub history_messages: usize,
    pub history_chars: usize,
    /// Live cancellation tokens per session. Deliberately OUTSIDE the actor mailbox: cancelling a
    /// run must not queue behind the run that is being cancelled.
    pub run_tokens: Arc<RwLock<std::collections::HashMap<SessionId, CancellationToken>>>,
}

/// Serializable actor state. Everything here survives a checkpoint, a restart and a migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionActorState {
    pub session: SessionRecord,
    pub transcript: Vec<TranscriptMessage>,
    pub runs: Vec<AgentRun>,
    pub graphs: Vec<TaskGraphRecord>,
    pub active_run: Option<String>,
    /// Total goals handled by this actor instance.
    pub goals_handled: u64,
    pub restored_generation: u64,
}

impl SessionActorState {
    pub fn new(session: SessionRecord) -> Self {
        Self {
            session,
            transcript: vec![],
            runs: vec![],
            graphs: vec![],
            active_run: None,
            goals_handled: 0,
            restored_generation: 0,
        }
    }

    pub fn last_run(&self) -> Option<&AgentRun> {
        self.runs.last()
    }
}

pub struct SessionActor {
    deps: Arc<SessionDeps>,
    state: SessionActorState,
    session_id: SessionId,
}

impl SessionActor {
    pub fn new(deps: Arc<SessionDeps>, state: SessionActorState) -> Self {
        let session_id = state.session.id.clone();
        Self { deps, state, session_id }
    }

    pub fn state_ref(&self) -> &SessionActorState {
        &self.state
    }

    fn persist_session(&self) -> impl std::future::Future<Output = Result<()>> + '_ {
        let store = self.deps.store.clone();
        let record = self.state.session.clone();
        async move {
            let collection: Collection<SessionRecord> = Collection::new(collections::SESSIONS);
            collection.save(store.as_ref(), record.id.as_str(), &record).await
        }
    }

    async fn persist_run(&self, run: &AgentRun) -> Result<()> {
        let collection: Collection<AgentRun> = Collection::new(collections::RUNS);
        collection.save(self.deps.store.as_ref(), run.id.as_str(), run).await
    }

    fn set_session_state(&mut self, next: SessionState) -> Result<()> {
        self.state.session.state = self
            .state
            .session
            .state
            .transition(next)
            .map_err(|e| RuntimeError::conflict(format!("session {}: {e}", self.session_id)))?;
        self.state.session.updated_at = now_ms();
        Ok(())
    }

    /// The whole point of the actor: one goal, executed to completion, in order.
    async fn handle_goal(&mut self, text: String, correlation: Option<Correlation>) -> Result<serde_json::Value> {
        // Untrusted input is trimmed and size-bounded before it becomes part of the transcript.
        let goal = text.trim().to_string();
        if goal.is_empty() {
            return Err(RuntimeError::invalid_input("goal must not be empty"));
        }
        if goal.len() > 16 * 1024 {
            return Err(RuntimeError::invalid_input("goal exceeds 16 KiB"));
        }

        let correlation = correlation.unwrap_or_else(|| Correlation::new().with_session(&self.session_id));
        self.set_session_state(SessionState::Active)?;
        self.state.session.message_count += 1;

        let user_message = TranscriptMessage {
            id: agentos_core::MessageId::new(),
            session_id: self.session_id.clone(),
            role: agentos_core::model::MessageRole::User,
            parts: vec![agentos_core::model::ContentPart::Text { text: goal.clone() }],
            created_at: now_ms(),
            correlation_id: Some(correlation.request()),
            agent_id: None,
        };
        self.state.transcript.push(user_message.clone());
        self.persist_session().await?;

        self.deps
            .bus
            .publish(
                NewEvent::new(EventKind::SessionMessageQueued, "user goal accepted")
                    .session(self.session_id.clone())
                    .node(self.deps.node_id.clone())
                    .payload(serde_json::json!({ "chars": goal.len() })),
            )
            .await?;

        let mut run = AgentRun::new(self.session_id.clone(), &self.deps.spec, goal.clone());
        self.state.active_run = Some(run.id.as_str().to_string());
        self.persist_run(&run).await?;
        self.deps
            .bus
            .publish(
                NewEvent::new(EventKind::RunCreated, "agent run created")
                    .session(self.session_id.clone())
                    .agent(run.id.clone())
                    .node(self.deps.node_id.clone())
                    .payload(serde_json::json!({ "goal": goal })),
            )
            .await?;

        let token = CancellationToken::new();
        self.deps
            .run_tokens
            .write()
            .insert(self.session_id.clone(), token.clone());

        // The conversation so far, minus the goal that was just appended: the loop adds the
        // current goal itself, so passing it here would duplicate the newest turn.
        let prior = &self.state.transcript[..self.state.transcript.len().saturating_sub(1)];
        let history = history_for_model(prior, self.deps.history_messages, self.deps.history_chars);
        self.deps
            .bus
            .publish(
                NewEvent::new(EventKind::AgentStep, "conversation history assembled")
                    .session(self.session_id.clone())
                    .agent(run.id.clone())
                    .node(self.deps.node_id.clone())
                    .payload(serde_json::json!({
                        "history_messages": history.len(),
                        "transcript_messages": prior.len(),
                        "budget_messages": self.deps.history_messages,
                        "budget_chars": self.deps.history_chars,
                    })),
            )
            .await?;

        let mut loop_ = AgentLoop::new(self.deps.clone(), self.session_id.clone(), correlation.clone(), token.clone());
        let outcome = match tokio::time::timeout(
            std::time::Duration::from_millis(self.deps.run_timeout_ms.max(1)),
            loop_.run(&mut run, &goal, &history),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::timeout(format!(
                "agent run exceeded {} ms",
                self.deps.run_timeout_ms
            ))),
        };
        self.deps.run_tokens.write().remove(&self.session_id);
        let finished = now_ms();

        let (final_answer, error) = match outcome {
            Ok(outcome) => (Some(outcome.answer.clone()), None),
            Err(e) => {
                let cancelled = e.kind == agentos_core::ErrorKind::Cancelled;
                run.state = run
                    .state
                    .transition(if cancelled { AgentRunState::Cancelled } else { AgentRunState::Failed })
                    .unwrap_or(run.state);
                (None, Some(e.to_string()))
            }
        };

        run.updated_at = finished;
        if run.is_finished() {
            run.finished_at = Some(finished);
        } else if final_answer.is_some() {
            run.state = run.state.transition(AgentRunState::Finalizing).unwrap_or(run.state);
            run.state = run.state.transition(AgentRunState::Succeeded).unwrap_or(run.state);
            run.finished_at = Some(finished);
        }
        run.final_answer = final_answer.clone();
        run.error = error.clone();
        self.state.active_run = None;
        self.state.goals_handled += 1;
        self.state.runs.push(run.clone());
        self.persist_run(&run).await?;

        if let Some(answer) = &final_answer {
            self.state.transcript.push(TranscriptMessage {
                id: agentos_core::MessageId::new(),
                session_id: self.session_id.clone(),
                role: agentos_core::model::MessageRole::Assistant,
                parts: vec![agentos_core::model::ContentPart::Text { text: answer.clone() }],
                created_at: now_ms(),
                correlation_id: Some(correlation.request()),
                agent_id: Some(run.id.as_str().to_string()),
            });
            self.deps
                .bus
                .publish(
                    NewEvent::new(EventKind::SessionMessageHandled, "assistant reply recorded")
                        .session(self.session_id.clone())
                        .agent(run.id.clone())
                        .node(self.deps.node_id.clone()),
                )
                .await?;
        }

        self.deps
            .bus
            .publish(
                NewEvent::new(
                    if error.is_none() { EventKind::RunCompleted } else { EventKind::RunFailed },
                    if error.is_none() { "agent run completed" } else { "agent run failed" },
                )
                .session(self.session_id.clone())
                .agent(run.id.clone())
                .node(self.deps.node_id.clone())
                .payload(serde_json::json!({
                    "steps": run.steps.len(),
                    "state": run.state.as_str(),
                    "error": error,
                    "duration_ms": finished.saturating_sub(run.created_at),
                })),
            )
            .await?;

        let _ = self
            .deps
            .memory
            .write(episode(
                self.session_id.clone(),
                format!("goal: {goal}\nresult: {}", final_answer.clone().unwrap_or_default()),
                &["session", "goal"],
            ))
            .await;

        self.set_session_state(SessionState::Idle)?;
        self.persist_session().await?;

        Ok(serde_json::json!({
            "session_id": self.session_id.as_str(),
            "agent_id": run.id.as_str(),
            "state": run.state.as_str(),
            "answer": final_answer,
            "error": error,
            "steps": run.steps.len(),
        }))
    }

    fn status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "session_id": self.session_id.as_str(),
            "state": self.state.session.state.as_str(),
            "title": self.state.session.title,
            "messages": self.state.transcript.len(),
            "goals_handled": self.state.goals_handled,
            "runs": self.state.runs.iter().map(|r| serde_json::json!({
                "agent_id": r.id.as_str(),
                "state": r.state.as_str(),
                "goal": r.goal,
                "steps": r.steps.len(),
                "provider": r.provider,
                "model": r.model,
                "final_answer": r.final_answer,
                "error": r.error,
                "usage": r.usage,
            })).collect::<Vec<_>>(),
            // Session totals are summed from the runs rather than kept beside them: one source of
            // truth cannot drift, and a run restored from a checkpoint is counted exactly once.
            "usage": self
                .state
                .runs
                .iter()
                .fold(TokenUsage::default(), |mut total, run| {
                    total.add(&run.usage);
                    total
                }),
            "graphs": self.state.graphs.iter().map(|g| serde_json::json!({
                "graph_id": g.id.as_str(),
                "title": g.title,
                "nodes": g.nodes.len(),
                "succeeded": g.succeeded(),
                "failed": g.failed(),
            })).collect::<Vec<_>>(),
        })
    }
}

#[async_trait]
impl Actor for SessionActor {
    type Message = SessionMessage;

    fn kind(&self) -> &'static str {
        "session"
    }

    fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    fn state(&self) -> serde_json::Value {
        serde_json::to_value(&self.state).unwrap_or(serde_json::Value::Null)
    }

    fn restore_state(&mut self, state: serde_json::Value) -> Result<()> {
        let restored: SessionActorState = serde_json::from_value(state)
            .map_err(|e| RuntimeError::migration(format!("cannot restore session actor state: {e}")))?;
        if restored.session.id != self.session_id {
            return Err(RuntimeError::migration(format!(
                "snapshot belongs to session {}, not {}",
                restored.session.id, self.session_id
            )));
        }
        self.state = restored;
        self.state.restored_generation += 1;
        Ok(())
    }

    async fn on_start(&mut self, ctx: &ActorContext) -> Result<()> {
        tracing::info!(
            session = %self.session_id,
            actor = %ctx.actor_id,
            generation = ctx.generation,
            "session actor started"
        );
        Ok(())
    }

    async fn on_stop(&mut self, _ctx: &ActorContext) -> Result<()> {
        let mut session = self.state.session.clone();
        if !session.state.is_terminal() {
            session.state = session.state.transition(SessionState::Closing).unwrap_or(session.state);
            session.state = session.state.transition(SessionState::Closed).unwrap_or(session.state);
            session.closed_at = Some(now_ms());
            self.state.session = session;
        }
        self.persist_session().await
    }

    /// Replay: re-apply historical events. Session actors keep the durable truth in the store and
    /// the transcript, so replay only needs to re-derive counters.
    async fn on_replay(&mut self, event: &EventRecord) -> Result<()> {
        match event.kind {
            EventKind::SessionMessageQueued => {
                self.state.goals_handled = self.state.goals_handled.saturating_add(1);
            }
            EventKind::SnapshotRestored => {
                self.state.restored_generation = self.state.restored_generation.saturating_add(1);
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle(&mut self, message: SessionMessage, _ctx: &ActorContext) -> Result<serde_json::Value> {
        match message {
            SessionMessage::UserGoal { text, correlation } => self.handle_goal(text, correlation).await,
            SessionMessage::Cancel { reason } => {
                let cancelled = match self.deps.run_tokens.read().get(&self.session_id) {
                    Some(token) => {
                        token.cancel();
                        true
                    }
                    None => false,
                };
                Ok(serde_json::json!({ "cancelled": cancelled, "reason": reason }))
            }
            SessionMessage::Status => Ok(self.status_json()),
            SessionMessage::LastRun => Ok(self
                .state
                .last_run()
                .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::Value::Null)),
            SessionMessage::Transcript { limit } => {
                // Newest-last, so a client can append without re-sorting. A limit keeps a long
                // conversation from being shipped whole on every poll.
                let all = &self.state.transcript;
                let start = match limit {
                    Some(limit) if limit > 0 && all.len() > limit => all.len() - limit,
                    _ => 0,
                };
                Ok(serde_json::json!({
                    "messages": &all[start..],
                    "total": all.len(),
                    "truncated": start > 0,
                }))
            }
        }
    }
}

/// Adapts a session actor into the erased form the actor runtime drives.
pub struct SessionActorHandle {
    pub inner: TypedActor<SessionActor>,
}

impl SessionActorHandle {
    pub fn boot(deps: Arc<SessionDeps>, state: SessionActorState) -> Self {
        Self { inner: TypedActor::new(SessionActor::new(deps, state)) }
    }

    pub fn into_erased(self) -> Box<dyn ErasedActor> {
        Box::new(self.inner)
    }
}
