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
    /// The provider this run should prefer. None means "let the router decide".
    pub model_hint: Option<String>,
    /// How much thinking to ask for. None means "whatever the provider does by default".
    pub reasoning_effort: Option<ReasoningEffort>,
    pub temperature: f32,
    pub timeout_ms: u64,
}

/// How hard the model should think before answering.
///
/// Deliberately provider-neutral: the runtime states an intent, and each adapter translates it into
/// whatever its API calls it (or ignores it, saying so in a comment rather than in a made-up
/// parameter). Inventing a parameter name for a provider that does not document one is how a
/// request starts failing with a 400 nobody can explain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// Answer directly: no extended thinking, smallest budget.
    Off,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Parse the wire spelling. Returns None for anything unknown, so a client typo cannot
    /// silently become "high".
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => Some(Self::Off),
            "low" => Some(Self::Low),
            "medium" | "mid" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    /// A token budget for providers that take one instead of an enum.
    pub fn thinking_budget_tokens(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Low => 1_024,
            Self::Medium => 4_096,
            Self::High => 16_384,
        }
    }
}

impl Default for AgentSpec {
    fn default() -> Self {
        Self {
            name: "default".into(),
            system_prompt: "You are an Agent OS worker. Decompose the goal, call capabilities when useful, then answer.".into(),
            allowed_capabilities: vec![],
            max_steps: 12,
            model_hint: None,
            reasoning_effort: None,
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
    /// Provider and model that produced this plan, when a model did.
    #[serde(default)]
    pub answered_by: Option<(String, String)>,
    /// Providers that were tried and failed before that plan.
    #[serde(default)]
    pub failed_over_from: Vec<String>,
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
/// Token accounting for a run or a session.
///
/// Every field defaults to zero, and zero means "nobody recorded it" - an old record without the
/// field parses fine and simply reports no usage rather than a wrong number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// How many model calls contributed to the totals above.
    #[serde(default)]
    pub calls: u64,
}

impl TokenUsage {
    /// Add one model call. The total is taken from the provider rather than recomputed, because
    /// providers disagree about what counts (cached prompt tokens, reasoning tokens).
    pub fn record(&mut self, prompt_tokens: u64, completion_tokens: u64, total_tokens: u64) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(prompt_tokens);
        self.completion_tokens = self.completion_tokens.saturating_add(completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(total_tokens);
        self.calls = self.calls.saturating_add(1);
    }

    pub fn add(&mut self, other: &TokenUsage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self.completion_tokens.saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.calls = self.calls.saturating_add(other.calls);
    }

    pub fn is_empty(&self) -> bool {
        self.calls == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    pub id: AgentId,
    pub session_id: SessionId,
    pub spec_name: String,
    pub goal: String,
    pub state: AgentRunState,
    pub steps: Vec<AgentStep>,
    pub plan: Option<Plan>,
    /// Who actually answered the run's planning call, as reported by the router.
    ///
    /// Deliberately not "the provider we asked for": the router fails over, so a client that shows
    /// the requested provider can name the wrong one. Older records without this field simply have
    /// no recorded answerer.
    #[serde(default)]
    pub provider: Option<String>,
    pub model: Option<String>,
    pub final_answer: Option<String>,
    pub error: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub task_graph_id: Option<TaskId>,
    /// What this run cost in tokens, across every model call it made - including the parallel
    /// task-graph calls.
    #[serde(default)]
    pub usage: TokenUsage,
    /// The provider this run was asked to prefer, if any. Recorded so a surprising answer can be
    /// traced back to the choice that produced it.
    #[serde(default)]
    pub model_hint: Option<String>,
    /// The thinking effort this run was asked for, if any.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Set when the answer did not come from the provider that was asked for: the text is a
    /// fallback's, and a reader deserves to know that before trusting it.
    #[serde(default)]
    pub degraded: Option<String>,
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
            provider: None,
            model: None,
            final_answer: None,
            error: None,
            created_at: now,
            updated_at: now,
            finished_at: None,
            task_graph_id: None,
            usage: TokenUsage::default(),
            model_hint: spec.model_hint.clone(),
            reasoning_effort: spec.reasoning_effort,
            degraded: None,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.state.is_terminal()
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn token_usage_accumulates_across_calls() {
        let mut usage = TokenUsage::default();
        assert!(usage.is_empty());
        usage.record(100, 20, 120);
        usage.record(300, 45, 345);
        assert_eq!(usage.calls, 2);
        assert_eq!(usage.prompt_tokens, 400);
        assert_eq!(usage.completion_tokens, 65);
        assert_eq!(usage.total_tokens, 465);
    }

    #[test]
    fn sessions_add_up_their_runs() {
        let mut run = TokenUsage::default();
        run.record(100, 20, 120);
        let mut session = TokenUsage::default();
        session.add(&run);
        session.add(&run);
        assert_eq!(session.total_tokens, 240);
        assert_eq!(session.calls, 2);
    }

    #[test]
    fn totals_saturate_instead_of_wrapping() {
        let mut usage = TokenUsage { total_tokens: u64::MAX, ..Default::default() };
        usage.record(u64::MAX, u64::MAX, u64::MAX);
        assert_eq!(usage.total_tokens, u64::MAX);
        assert_eq!(usage.calls, 1);
    }
}
