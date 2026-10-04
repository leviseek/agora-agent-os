//! Provider-agnostic model types.

use agentos_core::config::ProviderKind;
use agentos_core::error::Result;
use agentos_core::telemetry::Correlation;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: content.into(), name: None, tool_call_id: None }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: content.into(), name: None, tool_call_id: None }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: content.into(), name: None, tool_call_id: None }
    }
    pub fn tool(content: impl Into<String>, call_id: impl Into<String>) -> Self {
        Self { role: "tool".into(), content: content.into(), name: None, tool_call_id: Some(call_id.into()) }
    }
}

/// A capability offered to the model as a tool. The schema comes straight from the capability
/// descriptor, so the model can never call something that does not exist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// What the model is being asked to do. Routing policy keys off this, not off the prompt text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTask {
    Plan,
    Think,
    Act,
    Summarize,
    Classify,
    General,
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub model_hint: Option<String>,
    pub task: ModelTask,
    pub temperature: f32,
    pub max_tokens: Option<u32>,
    /// Ask the provider for strict JSON.
    pub json_mode: bool,
    pub timeout_ms: u64,
    pub correlation: Correlation,
}

impl ModelRequest {
    pub fn new(task: ModelTask, messages: Vec<ChatMessage>) -> Self {
        Self {
            messages,
            tools: vec![],
            model_hint: None,
            task,
            temperature: 0.2,
            max_tokens: None,
            json_mode: false,
            timeout_ms: 60_000,
            correlation: Correlation::new(),
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_json(mut self) -> Self {
        self.json_mode = true;
        self
    }

    pub fn last_user_message(&self) -> Option<&ChatMessage> {
        self.messages.iter().rev().find(|m| m.role == "user")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
    #[serde(default)]
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub latency_ms: u64,
    pub finish_reason: String,
}

impl ModelResponse {
    pub fn text(provider: &str, model: &str, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            tool_calls: vec![],
            provider: provider.to_string(),
            model: model.to_string(),
            usage: Usage::default(),
            latency_ms: 0,
            finish_reason: "stop".into(),
        }
    }

    pub fn wants_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealth {
    Ready,
    Degraded(String),
    /// Reachable but missing credentials.
    Unconfigured(String),
    Down(String),
}

impl ProviderHealth {
    pub fn is_ready(&self) -> bool {
        matches!(self, ProviderHealth::Ready)
    }
    pub fn reason(&self) -> String {
        match self {
            ProviderHealth::Ready => "ready".into(),
            ProviderHealth::Degraded(r) => r.clone(),
            ProviderHealth::Unconfigured(r) => r.clone(),
            ProviderHealth::Down(r) => r.clone(),
        }
    }
}

#[async_trait]
pub trait ModelProvider: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn kind(&self) -> ProviderKind;
    fn model(&self) -> &str;
    /// Whether the provider can emit tool calls.
    fn supports_tools(&self) -> bool {
        true
    }
    /// Cheap, synchronous "is this usable right now" check. The router skips providers that say
    /// no, which keeps credentials-free deployments quiet instead of logging a warning per call.
    fn is_configured(&self) -> bool {
        true
    }
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse>;
    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }
}
