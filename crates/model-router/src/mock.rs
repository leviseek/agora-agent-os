//! Deterministic in-process provider.
//!
//! This exists for two reasons: the whole runtime must be testable with no network, and the
//! desktop demo must work offline. It implements the same contract as a real provider, including
//! tool calls, so the agent loop exercises the real code path.

use crate::provider::{
    ModelProvider, ModelRequest, ModelResponse, ModelTask, ProviderHealth, ToolCall, Usage,
};
use agentos_core::config::ProviderKind;
use agentos_core::error::Result;
use async_trait::async_trait;
use serde_json::json;

pub struct MockProvider {
    model: String,
    /// Milliseconds between streamed chunks. Zero in production; a demo and test aid for watching
    /// a stream arrive, because a deterministic local answer is otherwise written in microseconds
    /// and no human ever sees it happen.
    stream_delay_ms: u64,
}

impl MockProvider {
    pub fn new(model: impl Into<String>) -> Self {
        Self { model: model.into(), stream_delay_ms: default_stream_delay_ms() }
    }

    /// Pace the stream. Used by tests and by a demo that wants to show the console filling in.
    pub fn with_stream_delay_ms(mut self, delay_ms: u64) -> Self {
        self.stream_delay_ms = delay_ms;
        self
    }

    /// Detect a plain arithmetic expression inside free text.
    fn extract_expression(text: &str) -> Option<String> {
        let cleaned: String = text
            .chars()
            .filter(|c| c.is_ascii_digit() || "+-*/%^(). ".contains(*c))
            .collect();
        let mut best = String::new();
        let mut current = String::new();
        for c in cleaned.chars() {
            if c.is_ascii_digit() || "+-*/%^().".contains(c) {
                current.push(c);
            } else {
                if current.matches(|ch: char| ch.is_ascii_digit()).count() > 0
                    && current.matches(|ch: char| "+-*/%^".contains(ch)).count() > 0
                    && current.len() > best.len()
                {
                    best = current.clone();
                }
                current.clear();
            }
        }
        if current.matches(|ch: char| ch.is_ascii_digit()).count() > 0
            && current.matches(|ch: char| "+-*/%^".contains(ch)).count() > 0
            && current.len() > best.len()
        {
            best = current;
        }
        let trimmed = best.trim().trim_matches(|c| c == '(' || c == ')' || c == '.').to_string();
        if trimmed.chars().any(|c| c.is_ascii_digit()) && trimmed.chars().any(|c| "+-*/%^".contains(c)) {
            Some(trimmed)
        } else {
            None
        }
    }

    /// Detect a request to read a workspace file.
    fn extract_path(text: &str) -> Option<String> {
        let lower = text.to_ascii_lowercase();
        for marker in ["read ", "open ", "cat ", "read the file ", "文件 ", "读取 "] {
            if let Some(idx) = lower.find(marker) {
                let rest = &text[idx + marker.len()..];
                let candidate: String = rest
                    .trim_start_matches(|c: char| c == '"' || c == '\'' || c == ':')
                    .chars()
                    .take_while(|c| !c.is_whitespace() && *c != ',' && *c != '。')
                    .collect();
                let candidate = candidate.trim_matches(|c| c == '"' || c == '\'').to_string();
                if candidate.contains('.') && !candidate.is_empty() && candidate.len() < 200 {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn plan_json(goal: &str, tools: &[String]) -> String {
        let mut steps = vec![json!({
            "id": "think-1",
            "description": "Restate the goal and decide what evidence is needed",
            "kind": "think",
            "input": { "goal": goal },
            "depends_on": []
        })];
        if let Some(expr) = Self::extract_expression(goal) {
            if tools.iter().any(|t| t == "calculator") {
                steps.push(json!({
                    "id": "act-1",
                    "description": format!("Compute {expr} with the calculator capability"),
                    "kind": "capability",
                    "capability": "calculator",
                    "input": { "expression": expr },
                    "depends_on": ["think-1"]
                }));
            }
        }
        if let Some(path) = Self::extract_path(goal) {
            if tools.iter().any(|t| t == "filesystem-read") {
                steps.push(json!({
                    "id": "act-2",
                    "description": format!("Read {path} from the workspace"),
                    "kind": "capability",
                    "capability": "filesystem-read",
                    "input": { "path": path },
                    "depends_on": ["think-1"]
                }));
            }
        }
        // A capability whose name appears in the goal is selected by name. This keeps the mock
        // useful for testing real capabilities without teaching it every capability's semantics.
        let lowered = goal.to_ascii_lowercase();
        for tool in tools {
            if tool == "calculator" || tool == "filesystem-read" {
                continue;
            }
            if lowered.contains(&tool.to_ascii_lowercase()) {
                steps.push(json!({
                    "id": format!("act-{}", tool),
                    "description": format!("Run the {tool} capability"),
                    "kind": "capability",
                    "capability": tool,
                    "input": {},
                    "depends_on": ["think-1"]
                }));
            }
        }
        steps.push(json!({
            "id": "respond",
            "description": "Answer the user with the observations collected",
            "kind": "respond",
            "input": {},
            "depends_on": steps.iter().map(|s| s["id"].clone()).collect::<Vec<_>>()
        }));
        json!({
            "goal": goal,
            "reasoning": "Deterministic plan produced by the mock provider.",
            "steps": steps
        })
        .to_string()
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new("agentos-mock-1")
    }
}

/// The demo pacing knob, read once at construction.
///
/// It is an environment variable rather than a configuration field because it exists to be
/// watched, not to be deployed: nobody should ship a paced placeholder provider, and a deployment
/// that sets this gets exactly what it asked for.
fn default_stream_delay_ms() -> u64 {
    std::env::var("AGENTOS_MOCK_STREAM_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[async_trait]
impl ModelProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Mock
    }

    fn model(&self) -> &str {
        &self.model
    }

    /// The built-in provider streams too, so the streaming path can be exercised end to end
    /// without a network or a key. The chunking is by word and paced only by the caller.
    async fn complete_streaming(
        &self,
        request: ModelRequest,
        on_delta: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ModelResponse> {
        let response = self.complete(request).await?;
        // Pacing is off unless someone asked for it: the delay exists so a person can watch the
        // console render a stream, not because a fake model needs time to think.
        for word in response.content.split_inclusive(' ') {
            if self.stream_delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.stream_delay_ms)).await;
            }
            on_delta(word.to_string());
        }
        if self.stream_delay_ms > 0 && response.content.is_empty() {
            on_delta(String::new());
        }
        Ok(response)
    }

    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let started = agentos_core::now_ms();
        let tool_names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();

        // 1. Planning: emit a machine readable plan.
        if request.json_mode && request.task == ModelTask::Plan {
            let goal = request
                .last_user_message()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            let content = Self::plan_json(&goal, &tool_names);
            return Ok(ModelResponse {
                content,
                tool_calls: vec![],
                provider: self.name().to_string(),
                model: self.model.clone(),
                usage: Usage { prompt_tokens: 64, completion_tokens: 128, total_tokens: 192 },
                latency_ms: agentos_core::now_ms().saturating_sub(started),
                finish_reason: "stop".into(),
            });
        }

        // 2. A tool result is already in the transcript: produce the final answer.
        if let Some(tool_msg) = request.messages.iter().rev().find(|m| m.role == "tool") {
            let goal = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .map(|m| m.content.clone())
                .unwrap_or_else(|| "the request".into());
            let answer = format!(
                "Goal: {goal}\nObservation from capability: {}\nAnswer: derived from the observation above.",
                tool_msg.content.trim()
            );
            return Ok(ModelResponse {
                content: answer,
                tool_calls: vec![],
                provider: self.name().to_string(),
                model: self.model.clone(),
                usage: Usage { prompt_tokens: 96, completion_tokens: 64, total_tokens: 160 },
                latency_ms: agentos_core::now_ms().saturating_sub(started),
                finish_reason: "stop".into(),
            });
        }

        // 3. Fresh user turn: decide whether a capability is needed.
        let goal = request
            .last_user_message()
            .map(|m| m.content.clone())
            .unwrap_or_default();

        if tool_names.iter().any(|t| t == "calculator") {
            if let Some(expression) = Self::extract_expression(&goal) {
                return Ok(ModelResponse {
                    content: format!("I will compute {expression} with the calculator capability."),
                    tool_calls: vec![ToolCall {
                        name: "calculator".into(),
                        arguments: json!({ "expression": expression }),
                        id: "call-calc-1".into(),
                    }],
                    provider: self.name().to_string(),
                    model: self.model.clone(),
                    usage: Usage { prompt_tokens: 48, completion_tokens: 24, total_tokens: 72 },
                    latency_ms: agentos_core::now_ms().saturating_sub(started),
                    finish_reason: "tool_calls".into(),
                });
            }
        }

        if tool_names.iter().any(|t| t == "filesystem-read") {
            if let Some(path) = Self::extract_path(&goal) {
                return Ok(ModelResponse {
                    content: format!("I will read {path} from the workspace."),
                    tool_calls: vec![ToolCall {
                        name: "filesystem-read".into(),
                        arguments: json!({ "path": path }),
                        id: "call-fs-1".into(),
                    }],
                    provider: self.name().to_string(),
                    model: self.model.clone(),
                    usage: Usage { prompt_tokens: 48, completion_tokens: 24, total_tokens: 72 },
                    latency_ms: agentos_core::now_ms().saturating_sub(started),
                    finish_reason: "tool_calls".into(),
                });
            }
        }

        Ok(ModelResponse {
            content: format!(
                "Acknowledged: {goal}. No capability was required, so this is the final answer."
            ),
            tool_calls: vec![],
            provider: self.name().to_string(),
            model: self.model.clone(),
            usage: Usage { prompt_tokens: 40, completion_tokens: 32, total_tokens: 72 },
            latency_ms: agentos_core::now_ms().saturating_sub(started),
            finish_reason: "stop".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ModelTask, ToolSpec};

    fn tools() -> Vec<ToolSpec> {
        vec![
            ToolSpec { name: "calculator".into(), description: "math".into(), input_schema: json!({}) },
            ToolSpec { name: "filesystem-read".into(), description: "fs".into(), input_schema: json!({}) },
        ]
    }

    #[tokio::test]
    async fn emits_a_tool_call_for_arithmetic_goals() {
        let p = MockProvider::default();
        let req = ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("what is 12*7+3?")])
            .with_tools(tools());
        let resp = p.complete(req).await.unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "calculator");
        assert_eq!(resp.tool_calls[0].arguments["expression"], "12*7+3");
    }

    #[tokio::test]
    async fn emits_a_tool_call_for_file_requests() {
        let p = MockProvider::default();
        let req = ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("please read notes.txt")])
            .with_tools(tools());
        let resp = p.complete(req).await.unwrap();
        assert_eq!(resp.tool_calls[0].name, "filesystem-read");
        assert_eq!(resp.tool_calls[0].arguments["path"], "notes.txt");
    }

    #[tokio::test]
    async fn answers_directly_when_no_tool_is_needed() {
        let p = MockProvider::default();
        let req = ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("say hello")]);
        let resp = p.complete(req).await.unwrap();
        assert!(resp.tool_calls.is_empty());
        assert!(resp.content.contains("hello"));
    }

    #[tokio::test]
    async fn closes_the_loop_after_a_tool_result() {
        let p = MockProvider::default();
        let req = ModelRequest::new(
            ModelTask::Think,
            vec![
                ChatMessage::user("what is 12*7+3?"),
                ChatMessage::assistant("calling calculator"),
                ChatMessage::tool("{\"result\":87}", "call-calc-1"),
            ],
        );
        let resp = p.complete(req).await.unwrap();
        assert!(resp.tool_calls.is_empty());
        assert!(resp.content.contains("87"), "final answer must incorporate the observation");
    }

    #[tokio::test]
    async fn planning_mode_returns_valid_json() {
        let p = MockProvider::default();
        let req = ModelRequest::new(ModelTask::Plan, vec![ChatMessage::user("compute 2+2 and tell me")])
            .with_tools(tools())
            .with_json();
        let resp = p.complete(req).await.unwrap();
        let plan: serde_json::Value = serde_json::from_str(&resp.content).unwrap();
        assert!(plan["steps"].as_array().unwrap().len() >= 2);
        assert!(plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["capability"] == "calculator"));
    }
}
