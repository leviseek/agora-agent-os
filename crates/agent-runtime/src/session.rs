//! The Session Actor: one logical actor per user session.
//!
//! Guarantees:
//!   * messages inside one session are processed strictly in order (single mailbox),
//!   * different sessions run on different actor tasks, so one slow session never blocks another,
//!   * the whole state (transcript, runs, task graphs) is serializable, which is what makes
//!     checkpoint, restore and migration work.

use crate::agent_loop::{history_for_model, AgentLoop, PromptContext};
use crate::context::load_workspace_context;
use crate::compaction::compaction_window;

/// The built-in stand-in provider. Its answers are recorded like any other turn - the user should
/// see exactly what happened - but they are kept out of the history handed to a real model, because
/// a model reading canned placeholder prose copies its phrasing. That is exactly what happened
/// before this filter existed: a real answer came back sounding like the placeholder.
const PLACEHOLDER_PROVIDER: &str = "mock";

/// Does this turn carry the placeholder's own wording, whoever the runtime says wrote it?
pub fn is_placeholder_flavoured(message: &TranscriptMessage) -> bool {
    message.parts.iter().any(|part| match part {
        agentos_core::model::ContentPart::Text { text } => {
            text.contains(agentos_core::PLACEHOLDER_ANSWER_MARKER)
        }
        _ => false,
    })
}
use crate::deltas::DeltaPublisher;
use crate::memory::{episode, recall_context, summary, MemoryStore};
use agentos_actor_runtime::actor::{Actor, ActorContext, ErasedActor, TypedActor};
use agentos_actor_runtime::checkpoint::CheckpointStore;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{
    AgentRun, AgentSpec, EventKind, EventRecord, MemoryKind, MemoryQuery, NewEvent, SessionRecord,
    SessionMessage as TranscriptMessage, TaskGraphRecord, TokenUsage,
};
use agentos_core::state::{AgentRunState, SessionState, StateMachine};
use agentos_core::telemetry::Correlation;
use agentos_core::{now_ms, SessionId};
use agentos_model_router::{ChatMessage, ModelRequest, ModelTask};
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
    /// A goal, optionally with workspace-relative image paths attached and a one-off choice of
    /// provider and thinking effort for this run only.
    UserGoal {
        text: String,
        correlation: Option<Correlation>,
        /// Workspace-relative image paths, read by the runtime through the workspace jail.
        images: Vec<String>,
        /// Artifact ids from an upload (POST /v1/sessions/{id}/attachments). A console cannot write
        /// into the runtime's workspace, so the bytes arrive through the API and are named here.
        #[serde(default)]
        attachments: Vec<String>,
        model: Option<String>,
        reasoning_effort: Option<agentos_core::model::ReasoningEffort>,
    },
    /// Cooperative cancellation of the run currently in flight.
    Cancel { reason: String },
    /// Inspect the session without mutating it.
    Status,
    /// Inspect the most recent run only.
    LastRun,
    /// Read the conversation: user goals and assistant replies, oldest first.
    Transcript { limit: Option<usize> },
    /// Session-level settings. Goes through the actor so its in-memory record cannot drift from
    /// the stored one. A field left as None is unchanged; an empty string clears it.
    Configure {
        title: Option<String>,
        model: Option<String>,
        reasoning_effort: Option<agentos_core::model::ReasoningEffort>,
    },
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
    /// Long-term recall budget: memories considered, and characters injected.
    pub memory_recall_limit: usize,
    pub memory_recall_chars: usize,
    /// Project instruction files read from the workspace at the start of every run.
    pub context_files: Vec<String>,
    pub context_files_chars: usize,
    /// Compaction: whether to summarise dropped turns, and the smallest range worth a model call.
    pub compaction_enabled: bool,
    pub compaction_min_messages: usize,
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
    /// How many transcript entries are already covered by a stored summary. Part of the durable
    /// state on purpose: a restart or a migration must not re-summarise turns it already paid for.
    #[serde(default)]
    pub compacted_through: usize,
    /// What the summaries themselves cost, kept apart from the runs that triggered them.
    #[serde(default)]
    pub compaction_usage: TokenUsage,
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
            compacted_through: 0,
            compaction_usage: TokenUsage::default(),
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

    /// Was this turn written by the built-in placeholder rather than a real model?
    ///
    /// Two ways to be one, and both are needed. The transcript records the run behind each reply,
    /// so a reply from a placeholder-answered run is identified directly. But a real model that
    /// reads a placeholder answer in its history reproduces it almost verbatim - and that copy is
    /// attributed to the real model, so attribution alone would leave it in the history and the
    /// pattern would keep reproducing itself. Hence the content check.
    fn is_placeholder_turn(&self, message: &TranscriptMessage) -> bool {
        if let Some(agent_id) = message.agent_id.as_deref() {
            let answered_by_placeholder = self
                .state
                .runs
                .iter()
                .find(|run| run.id.as_str() == agent_id)
                .map(|run| run.provider.as_deref() == Some(PLACEHOLDER_PROVIDER))
                .unwrap_or(false);
            if answered_by_placeholder {
                return true;
            }
        }
        is_placeholder_flavoured(message)
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

    /// Summarise the turns that left the history window, once, and store the summary as memory.
    ///
    /// Failure is contained: a model that cannot summarise leaves the watermark where it is, so the
    /// next run retries the same range instead of silently forgetting it.
    async fn maybe_compact(&mut self) -> Result<()> {
        if !self.deps.compaction_enabled {
            return Ok(());
        }
        let window = compaction_window(
            self.state.transcript.len(),
            self.state.compacted_through,
            self.deps.history_messages,
            self.deps.compaction_min_messages,
        );
        let Some((start, end)) = window else {
            return Ok(());
        };

        let mut transcript = String::new();
        for message in &self.state.transcript[start..end] {
            let role = message.role.as_str();
            for part in &message.parts {
                if let agentos_core::model::ContentPart::Text { text } = part {
                    transcript.push_str(role);
                    transcript.push_str(": ");
                    transcript.push_str(text);
                    transcript.push('\n');
                }
            }
        }
        if transcript.trim().is_empty() {
            // Nothing readable in that range: move the watermark so it is not rescanned forever.
            self.state.compacted_through = end;
            return Ok(());
        }

        let request = ModelRequest::new(
            ModelTask::Summarize,
            vec![
                ChatMessage::system(
                    "Summarise the conversation below for a later reader. Keep decisions, facts,                      names and open questions; drop pleasantries. Answer with the summary only.",
                ),
                ChatMessage::user(transcript),
            ],
        );
        let response = match self.deps.models.complete(request).await {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(error = %error, "summarisation call failed");
                return Err(error);
            }
        };

        self.state.compaction_usage.record(
            u64::from(response.usage.prompt_tokens),
            u64::from(response.usage.completion_tokens),
            u64::from(response.usage.total_tokens),
        );
        let summary_text = response.content.trim();
        if !summary_text.is_empty() {
            let _ = self
                .deps
                .memory
                .write(summary(
                    self.session_id.clone(),
                    format!("summary of turns {start}..{end}: {summary_text}"),
                ))
                .await;
        }
        self.state.compacted_through = end;
        self.persist_session().await?;

        self.deps
            .bus
            .publish(
                NewEvent::new(EventKind::SessionCompacted, "older turns summarised")
                    .session(self.session_id.clone())
                    .node(self.deps.node_id.clone())
                    .payload(serde_json::json!({
                        "from": start,
                        "to": end,
                        "turns": end - start,
                        "summary_chars": summary_text.chars().count(),
                        "tokens": response.usage.total_tokens,
                        "provider": response.provider,
                    })),
            )
            .await?;
        Ok(())
    }

    /// Recall the memories that the history window does not already show.
    ///
    /// Failure is never fatal: memory is an optimisation on top of the conversation, and a store
    /// that is slow, empty or broken must not stop a goal from running.
    async fn recall_context(&self, history: &[agentos_model_router::ChatMessage]) -> Option<String> {
        if self.deps.memory_recall_limit == 0 || self.deps.memory_recall_chars == 0 {
            return None;
        }
        let query = MemoryQuery {
            session_id: Some(self.session_id.clone()),
            kinds: vec![MemoryKind::Episode],
            limit: self.deps.memory_recall_limit,
            ..Default::default()
        };
        let records = match self.deps.memory.recall(query).await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(error = %error, "memory recall failed; continuing without it");
                return None;
            }
        };
        let history_texts: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
        recall_context(&records, &history_texts, self.deps.memory_recall_chars)
    }

    /// The whole point of the actor: one goal, executed to completion, in order.
    async fn handle_goal(
        &mut self,
        text: String,
        correlation: Option<Correlation>,
        images: Vec<String>,
        attachments: Vec<String>,
        model: Option<String>,
        reasoning_effort: Option<agentos_core::model::ReasoningEffort>,
    ) -> Result<serde_json::Value> {
        // What this run will actually use: the kernel's spec, narrowed by the session's stored
        // choice, narrowed again by anything asked for on this one goal. Computed per run so two
        // runs of one session never fight over shared state.
        let mut spec = self.deps.spec.clone();
        if let Some(hint) = self.state.session.model_hint.clone() {
            spec.model_hint = Some(hint);
        }
        if let Some(effort) = self.state.session.reasoning_effort {
            spec.reasoning_effort = Some(effort);
        }
        if let Some(override_model) = model.as_ref().map(|value| value.trim()).filter(|v| !v.is_empty()) {
            spec.model_hint = Some(override_model.to_string());
        }
        if let Some(override_effort) = reasoning_effort {
            spec.reasoning_effort =
                (override_effort != agentos_core::model::ReasoningEffort::Off).then_some(override_effort);
        }
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

        // Attachments are resolved before the run starts, so a bad path or an id nobody stored
        // fails the goal immediately instead of mid-run. Images come from the workspace or an
        // upload; text files come from an upload, and the two are told apart by content.
        let attached = crate::images::attach_images(
            &self.deps.workspace,
            &self.deps.artifacts,
            &self.session_id,
            &images,
            &attachments,
        )
        .await?;
        let documents = crate::documents::attach_documents(&self.deps.artifacts, &attachments).await?;
        // Every uploaded id must be readable as one or the other. An id that is neither is a client
        // error, and silence would look like the attachment simply had no effect.
        let claimed = attached.parts.len() + documents.len();
        if claimed != attachments.len() {
            return Err(RuntimeError::invalid_input(format!(
                "{claimed} of {} uploaded attachment(s) could be read: an attachment must be an image \
                 or a text file",
                attachments.len()
            )));
        }
        if !attached.parts.is_empty() || !documents.is_empty() {
            self.deps
                .bus
                .publish(
                    NewEvent::new(EventKind::ArtifactCreated, "attachments resolved")
                        .session(self.session_id.clone())
                        .node(self.deps.node_id.clone())
                        .payload(serde_json::json!({
                            "count": attached.parts.len() + documents.len(),
                            // Names for a human reading the log; ids for a client that wants to
                            // render the image without walking the transcript.
                            "names": attached
                                .parts
                                .iter()
                                .filter_map(|part| match part {
                                    agentos_core::model::ContentPart::Image { name, .. }
                                    | agentos_core::model::ContentPart::Artifact { name, .. } => {
                                        Some(name.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>(),
                            "artifact_ids": attached
                                .parts
                                .iter()
                                .filter_map(|part| match part {
                                    agentos_core::model::ContentPart::Image { artifact_id, .. }
                                    | agentos_core::model::ContentPart::Artifact {
                                        artifact_id, ..
                                    } => Some(artifact_id.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>(),
                        })),
                )
                .await?;
        }

        // The transcript records the images and the names of the documents (not their text: a
        // transcript that carries a spreadsheet is a transcript nobody can read).
        let mut all_parts = attached.parts.clone();
        all_parts.extend(documents.iter().map(|document| document.part.clone()));
        let user_message = crate::images::user_message(&self.session_id, &goal, &all_parts);
        let user_message = TranscriptMessage {
            correlation_id: Some(correlation.request()),
            ..user_message
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

        let mut run = AgentRun::new(self.session_id.clone(), &spec, goal.clone());
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

        // Compact before assembling anything else: the summary is then written, recalled and
        // visible in this very turn's prompt rather than a turn late.
        if let Err(error) = self.maybe_compact().await {
            tracing::warn!(error = %error, "compaction failed; the conversation stays as it is");
        }

        // The conversation so far, minus the goal that was just appended: the loop adds the
        // current goal itself, so passing it here would duplicate the newest turn.
        let prior = &self.state.transcript[..self.state.transcript.len().saturating_sub(1)];
        // Placeholder answers are dropped from the model's view: they are not a voice worth
        // imitating, and a real model that reads them answers in their canned style.
        let speakable: Vec<TranscriptMessage> = prior
            .iter()
            .filter(|message| !self.is_placeholder_turn(message))
            .cloned()
            .collect();
        let history = history_for_model(&speakable, self.deps.history_messages, self.deps.history_chars);
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

        // Long-term recall: only what the history window can no longer show survives here, so the
        // model gets older turns without being told the same thing twice.
        let memory_context = self.recall_context(&history).await;
        if let Some(context) = &memory_context {
            self.deps
                .bus
                .publish(
                    NewEvent::new(EventKind::MemoryRecalled, "recalled memory from earlier turns")
                        .session(self.session_id.clone())
                        .agent(run.id.clone())
                        .node(self.deps.node_id.clone())
                        .payload(serde_json::json!({
                            "chars": context.chars().count(),
                            "budget_limit": self.deps.memory_recall_limit,
                            "budget_chars": self.deps.memory_recall_chars,
                        })),
                )
                .await?;
        }

        // Project instructions, read fresh at the start of every run so editing AGENTS.md takes
        // effect on the next goal rather than the next restart.
        let workspace_context =
            load_workspace_context(&self.deps.workspace, &self.deps.context_files, self.deps.context_files_chars);
        if let Some(loaded) = &workspace_context {
            self.deps
                .bus
                .publish(
                    NewEvent::new(EventKind::ContextLoaded, "workspace context loaded")
                        .session(self.session_id.clone())
                        .agent(run.id.clone())
                        .node(self.deps.node_id.clone())
                        .payload(serde_json::json!({
                            "files": loaded.files,
                            "chars": loaded.text.chars().count(),
                            "truncated": loaded.truncated,
                            "budget_chars": self.deps.context_files_chars,
                        })),
                )
                .await?;
        }
        let prompt_context = PromptContext {
            history: &history,
            images: &attached.images,
            documents: &documents,
            workspace: workspace_context.as_ref().map(|loaded| loaded.text.as_str()),
            memory: memory_context.as_deref(),
        };

        // Deltas are published beside the run, never inside it: if the event bus is busy the
        // preview is late, not the answer.
        let deltas = DeltaPublisher::start(
            self.deps.bus.clone(),
            self.session_id.clone(),
            run.id.as_str().to_string(),
            self.deps.node_id.clone(),
        );
        let mut loop_ = AgentLoop::new(self.deps.clone(), self.session_id.clone(), correlation.clone(), token.clone())
            .with_deltas(deltas.sink())
            .with_spec(spec);
        let outcome = match tokio::time::timeout(
            std::time::Duration::from_millis(self.deps.run_timeout_ms.max(1)),
            loop_.run(&mut run, &goal, prompt_context),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::timeout(format!(
                "agent run exceeded {} ms",
                self.deps.run_timeout_ms
            ))),
        };
        // The run is over: stop the ticker and flush whatever the last batch was holding.
        deltas.finish().await;
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

        // One turn record per run, in a shape a reader can parse back: recall extracts the goal
        // line to tell whether the recent-history window already shows this turn.
        let outcome_text = final_answer
            .clone()
            .filter(|answer| !answer.trim().is_empty())
            .unwrap_or_else(|| format!("(no answer: {})", error.clone().unwrap_or_else(|| "unknown".into())));
        let _ = self
            .deps
            .memory
            .write(episode(
                self.session_id.clone(),
                format!("goal: {goal}\nanswer: {outcome_text}"),
                &["session", "turn"],
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
                // What this run was asked for, next to who actually answered: a surprising answer
                // should be traceable to the choice that produced it.
                "model_hint": r.model_hint,
                "reasoning_effort": r.reasoning_effort,
                "final_answer": r.final_answer,
                "error": r.error,
                // Set when the answer came from a fallback rather than the provider that was asked
                // for, so a client can say so instead of presenting it as a clean success.
                "degraded": r.degraded,
                "usage": r.usage,
            })).collect::<Vec<_>>(),
            // Session totals are summed from the runs rather than kept beside them: one source of
            // truth cannot drift, and a run restored from a checkpoint is counted exactly once.
            // Summaries are part of what the session cost, so they are in the total - and shown
            // apart from it, because they are paid for by the session rather than by a run.
            "compaction_usage": self.state.compaction_usage,
            "compacted_through": self.state.compacted_through,
            "usage": self
                .state
                .runs
                .iter()
                .fold(self.state.compaction_usage, |mut total, run| {
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
            SessionMessage::UserGoal { text, correlation, images, attachments, model, reasoning_effort } => {
                self.handle_goal(text, correlation, images, attachments, model, reasoning_effort).await
            }
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
            SessionMessage::Configure { title, model, reasoning_effort } => {
                if let Some(title) = &title {
                    let title = title.trim();
                    if title.is_empty() {
                        return Err(RuntimeError::invalid_input("title must not be empty"));
                    }
                    if title.chars().count() > 200 {
                        return Err(RuntimeError::invalid_input("title is limited to 200 characters"));
                    }
                    self.state.session.title = title.to_string();
                }
                if let Some(model) = &model {
                    // An empty string clears the choice, which is how a client says "let the router
                    // decide again" without a second endpoint.
                    let model = model.trim();
                    self.state.session.model_hint = (!model.is_empty()).then(|| model.to_string());
                }
                if let Some(effort) = reasoning_effort {
                    self.state.session.reasoning_effort =
                        (effort != agentos_core::model::ReasoningEffort::Off).then_some(effort);
                }
                self.state.session.updated_at = now_ms();
                self.persist_session().await?;
                self.deps
                    .bus
                    .publish(
                        NewEvent::new(EventKind::SessionRenamed, "session settings changed")
                            .session(self.session_id.clone())
                            .node(self.deps.node_id.clone())
                            .payload(serde_json::json!({
                                "title": self.state.session.title,
                                "model": self.state.session.model_hint,
                                "reasoning_effort": self.state.session.reasoning_effort,
                            })),
                    )
                    .await?;
                Ok(serde_json::json!({
                    "session_id": self.session_id.as_str(),
                    "title": self.state.session.title,
                    "model": self.state.session.model_hint,
                    "reasoning_effort": self.state.session.reasoning_effort,
                }))
            }
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

#[cfg(test)]
mod placeholder_filter_tests {
    use super::*;
    use agentos_core::model::{ContentPart, MessageRole, SessionMessage};
    use agentos_core::{MessageId, SessionId};

    fn turn(role: MessageRole, text: &str) -> SessionMessage {
        SessionMessage {
            id: MessageId::new(),
            session_id: SessionId::new(),
            role,
            parts: vec![ContentPart::Text { text: text.to_string() }],
            created_at: agentos_core::now_ms(),
            correlation_id: None,
            agent_id: None,
        }
    }

    #[test]
    fn the_placeholders_wording_is_recognised_whoever_is_credited_with_it() {
        // A real model that reads a placeholder answer reproduces it almost word for word. That
        // copy is credited to the real model, so only a content check catches it - and if it is
        // not caught, the history keeps teaching the next run to answer the same way.
        let copied = turn(
            MessageRole::Assistant,
            "Acknowledged: 你好. No capability was required, so this is the final answer.",
        );
        assert!(is_placeholder_flavoured(&copied));
    }

    #[test]
    fn a_real_answer_is_left_alone() {
        let real = turn(
            MessageRole::Assistant,
            "21*2 = 42. I used the calculator capability to be sure.",
        );
        assert!(!is_placeholder_flavoured(&real));
        let question = turn(MessageRole::User, "what is 21*2?");
        assert!(!is_placeholder_flavoured(&question), "user turns are never filtered");
    }
}
