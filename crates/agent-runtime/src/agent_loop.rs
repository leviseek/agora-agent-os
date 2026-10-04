//! The agent loop: Goal -> Plan -> Act (task graph) -> Observe -> Finalize.
//!
//! Why a task graph in the middle: a plan usually contains several independent capability calls.
//! Expressing them as graph nodes means the scheduler can run them in parallel, retry the ones
//! that fail and cancel the ones whose dependencies died - all without the loop knowing about it.

use crate::memory::semantic;
use crate::session::SessionDeps;
use agentos_capability_runtime::capability::CallerContext;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{
    ActionCall, AgentRun, AgentStep, EventKind, NewEvent, Observation, Plan, PlanStep, PlanStepKind,
    StepKind, TaskGraphRecord, TaskKind, TaskPayload, TaskRecord,
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

    pub async fn run(&mut self, run: &mut AgentRun, goal: &str) -> Result<AgentLoopOutcome> {
        let tools = self.all_tool_specs();
        let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();

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
        let plan = self.plan(goal, tools.clone()).await?;
        run.plan = Some(plan.clone());
        run.model = Some(self.deps.spec.model_hint.clone().unwrap_or_else(|| self.deps.models.policy().default_provider.clone()));
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
            let collection: Collection<TaskGraphRecord> = Collection::new(collections::GRAPHS);
            let graph_record = result_to_graph(result, &graph_id, &run.session_id);
            collection.save(self.deps.store.as_ref(), graph_id.as_str(), &graph_record).await?;
        }

        // ---- Finalize -----------------------------------------------------------
        Self::transition(run, AgentRunState::Finalizing)?;
        let final_started = now_ms();
        let answer = self.finalise(goal, &plan, &observations, tools).await?;
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

        let _ = self
            .deps
            .memory
            .write(semantic(
                run.session_id.clone(),
                format!("goal: {goal} -> answer: {answer}"),
                &["agent", "answer"],
            ))
            .await;

        Ok(AgentLoopOutcome { answer, steps, plan: run.plan.clone(), graph_id: Some(graph_id) })
    }

    async fn plan(&self, goal: &str, tools: Vec<ToolSpec>) -> Result<Plan> {
        let system = format!(
            "{}\nRespond with JSON only: {{\"goal\": string, \"reasoning\": string, \"steps\": [{{\"id\": string, \"description\": string, \"kind\": \"think\"|\"capability\"|\"respond\", \"capability\": string|null, \"input\": object, \"depends_on\": [string]}}]}}",
            self.deps.spec.system_prompt
        );
        let request = ModelRequest::new(
            ModelTask::Plan,
            vec![ChatMessage::system(system), ChatMessage::user(goal.to_string())],
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

        self.deps
            .bus
            .publish(
                NewEvent::new(EventKind::ModelResult, "planning finished")
                    .session(self.session_id.clone())
                    .payload(serde_json::json!({
                        "provider": response.provider,
                        "model": response.model,
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
        Ok(plan)
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

        let mut messages = vec![
            ChatMessage::system(self.deps.spec.system_prompt.clone()),
            ChatMessage::user(goal.to_string()),
        ];
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
