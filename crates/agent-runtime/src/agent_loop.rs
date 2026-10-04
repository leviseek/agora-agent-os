//! The agent loop: Goal -> Plan -> Act (task graph) -> Observe -> Finalize.
//!
//! Why a task graph in the middle: a plan usually contains several independent capability calls.
//! Expressing them as graph nodes means the scheduler can run them in parallel, retry the ones
//! that fail and cancel the ones whose dependencies died - all without the loop knowing about it.

use crate::session::SessionDeps;
use agentos_capability_runtime::capability::CallerContext;
use agentos_core::error::{Result, RuntimeError};
/// Who answered a model call. Carried out of planning so the run can record the truth.
#[derive(Debug, Clone)]
struct Answerer {
    provider: String,
    model: String,
}

/// Everything that goes into a prompt besides the goal itself.
///
/// Grouped rather than passed as loose arguments because the set keeps growing (history, project
/// instructions, recalled memory, and later retrieved documents), and because the order they are
/// assembled in is a decision worth stating once: instructions first, then background, then the
/// conversation, then the goal.
#[derive(Debug, Clone, Copy, Default)]
pub struct PromptContext<'a> {
    /// Conversation so far, oldest first.
    pub history: &'a [ChatMessage],
    /// Project instructions read from the workspace at the start of the run.
    pub workspace: Option<&'a str>,
    /// Memories the history window can no longer show.
    pub memory: Option<&'a str>,
}

/// Token accounting shared by every model call of one run.
///
/// Shared rather than returned because the parallel task-graph calls happen inside a TaskRunner:
/// the loop never sees their responses, but the run still has to pay for them.
#[derive(Debug, Clone, Default)]
pub struct UsageMeter {
    inner: Arc<parking_lot::Mutex<TokenUsage>>,
}

impl UsageMeter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, usage: &agentos_model_router::Usage) {
        // Scoped on purpose: holding this guard across an await would make the future non-Send.
        let mut inner = self.inner.lock();
        // Providers report u32 (that is what the wire formats use); the runtime keeps u64 totals so
        // a long session cannot overflow what a single response carries.
        inner.record(
            u64::from(usage.prompt_tokens),
            u64::from(usage.completion_tokens),
            u64::from(usage.total_tokens),
        );
    }

    pub fn snapshot(&self) -> TokenUsage {
        *self.inner.lock()
    }
}

use agentos_core::model::{
    ActionCall, AgentRun, AgentStep, ContentPart, EventKind, MessageRole, NewEvent, Observation,
    Plan, PlanStep, PlanStepKind, SessionMessage as TranscriptMessage, StepKind, TaskGraphRecord,
    TaskKind, TaskPayload, TaskRecord, TokenUsage,
};
use agentos_core::state::{AgentRunState, StateMachine};
use agentos_core::telemetry::Correlation;
use agentos_core::{now_ms, TaskId};
use agentos_model_router::{ChatMessage, ModelRequest, ModelTask, ToolSpec};
use agentos_storage::store::{collections, Collection};
use agentos_task_scheduler::scheduler::{TaskContext, TaskRunner};
use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Turn the stored conversation into model messages, oldest first.
///
/// Two rules matter here:
///   * the budget is spent on the *newest* turns - dropping the oldest is the only sane way to
///     shrink a conversation, because old turns are the ones a follow-up question is least likely
///     to depend on;
///   * the newest turn is truncated, never dropped, so an oversized message cannot silently
///     remove the very context the goal refers to.
///
/// Only text parts take part: a future non-text part must be handled explicitly rather than
/// stringified by accident.
pub fn history_for_model(
    transcript: &[TranscriptMessage],
    max_messages: usize,
    max_chars: usize,
) -> Vec<ChatMessage> {
    if max_messages == 0 || max_chars == 0 {
        return Vec::new();
    }

    let mut converted: Vec<ChatMessage> = transcript
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                // System and tool turns are reconstructed per run; replaying them would duplicate
                // the live tool exchange the loop is about to build.
                _ => return None,
            };
            // Text parts only, on purpose: stringifying a JSON or artifact part would put a
            // machine payload into the conversation as if the user had typed it.
            let text: String = message
                .parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.trim().is_empty() {
                return None;
            }
            Some(ChatMessage { role: role.to_string(), content: text, name: None, tool_call_id: None })
        })
        .collect();

    // Newest first while spending the budget, then flip back.
    let mut kept: Vec<ChatMessage> = Vec::new();
    let mut chars = 0usize;
    for message in converted.drain(..).rev() {
        if kept.len() >= max_messages {
            break;
        }
        let remaining = max_chars.saturating_sub(chars);
        if remaining == 0 {
            break;
        }
        if message.content.chars().count() <= remaining {
            chars += message.content.chars().count();
            kept.push(message);
        } else {
            let truncated: String = message.content.chars().take(remaining).collect();
            kept.push(ChatMessage { content: truncated, ..message });
            break;
        }
    }
    kept.reverse();
    kept
}

/// Assemble everything that precedes the goal, in the order that makes it readable.
fn push_context(messages: &mut Vec<ChatMessage>, context: &PromptContext<'_>) {
    // Instructions from the repository come first: they are the rules of the place.
    if let Some(workspace) = context.workspace {
        messages.push(ChatMessage::system(workspace));
    }
    // Then what happened earlier in this session but is no longer in the window.
    if let Some(memory) = context.memory {
        messages.push(ChatMessage::system(memory));
    }
    messages.extend(context.history.iter().cloned());
}

#[derive(Debug, Clone)]
pub struct AgentLoopOutcome {
    pub answer: String,
    pub steps: u32,
    pub plan: Option<Plan>,
    pub graph_id: Option<TaskId>,
}

pub struct AgentLoop {
    deps: Arc<SessionDeps>,
    session_id: agentos_core::SessionId,
    correlation: Correlation,
    cancellation: CancellationToken,
}

impl AgentLoop {
    pub fn new(
        deps: Arc<SessionDeps>,
        session_id: agentos_core::SessionId,
        correlation: Correlation,
        cancellation: CancellationToken,
    ) -> Self {
        Self { deps, session_id, correlation, cancellation }
    }

    fn transition(run: &mut AgentRun, next: AgentRunState) -> Result<()> {
        run.state = run
            .state
            .transition(next)
            .map_err(|e| RuntimeError::internal(format!("agent run state machine: {e}")))?;
        run.updated_at = now_ms();
        Ok(())
    }

    fn push_step(run: &mut AgentRun, step: AgentStep) {
        run.steps.push(step);
        run.updated_at = now_ms();
    }

    /// Tool specifications are derived from the live capability registry: the model can only ever
    /// call something the mesh can actually serve.
    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.deps
            .mesh
            .list()
            .into_iter()
            .filter(|d| !self.deps.spec.allowed_capabilities.is_empty()
                && self.deps.spec.allowed_capabilities.contains(&d.name))
            .map(|d| ToolSpec {
                name: d.name.clone(),
                description: d.description.clone(),
                input_schema: d.input_schema.clone(),
            })
            .collect()
    }

    fn all_tool_specs(&self) -> Vec<ToolSpec> {
        let filtered = self.tool_specs();
        if filtered.is_empty() {
            self.deps
                .mesh
                .list()
                .into_iter()
                .map(|d| ToolSpec {
                    name: d.name.clone(),
                    description: d.description.clone(),
                    input_schema: d.input_schema.clone(),
                })
                .collect()
        } else {
            filtered
        }
    }

    /// Run one goal to completion.
    ///
    /// History is the conversation so far, already bounded by the caller: the loop does not read
    /// the transcript itself, so it stays testable without a session actor.
    pub async fn run(
        &mut self,
        run: &mut AgentRun,
        goal: &str,
        context: PromptContext<'_>,
    ) -> Result<AgentLoopOutcome> {
        let tools = self.all_tool_specs();
        let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        let meter = UsageMeter::new();

        // ---- Goal ---------------------------------------------------------------
        Self::transition(run, AgentRunState::Goal)?;
        let mut steps: u32 = 0;
        self.deps.bus
            .publish(
                NewEvent::new(EventKind::AgentStep, "goal accepted")
                    .session(run.session_id.clone())
                    .agent(run.id.clone())
                    .node(self.deps.node_id.clone())
                    .payload(serde_json::json!({ "phase": "goal", "available_capabilities": tool_names })),
            )
            .await?;
        Self::push_step(run, AgentStep {
            index: steps,
            kind: StepKind::Goal,
            thought: format!("Goal received: {goal}"),
            action: None,
            observation: None,
            started_at: now_ms(),
            duration_ms: 0,
            correlation_id: Some(agentos_core::CorrelationId::new()),
        });

        // ---- Plan ---------------------------------------------------------------
        Self::transition(run, AgentRunState::Planning)?;
        steps += 1;
        let plan_started = now_ms();
        let (plan, answerer) = self
            .plan(goal, tools.clone(), context, &meter)
            .await?;
        // Recorded after every phase, not only at the end: a run that fails halfway still cost
        // what it cost, and a caller looking at the record deserves the real number.
        run.usage = meter.snapshot();
        run.plan = Some(plan.clone());
        // Recorded from the response, not from what we asked for: the router may have failed over.
        run.provider = Some(answerer.provider);
        run.model = Some(answerer.model);
        Self::push_step(run, AgentStep {
            index: steps,
            kind: StepKind::Plan,
            thought: plan.reasoning.clone(),
            action: None,
            observation: None,
            started_at: plan_started,
            duration_ms: now_ms().saturating_sub(plan_started),
            correlation_id: Some(agentos_core::CorrelationId::new()),
        });

        // ---- Act ----------------------------------------------------------------
        Self::transition(run, AgentRunState::Thinking)?;
        Self::transition(run, AgentRunState::Acting)?;
        let (graph, graph_id) = self.materialise_plan(run, &plan)?;
        run.task_graph_id = Some(graph_id.clone());

        let mut outcome = None;
        if !graph.nodes.is_empty() {
            let runner = Arc::new(PlanTaskRunner {
                deps: self.deps.clone(),
                session_id: run.session_id.clone(),
                agent_id: run.id.clone(),
                cancellation: self.cancellation.clone(),
                correlation: self.correlation.clone(),
                usage: meter.clone(),
            });
            let scheduler = agentos_task_scheduler::scheduler::Scheduler::new(
                self.deps.store.clone(),
                self.deps.bus.clone(),
                runner,
                self.deps.scheduler.config().clone(),
            );
            let result = scheduler.run_with_cancel(graph.clone(), self.cancellation.clone()).await?;
            outcome = Some(result);
        }

        // ---- Observe ------------------------------------------------------------
        Self::transition(run, AgentRunState::Observing)?;
        let mut observations: Vec<(String, serde_json::Value)> = Vec::new();
        if let Some(result) = &outcome {
            for node in &result.nodes {
                steps += 1;
                if steps > self.deps.spec.max_steps {
                    return Err(RuntimeError::policy_denied(format!(
                        "agent exceeded its step budget of {}",
                        self.deps.spec.max_steps
                    )));
                }
                let output = node.result.clone().unwrap_or(serde_json::Value::Null);
                let ok = node.state == agentos_core::state::TaskState::Succeeded;
                // Join nodes only aggregate their dependencies: their inputs duplicate the
                // capability outputs, so they are recorded as a step but not as an observation.
                if ok && node.kind != TaskKind::Join {
                    observations.push((node.title.clone(), output.clone()));
                }
                let action = match &node.payload {
                    TaskPayload::Capability { capability, input, .. } => Some(ActionCall {
                        capability: capability.clone(),
                        version: None,
                        input: input.clone(),
                        task_id: Some(node.id.clone()),
                    }),
                    _ => None,
                };
                Self::push_step(run, AgentStep {
                    index: steps,
                    kind: if action.is_some() { StepKind::Act } else { StepKind::Observe },
                    thought: node.title.clone(),
                    action,
                    observation: Some(Observation {
                        capability: node
                            .labels
                            .get("capability")
                            .cloned()
                            .unwrap_or_else(|| node.kind.as_str().to_string()),
                        ok,
                        output,
                        error: node.error.clone(),
                        duration_ms: node.duration_ms(),
                        attempts: node.attempts,
                    }),
                    started_at: node.started_at.unwrap_or_else(now_ms),
                    duration_ms: node.duration_ms(),
                    correlation_id: Some(agentos_core::CorrelationId::new()),
                });
            }
            run.usage = meter.snapshot();
            let collection: Collection<TaskGraphRecord> = Collection::new(collections::GRAPHS);
            let graph_record = result_to_graph(result, &graph_id, &run.session_id);
            collection.save(self.deps.store.as_ref(), graph_id.as_str(), &graph_record).await?;
        }

        // ---- Finalize -----------------------------------------------------------
        Self::transition(run, AgentRunState::Finalizing)?;
        let final_started = now_ms();
        let answer = self
            .finalise(goal, &plan, &observations, context, &meter, tools)
            .await?;
        run.usage = meter.snapshot();
        steps += 1;
        Self::push_step(run, AgentStep {
            index: steps,
            kind: StepKind::Finalize,
            thought: "final answer".into(),
            action: None,
            observation: None,
            started_at: final_started,
            duration_ms: now_ms().saturating_sub(final_started),
            correlation_id: Some(agentos_core::CorrelationId::new()),
        });
        run.final_answer = Some(answer.clone());

        // The turn record is written by the session actor once the run settles: it knows the goal,
        // the outcome and whether the run failed, so one record per turn is enough. Writing a
        // second, near-identical record here only made recall see everything twice.
        Ok(AgentLoopOutcome { answer, steps, plan: run.plan.clone(), graph_id: Some(graph_id) })
    }

    async fn plan(
        &self,
        goal: &str,
        tools: Vec<ToolSpec>,
        context: PromptContext<'_>,
        meter: &UsageMeter,
    ) -> Result<(Plan, Answerer)> {
        let system = format!(
            "{}\nRespond with JSON only: {{\"goal\": string, \"reasoning\": string, \"steps\": [{{\"id\": string, \"description\": string, \"kind\": \"think\"|\"capability\"|\"respond\", \"capability\": string|null, \"input\": object, \"depends_on\": [string]}}]}}",
            self.deps.spec.system_prompt
        );
        let request = ModelRequest::new(
            ModelTask::Plan,
            {
                // The plan is where a follow-up like "now do the same for the other file" is
                // understood, so the conversation goes in front of the goal, not behind it.
                let mut messages = vec![ChatMessage::system(system)];
                push_context(&mut messages, &context);
                messages.push(ChatMessage::user(goal.to_string()));
                messages
            },
        )
        .with_tools(tools)
        .with_json();

        if let Err(e) = self
            .deps
            .bus
            .publish(
                NewEvent::new(EventKind::ModelCall, "planning with the model router")
                    .session(self.session_id.clone())
                    .payload(serde_json::json!({ "task": "plan" })),
            )
            .await
        {
            tracing::warn!(error = %e, "failed to publish model call event");
        }
        let response = self.deps.models.complete(request).await?;
        meter.record(&response.usage);

        self.deps
            .bus
            .publish(
                NewEvent::new(EventKind::ModelResult, "planning finished")
                    .session(self.session_id.clone())
                    .payload(serde_json::json!({
                        "provider": response.provider.clone(),
                        "model": response.model.clone(),
                        "tokens": response.usage.total_tokens,
                        "latency_ms": response.latency_ms,
                    })),
            )
            .await?;

        let parsed = serde_json::from_str::<serde_json::Value>(&response.content).ok();
        let plan = parsed
            .as_ref()
            .and_then(|v| parse_plan(goal, v))
            .unwrap_or_else(|| Plan {
                goal: goal.to_string(),
                reasoning: format!("the model did not return a parsable plan ({}), answering directly", response.provider),
                steps: vec![PlanStep {
                    id: "respond-1".into(),
                    description: "Answer the user directly.".into(),
                    kind: PlanStepKind::Respond,
                    capability: None,
                    input: serde_json::json!({ "content": response.content }),
                    depends_on: vec![],
                }],
            });
        Ok((plan, Answerer { provider: response.provider, model: response.model }))
    }

    /// Turn plan steps into task graph nodes. Capability steps become capability tasks, think
    /// steps become model tasks and respond steps become join nodes.
    fn materialise_plan(&self, run: &AgentRun, plan: &Plan) -> Result<(TaskGraphRecord, TaskId)> {
        let mut graph = TaskGraphRecord::new(run.session_id.clone(), format!("plan for: {}", run.goal));
        graph.agent_id = Some(run.id.clone());
        let mut by_plan_id: std::collections::HashMap<String, TaskId> = std::collections::HashMap::new();

        for step in &plan.steps {
            let payload = match step.kind {
                PlanStepKind::Capability => TaskPayload::Capability {
                    capability: step.capability.clone().unwrap_or_default(),
                    version: None,
                    input: step.input.clone(),
                },
                PlanStepKind::Think => TaskPayload::Model {
                    prompt: format!("{}\nGoal: {}", step.description, plan.goal),
                    model_hint: self.deps.spec.model_hint.clone(),
                },
                PlanStepKind::Respond => TaskPayload::Join { template: step.description.clone() },
            };
            let kind = match step.kind {
                PlanStepKind::Capability => TaskKind::Capability,
                PlanStepKind::Think => TaskKind::Model,
                PlanStepKind::Respond => TaskKind::Join,
            };
            let mut node = TaskRecord::new(graph.id.clone(), run.session_id.clone(), step.description.clone(), kind, payload);
            node.agent_id = Some(run.id.clone());
            node.max_attempts = if kind == TaskKind::Join { 1 } else { 3 };
            node.timeout_ms = self.deps.spec.timeout_ms.min(60_000);
            if let Some(cap) = &step.capability {
                node.labels.insert("capability".into(), cap.clone());
            }
            let id = node.id.clone();
            by_plan_id.insert(step.id.clone(), id.clone());
            graph.add(node);
        }

        // Second pass: translate plan-level dependencies into task ids.
        let mut deps_by_task: std::collections::HashMap<TaskId, Vec<TaskId>> = std::collections::HashMap::new();
        for step in &plan.steps {
            let Some(task_id) = by_plan_id.get(&step.id) else { continue };
            let deps: Vec<TaskId> = step
                .depends_on
                .iter()
                .filter_map(|d| by_plan_id.get(d).cloned())
                .collect();
            deps_by_task.insert(task_id.clone(), deps);
        }
        for node in &mut graph.nodes {
            if let Some(deps) = deps_by_task.get(&node.id) {
                node.deps = deps.clone();
            }
        }
        let graph_id = graph.id.clone();
        Ok((graph, graph_id))
    }

    async fn finalise(
        &self,
        goal: &str,
        plan: &Plan,
        observations: &[(String, serde_json::Value)],
        context: PromptContext<'_>,
        meter: &UsageMeter,
        tools: Vec<ToolSpec>,
    ) -> Result<String> {
        // A respond step with a concrete answer short-circuits the second model call.
        if let Some(step) = plan.steps.iter().find(|s| s.kind == PlanStepKind::Respond) {
            if let Some(content) = step.input.get("content").and_then(|c| c.as_str()) {
                if !content.trim().is_empty() && observations.is_empty() {
                    return Ok(content.to_string());
                }
            }
        }

        if let Some((_, value)) = observations.iter().find(|(title, _)| title.to_lowercase().contains("answer")) {
            if let Some(text) = value.get("content").and_then(|c| c.as_str()) {
                return Ok(text.to_string());
            }
        }

        // Same rule as planning: conversation first, then the goal, then this run's exchange.
        let mut messages = vec![ChatMessage::system(self.deps.spec.system_prompt.clone())];
        push_context(&mut messages, &context);
        messages.push(ChatMessage::user(goal.to_string()));
        if !observations.is_empty() {
            messages.push(ChatMessage::assistant(format!(
                "I ran {} step(s): {}",
                observations.len(),
                observations.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>().join("; ")
            )));
            for (title, value) in observations {
                messages.push(ChatMessage::tool(
                    serde_json::json!({ "step": title, "output": value }).to_string(),
                    title.clone(),
                ));
            }
        }
        let response = self
            .deps
            .models
            .complete(
                ModelRequest::new(ModelTask::Summarize, messages)
                    .with_tools(tools)
                    .with_json(),
            )
            .await;
        match response {
            Ok(r) => {
                meter.record(&r.usage);
                if !r.content.trim().is_empty() {
                    return Ok(r.content);
                }
                Err(RuntimeError::model("the model returned an empty final answer"))
            }
            Err(e) => {
                // Degrade gracefully: return the best observation we have instead of failing the run.
                if let Some((title, value)) = observations.last() {
                    Ok(format!("{title}: {value}"))
                } else {
                    Err(e)
                }
            }
        }
    }
}

fn result_to_graph(
    outcome: &agentos_task_scheduler::scheduler::GraphOutcome,
    graph_id: &TaskId,
    session_id: &agentos_core::SessionId,
) -> TaskGraphRecord {
    let mut graph = TaskGraphRecord::new(session_id.clone(), "executed plan");
    graph.id = graph_id.clone();
    graph.state = outcome.state;
    graph.nodes = outcome.nodes.clone();
    graph.updated_at = now_ms();
    graph
}

fn parse_plan(goal: &str, value: &serde_json::Value) -> Option<Plan> {
    let steps_json = value.get("steps")?.as_array()?;
    let mut steps = Vec::new();
    for (i, raw) in steps_json.iter().enumerate() {
        let kind = match raw.get("kind").and_then(|k| k.as_str()).unwrap_or("think") {
            "capability" => PlanStepKind::Capability,
            "respond" => PlanStepKind::Respond,
            _ => PlanStepKind::Think,
        };
        let id = raw
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("step-{i}"));
        steps.push(PlanStep {
            id,
            description: raw
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("step")
                .to_string(),
            kind,
            capability: raw.get("capability").and_then(|v| v.as_str()).map(|s| s.to_string()),
            input: raw.get("input").cloned().unwrap_or(serde_json::json!({})),
            depends_on: raw
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default(),
        });
    }
    if steps.is_empty() {
        return None;
    }
    Some(Plan {
        goal: value
            .get("goal")
            .and_then(|g| g.as_str())
            .unwrap_or(goal)
            .to_string(),
        reasoning: value
            .get("reasoning")
            .and_then(|r| r.as_str())
            .unwrap_or("model supplied plan")
            .to_string(),
        steps,
    })
}

/// Executes one plan node. Capability nodes go through the mesh (policy, schema, timeout, retry).
pub struct PlanTaskRunner {
    deps: Arc<SessionDeps>,
    session_id: agentos_core::SessionId,
    agent_id: agentos_core::AgentId,
    cancellation: CancellationToken,
    correlation: Correlation,
    /// Parallel task-graph model calls report their cost here, because the loop never sees their
    /// responses.
    usage: UsageMeter,
}

#[async_trait]
impl TaskRunner for PlanTaskRunner {
    async fn run(&self, node: &TaskRecord, ctx: TaskContext) -> Result<serde_json::Value> {
        match &node.payload {
            TaskPayload::Capability { capability, input, .. } => {
                tracing::debug!(
                    agent = %self.agent_id,
                    task = %ctx.task_id,
                    capability = capability.as_str(),
                    attempt = ctx.attempt,
                    "plan task invoking capability"
                );
                let caller = CallerContext {
                    session_id: self.session_id.clone(),
                    actor_id: None,
                    task_id: Some(node.id.clone()),
                    correlation: self.correlation.clone(),
                    cancellation: self.cancellation.clone(),
                };
                let result = self
                    .deps
                    .mesh
                    .invoke(capability, None, input.clone(), caller)
                    .await?;
                Ok(result.output)
            }
            TaskPayload::Model { prompt, model_hint } => {
                let mut request = ModelRequest::new(
                    ModelTask::Think,
                    vec![ChatMessage::system(self.deps.spec.system_prompt.clone()), ChatMessage::user(prompt.clone())],
                );
                request.model_hint = model_hint.clone();
                let response = self.deps.models.complete(request).await?;
                self.usage.record(&response.usage);
                Ok(serde_json::json!({
                    "content": response.content,
                    "provider": response.provider,
                    "model": response.model,
                }))
            }
            TaskPayload::Join { template } => Ok(serde_json::json!({
                "template": template,
                "inputs": ctx.dependency_outputs,
            })),
            TaskPayload::Agent { goal, .. } => Err(RuntimeError::invalid_input(format!(
                "nested agent tasks are reserved for a later version (goal: {goal})"
            ))),
        }
    }

    async fn on_cancel(&self, node: &TaskRecord) -> Result<()> {
        tracing::debug!(task = %node.id, "plan task cancelled");
        Ok(())
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use agentos_core::SessionId;

    fn turns(count: usize) -> Vec<TranscriptMessage> {
        let session = SessionId::new();
        (0..count)
            .map(|i| {
                if i % 2 == 0 {
                    TranscriptMessage::user(session.clone(), format!("user turn {i}"))
                } else {
                    TranscriptMessage::assistant(session.clone(), format!("assistant turn {i}"))
                }
            })
            .collect()
    }

    #[test]
    fn keeps_the_newest_turns_and_drops_the_oldest() {
        let history = history_for_model(&turns(10), 4, 10_000);
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].content, "user turn 6");
        assert_eq!(history[3].content, "assistant turn 9");
        assert_eq!(history[0].role, "user");
        assert_eq!(history[3].role, "assistant");
    }

    #[test]
    fn spends_the_character_budget_on_the_newest_turns() {
        let history = history_for_model(&turns(10), 50, 30);
        assert!(history.len() < 10, "the budget must bite: kept {}", history.len());
        assert_eq!(history.last().unwrap().content, "assistant turn 9");
        let chars: usize = history.iter().map(|m| m.content.chars().count()).sum();
        assert!(chars <= 30, "kept {chars} chars, budget was 30");
    }

    #[test]
    fn truncates_an_oversized_newest_turn_instead_of_dropping_it() {
        let session = SessionId::new();
        let long = TranscriptMessage::user(session, "x".repeat(500));
        let history = history_for_model(&[long], 10, 100);
        assert_eq!(history.len(), 1, "the newest turn is never dropped");
        assert_eq!(history[0].content.chars().count(), 100);
    }

    #[test]
    fn ignores_empty_turns_and_non_text_parts() {
        let session = SessionId::new();
        let mut artifact_only = TranscriptMessage::assistant(session.clone(), "");
        artifact_only.parts = vec![ContentPart::Artifact { artifact_id: "a1".into(), name: "x".into() }];
        let transcript = vec![
            TranscriptMessage::user(session.clone(), "   "),
            artifact_only,
            TranscriptMessage::user(session, "real turn"),
        ];
        let history = history_for_model(&transcript, 10, 1_000);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "real turn");
    }

    #[test]
    fn a_zero_budget_switches_history_off() {
        assert!(history_for_model(&turns(4), 0, 1_000).is_empty());
        assert!(history_for_model(&turns(4), 4, 0).is_empty());
    }
}
