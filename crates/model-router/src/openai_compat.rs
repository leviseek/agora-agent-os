//! OpenAI-compatible provider.
//!
//! DeepSeek, OpenAI, Qwen (DashScope compatible mode) and local servers (Ollama, vLLM, LM Studio)
//! all speak the same /v1/chat/completions dialect, so one implementation serves all four. The
//! API key is read from the environment at call time and never stored.

use crate::provider::{
    ModelProvider, ModelRequest, ModelResponse, ProviderHealth, ToolCall, Usage,
};
use agentos_core::config::{ProviderConfig, ProviderKind};
use agentos_core::error::{Result, RuntimeError};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct OpenAiCompatibleProvider {
    name: String,
    kind: ProviderKind,
    model: String,
    base_url: String,
    api_key: Option<String>,
    key_env: String,
    client: reqwest::Client,
    timeout_ms: u64,
}

impl OpenAiCompatibleProvider {
    /// Build from configuration. The key is resolved once at construction for logging purposes,
    /// but is re-read from the environment on every call so rotation needs no restart.
    pub fn from_config(cfg: &ProviderConfig, client: reqwest::Client) -> Self {
        Self {
            name: cfg.name.clone(),
            kind: cfg.kind,
            model: cfg.model.clone(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key(),
            key_env: cfg.api_key_env.clone(),
            client,
            timeout_ms: cfg.timeout_ms,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    fn current_key(&self) -> Option<String> {
        std::env::var(&self.key_env).ok().filter(|k| !k.trim().is_empty()).or_else(|| self.api_key.clone())
    }

    fn build_body(&self, request: &ModelRequest) -> Value {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                let mut obj = json!({ "role": m.role, "content": m.content });
                if let Some(id) = &m.tool_call_id {
                    obj["tool_call_id"] = json!(id);
                }
                if let Some(name) = &m.name {
                    obj["name"] = json!(name);
                }
                obj
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": request.temperature,
        });
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = json!(max);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request
                .tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                }))
                .collect::<Vec<Value>>());
            body["tool_choice"] = json!("auto");
        }
        if request.json_mode {
            body["response_format"] = json!({ "type": "json_object" });
        }
        body
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn is_configured(&self) -> bool {
        matches!(self.kind, ProviderKind::Local) || self.current_key().is_some()
    }

    async fn health(&self) -> ProviderHealth {
        match self.kind {
            ProviderKind::Local | ProviderKind::Mock => ProviderHealth::Ready,
            _ => match self.current_key() {
                Some(_) => ProviderHealth::Ready,
                None => ProviderHealth::Unconfigured(format!("environment variable {} is not set", self.key_env)),
            },
        }
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let started = agentos_core::now_ms();
        let key = self.current_key();
        if key.is_none() && !matches!(self.kind, ProviderKind::Local) {
            return Err(RuntimeError::model(format!(
                "provider {} has no API key: set {}",
                self.name, self.key_env
            ))
            .retryable(false)
            .with_detail("provider", self.name.clone()));
        }

        let body = self.build_body(&request);
        let mut builder = self
            .client
            .post(self.endpoint())
            .header("content-type", "application/json")
            .timeout(std::time::Duration::from_millis(
                if request.timeout_ms > 0 { request.timeout_ms } else { self.timeout_ms },
            ))
            .json(&body);
        if let Some(k) = &key {
            builder = builder.bearer_auth(k);
        }

        let response = builder.send().await.map_err(|e| {
            let kind = if e.is_timeout() {
                RuntimeError::timeout(format!("provider {} timed out", self.name))
            } else {
                RuntimeError::network(format!("provider {} is unreachable: {e}", self.name))
            };
            kind.with_detail("provider", self.name.clone())
        })?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let retryable = status.as_u16() == 429 || status.is_server_error();
            return Err(RuntimeError::model(format!(
                "provider {} returned HTTP {status}",
                self.name
            ))
            .retryable(retryable)
            .with_detail("status", status.as_u16())
            .with_detail("body", truncate(&text, 512)));
        }

        let parsed: Value = serde_json::from_str(&text).map_err(|e| {
            RuntimeError::model(format!("provider {} returned invalid JSON: {e}", self.name))
                .with_detail("body", truncate(&text, 512))
        })?;

        let message = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .ok_or_else(|| {
                RuntimeError::model(format!("provider {} returned no choices", self.name))
                    .with_detail("body", truncate(&text, 512))
            })?;

        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();

        let mut tool_calls = Vec::new();
        if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
            for (i, call) in calls.iter().enumerate() {
                let name = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                let raw_args = call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let arguments: Value = serde_json::from_str(raw_args).unwrap_or_else(|_| json!({ "raw": raw_args }));
                let id = call
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("call-{i}"));
                if !name.is_empty() {
                    tool_calls.push(ToolCall { name, arguments, id });
                }
            }
        }

        let usage = parsed.get("usage").cloned().unwrap_or_else(|| json!({}));
        let finish_reason = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .and_then(|f| f.as_str())
            .unwrap_or(if tool_calls.is_empty() { "stop" } else { "tool_calls" })
            .to_string();

        Ok(ModelResponse {
            content,
            tool_calls,
            provider: self.name.clone(),
            model: self.model.clone(),
            usage: Usage {
                prompt_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                completion_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                total_tokens: usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            },
            latency_ms: agentos_core::now_ms().saturating_sub(started),
            finish_reason,
        })
    }

    /// Stream a completion using the OpenAI-compatible SSE protocol.
    ///
    /// The deltas are a preview: the same value the non-streaming call would return is assembled
    /// here and returned, including tool calls, which arrive as fragments and have to be put back
    /// together by index.
    async fn complete_streaming(
        &self,
        request: ModelRequest,
        on_delta: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ModelResponse> {
        let started = agentos_core::now_ms();
        let key = self.current_key();
        if key.is_none() && !matches!(self.kind, ProviderKind::Local) {
            return Err(RuntimeError::model(format!(
                "provider {} has no API key: set {}",
                self.name, self.key_env
            ))
            .retryable(false)
            .with_detail("provider", self.name.clone()));
        }

        let mut body = self.build_body(&request);
        body["stream"] = json!(true);
        // Without this the final chunk carries no usage, and a streamed call would look free.
        body["stream_options"] = json!({ "include_usage": true });

        let mut builder = self
            .client
            .post(self.endpoint())
            .header("content-type", "application/json")
            .timeout(std::time::Duration::from_millis(
                if request.timeout_ms > 0 { request.timeout_ms } else { self.timeout_ms },
            ))
            .json(&body);
        if let Some(k) = &key {
            builder = builder.bearer_auth(k);
        }

        let mut response = builder.send().await.map_err(|e| {
            let kind = if e.is_timeout() {
                RuntimeError::timeout(format!("provider {} timed out", self.name))
            } else {
                RuntimeError::network(format!("provider {} is unreachable: {e}", self.name))
            };
            kind.with_detail("provider", self.name.clone())
        })?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let retryable = status.as_u16() == 429 || status.is_server_error();
            return Err(RuntimeError::model(format!(
                "provider {} returned HTTP {status}",
                self.name
            ))
            .retryable(retryable)
            .with_detail("status", status.as_u16())
            .with_detail("body", truncate(&text, 512)));
        }

        let mut buffer = String::new();
        let mut content = String::new();
        let mut fragments: Vec<ToolCallFragment> = Vec::new();
        let mut usage: Value = json!({});
        let mut finish_reason: Option<String> = None;

        // Chunks arrive at arbitrary byte boundaries, so lines are only parsed once complete.
        while let Some(bytes) = response.chunk().await.map_err(|e| {
            RuntimeError::network(format!("provider {} stream broke: {e}", self.name))
        })? {
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(index) = buffer.find('\n') {
                let line: String = buffer[..index].trim().to_string();
                buffer.drain(..=index);
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                let Ok(parsed) = serde_json::from_str::<Value>(payload) else {
                    continue;
                };
                if let Some(reported) = parsed.get("usage").filter(|value| !value.is_null()) {
                    usage = reported.clone();
                }
                let Some(choice) = parsed.get("choices").and_then(|c| c.get(0)) else {
                    continue;
                };
                if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                    finish_reason = Some(reason.to_string());
                }
                let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
                if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                    if !text.is_empty() {
                        content.push_str(text);
                        on_delta(text.to_string());
                    }
                }
                if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                    for call in calls {
                        let index = call.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        while fragments.len() <= index {
                            fragments.push(ToolCallFragment::default());
                        }
                        let slot = &mut fragments[index];
                        if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                            slot.id = Some(id.to_string());
                        }
                        if let Some(function) = call.get("function") {
                            if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                                slot.name.push_str(name);
                            }
                            if let Some(args) = function.get("arguments").and_then(|v| v.as_str()) {
                                slot.arguments.push_str(args);
                            }
                        }
                    }
                }
            }
        }

        let tool_calls = fragments
            .into_iter()
            .enumerate()
            .filter(|(_, fragment)| !fragment.name.trim().is_empty())
            .map(|(index, fragment)| ToolCall {
                name: fragment.name,
                arguments: serde_json::from_str(&fragment.arguments)
                    .unwrap_or_else(|_| json!({ "raw": fragment.arguments })),
                id: fragment.id.unwrap_or_else(|| format!("call-{index}")),
            })
            .collect::<Vec<_>>();

        // Decided before the vector moves into the response.
        let finish_reason = finish_reason.unwrap_or_else(|| {
            if tool_calls.is_empty() { "stop".to_string() } else { "tool_calls".to_string() }
        });

        Ok(ModelResponse {
            content,
            tool_calls,
            provider: self.name.clone(),
            model: self.model.clone(),
            usage: Usage {
                prompt_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                completion_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                total_tokens: usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            },
            latency_ms: agentos_core::now_ms().saturating_sub(started),
            finish_reason,
        })
    }
}

/// Tool calls stream in pieces: the name and the arguments arrive across several chunks and are
/// stitched together by index.
#[derive(Default)]
struct ToolCallFragment {
    id: Option<String>,
    name: String,
    arguments: String,
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, ModelTask, ToolSpec};

    fn cfg() -> ProviderConfig {
        ProviderConfig {
            name: "deepseek".into(),
            kind: ProviderKind::Deepseek,
            model: "deepseek-chat".into(),
            base_url: "https://api.deepseek.com/".into(),
            api_key_env: "AGENTOS_TEST_UNSET_KEY".into(),
            enabled: true,
            priority: 1,
            timeout_ms: 1000,
        }
    }

    #[test]
    fn endpoint_normalises_trailing_slash() {
        let p = OpenAiCompatibleProvider::from_config(&cfg(), reqwest::Client::new());
        assert_eq!(p.endpoint(), "https://api.deepseek.com/v1/chat/completions");
    }

    #[test]
    fn body_carries_tools_and_json_mode() {
        let p = OpenAiCompatibleProvider::from_config(&cfg(), reqwest::Client::new());
        let req = ModelRequest::new(ModelTask::Plan, vec![ChatMessage::user("hi")])
            .with_tools(vec![ToolSpec {
                name: "echo".into(),
                description: "e".into(),
                input_schema: json!({"type":"object"}),
            }])
            .with_json();
        let body = p.build_body(&req);
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["tools"][0]["function"]["name"], "echo");
        assert_eq!(body["response_format"]["type"], "json_object");
    }

    #[tokio::test]
    async fn missing_key_is_reported_as_unconfigured_not_as_a_network_error() {
        let p = OpenAiCompatibleProvider::from_config(&cfg(), reqwest::Client::new());
        match p.health().await {
            ProviderHealth::Unconfigured(reason) => assert!(reason.contains("AGENTOS_TEST_UNSET_KEY")),
            other => panic!("expected unconfigured, got {other:?}"),
        }
        let err = p
            .complete(ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("hi")]))
            .await
            .unwrap_err();
        assert_eq!(err.kind, agentos_core::ErrorKind::Model);
        assert!(!err.is_retryable());
    }
}
