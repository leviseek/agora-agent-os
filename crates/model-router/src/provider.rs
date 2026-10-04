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

#[derive(Debug, Clone)]
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

    /// The tag a client switches on. Always a plain identifier, never a structure.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderHealth::Ready => "ready",
            ProviderHealth::Degraded(_) => "degraded",
            ProviderHealth::Unconfigured(_) => "unconfigured",
            ProviderHealth::Down(_) => "down",
        }
    }

    /// The human-readable explanation, empty when there is nothing to explain.
    pub fn reason(&self) -> String {
        match self {
            ProviderHealth::Ready => String::new(),
            ProviderHealth::Degraded(r) | ProviderHealth::Unconfigured(r) | ProviderHealth::Down(r) => {
                r.clone()
            }
        }
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    /// The bug this guards against: one field with two shapes.
    ///
    /// The derived Serialize wrote a bare string for Ready and an object for the variants that
    /// carry a reason, so a client that rendered the value as text crashed the whole console on
    /// the first provider that was unconfigured.
    #[test]
    fn every_variant_is_a_bare_identifier_on_the_wire() {
        let cases = [
            ProviderHealth::Ready,
            ProviderHealth::Degraded("slow".into()),
            ProviderHealth::Unconfigured("OPENAI_API_KEY is not set".into()),
            ProviderHealth::Down("connection refused".into()),
        ];
        for health in cases {
            let json = serde_json::to_value(&health).unwrap();
            assert!(json.is_string(), "expected a string, got {json}");
            assert_eq!(json.as_str().unwrap(), health.as_str());
            let round_tripped: ProviderHealth = serde_json::from_value(json).unwrap();
            assert_eq!(round_tripped.as_str(), health.as_str());
        }
    }

    #[test]
    fn the_reason_is_reported_separately_and_never_as_the_identifier() {
        let unconfigured = ProviderHealth::Unconfigured("DEEPSEEK_API_KEY is not set".into());
        assert_eq!(unconfigured.as_str(), "unconfigured");
        assert!(unconfigured.reason().contains("DEEPSEEK_API_KEY"));
        assert_eq!(ProviderHealth::Ready.reason(), "");
    }
}

/// Serialised as a bare identifier.
///
/// The derived representation was a string for unit variants and an *object* for the three
/// variants carrying a reason (for example {"unconfigured": "..."}). One field with two shapes
/// made a client that renders it as text crash the whole view, so the reason now travels in its
/// own field and this one is always a string.
impl Serialize for ProviderHealth {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderHealth {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let tag = String::deserialize(deserializer)?;
        // A round trip keeps the tag and drops the reason text, which is why the API exposes it
        // separately as health_reason.
        Ok(match tag.as_str() {
            "ready" => ProviderHealth::Ready,
            "degraded" => ProviderHealth::Degraded(String::new()),
            "unconfigured" => ProviderHealth::Unconfigured(String::new()),
            _ => ProviderHealth::Down(String::new()),
        })
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
    /// Stream a completion: call on_delta for each chunk as it arrives, then return exactly what
    /// complete would have returned.
    ///
    /// The default calls complete and emits the whole answer in one delta, so a provider that
    /// cannot stream still works - it just arrives at once. Deltas are a preview channel: the
    /// authoritative answer is always the returned response, and a caller that ignores deltas
    /// entirely is still correct.
    async fn complete_streaming(
        &self,
        request: ModelRequest,
        on_delta: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ModelResponse> {
        let response = self.complete(request).await?;
        if !response.content.is_empty() {
            on_delta(response.content.clone());
        }
        Ok(response)
    }
    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }
}
