use crate::ids::{AgentId, CorrelationId, SessionId, TaskId};
use crate::state::AgentRunState;
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};

/// Agent definition. Note the separation of concerns: an agent is a configuration plus a loop,
/// it is neither a model nor a process nor a capability set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub system_prompt: String,
    pub allowed_capabilities: Vec<String>,
    pub max_steps: u32,
    pub model_hint: Option<String>,
    pub temperature: f32,
    pub timeout_ms: u64,
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            name: "default".into(),
            system_prompt: "You are an Agent OS worker. Decompose the goal, call capabilities when useful, then answer.".into(),
            allowed_capabilities: vec![],
            max_steps: 12,
            model_hint: None,
            temperature: 0.2,
            timeout_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Goal,
    Plan,
    Think,
    Act,
    Observe,
    Finalize,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Goal => "goal",
            StepKind::Plan => "plan",
            StepKind::Think => "think",
            StepKind::Act => "act",
            StepKind::Observe => "observe",
            StepKind::Finalize => "finalize",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepKind {
    /// Pure reasoning step, resolved by the model.
    Think,
    /// A capability (tool) invocation.
    Capability,
    /// The final answer.
    Respond,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub description: String,
    pub kind: PlanStepKind,
    pub capability: Option<String>,
    pub input: serde_json::Value,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub steps: Vec<PlanStep>,
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionCall {
    pub capability: String,
    pub version: Option<String>,
    pub input: serde_json::Value,
    pub task_id: Option<TaskId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub capability: String,
    pub ok: bool,
    pub output: serde_json::Value,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStep {
    pub index: u32,
    pub kind: StepKind,
    pub thought: String,
    pub action: Option<ActionCall>,
    pub observation: Option<Observation>,
    pub started_at: Timestamp,
    pub duration_ms: u64,
    pub correlation_id: Option<CorrelationId>,
}

/// One execution of the agent loop for one goal. Long-lived and fully serializable, which is
/// what makes checkpoint/restore of a session actor possible mid-run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    pub id: AgentId,
    pub session_id: SessionId,
    pub spec_name: String,
    pub goal: String,
    pub state: AgentRunState,
    pub steps: Vec<AgentStep>,
    pub plan: Option<Plan>,
    pub model: Option<String>,
    pub final_answer: Option<String>,
    pub error: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub task_graph_id: Option<TaskId>,
}

impl AgentRun {
    pub fn new(session_id: SessionId, spec: &AgentSpec, goal: impl Into<String>) -> Self {
        let now = crate::now_ms();
        Self {
            id: AgentId::new(),
            session_id,
            spec_name: spec.name.clone(),
            goal: goal.into(),
            state: AgentRunState::Goal,
            steps: Vec::new(),
            plan: None,
            model: None,
            final_answer: None,
            error: None,
            created_at: now,
            updated_at: now,
            finished_at: None,
            task_graph_id: None,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.state.is_terminal()
    }
}
