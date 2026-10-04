//! Provider selection, failover and per-provider statistics.

use crate::provider::{ModelProvider, ModelRequest, ModelResponse, ModelTask, ProviderHealth};
use agentos_core::config::ProviderKind;
use agentos_core::error::{Result, RuntimeError};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct RoutingPolicy {
    pub default_provider: String,
    /// Preferred providers per task kind, in order.
    pub task_preferences: HashMap<ModelTask, Vec<String>>,
    /// Tried after task preferences and before giving up.
    pub fallback_chain: Vec<String>,
    pub max_retries_per_provider: u32,
    pub retry_backoff_ms: u64,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        let mut task_preferences = HashMap::new();
        task_preferences.insert(ModelTask::Plan, vec!["deepseek".into(), "openai".into(), "qwen".into()]);
        task_preferences.insert(ModelTask::Think, vec!["deepseek".into(), "openai".into(), "qwen".into()]);
        Self {
            default_provider: "mock".into(),
            task_preferences,
            fallback_chain: vec!["mock".into(), "local".into()],
            max_retries_per_provider: 2,
            retry_backoff_ms: 50,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub kind: ProviderKind,
    pub model: String,
    /// Always a bare identifier: ready, degraded, unconfigured or down.
    pub health: ProviderHealth,
    /// Why the provider is in that state; empty when healthy or when there is nothing to say.
    pub health_reason: String,
    pub calls: u64,
    pub failures: u64,
    pub avg_latency_ms: f64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default)]
struct ProviderStats {
    calls: u64,
    failures: u64,
    latency_sum: f64,
    tokens: u64,
}

/// How a completion is being asked for. Kept as one enum so the failover, accounting and
/// back-off logic exist once, no matter which shape the caller wants.
#[derive(Clone, Copy)]
enum CallMode<'a> {
    Plain,
    Streaming(&'a (dyn Fn(String) + Send + Sync)),
}

pub struct ModelRouter {
    providers: HashMap<String, Arc<dyn ModelProvider>>,
    policy: RoutingPolicy,
    stats: RwLock<HashMap<String, ProviderStats>>,
}

impl ModelRouter {
    pub fn new(policy: RoutingPolicy) -> Self {
        Self { providers: HashMap::new(), policy, stats: RwLock::new(HashMap::new()) }
    }

    pub fn register(&mut self, provider: Arc<dyn ModelProvider>) {
        self.providers.insert(provider.name().to_string(), provider);
    }

    pub fn with_provider(mut self, provider: Arc<dyn ModelProvider>) -> Self {
        self.register(provider);
        self
    }

    /// Build the router from configuration. The mock provider is always registered last as the
    /// offline fallback, so the runtime can answer even with no credentials and no network.
    pub fn from_config(config: &agentos_core::config::ModelConfig) -> Self {
        use crate::{MockProvider, OpenAiCompatibleProvider};
        use agentos_core::config::ProviderKind;
        use std::sync::Arc;

        let client = reqwest::Client::builder()
            .user_agent("agentos/0.1")
            .build()
            .unwrap_or_default();
        let policy = RoutingPolicy {
            default_provider: config.default_provider.clone(),
            max_retries_per_provider: config.max_retries.max(1),
            fallback_chain: vec!["mock".into()],
            ..Default::default()
        };
        let mut router = Self::new(policy);
        for provider in config.enabled_providers() {
            match provider.kind {
                ProviderKind::Mock => {
                    router.register(Arc::new(MockProvider::new(provider.model.clone())))
                }
                _ => router.register(Arc::new(OpenAiCompatibleProvider::from_config(provider, client.clone()))),
            }
        }
        if !router.has_provider("mock") {
            router.register(Arc::new(MockProvider::default()));
        }
        router
    }

    pub fn policy(&self) -> &RoutingPolicy {
        &self.policy
    }

    pub fn has_provider(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.providers.keys().cloned().collect();
        names.sort();
        names
    }

    /// Ordered candidate list for a request: explicit hint, then task preference, then default,
    /// then the fallback chain. Duplicates are removed and unknown providers are dropped.
    pub fn resolve(&self, request: &ModelRequest) -> Vec<String> {
        let mut ordered: Vec<String> = Vec::new();
        let configured = |name: &String| {
            self.providers
                .get(name)
                .map(|p| p.is_configured())
                .unwrap_or(false)
        };
        let push = |name: &String, ordered: &mut Vec<String>| {
            if self.providers.contains_key(name) && !ordered.contains(name) {
                ordered.push(name.clone());
            }
        };
        if let Some(hint) = &request.model_hint {
            push(hint, &mut ordered);
        }
        if let Some(prefs) = self.policy.task_preferences.get(&request.task) {
            for p in prefs {
                push(p, &mut ordered);
            }
        }
        push(&self.policy.default_provider, &mut ordered);
        for p in &self.policy.fallback_chain {
            push(p, &mut ordered);
        }
        // Deterministic tail so a request can never end up with zero candidates while providers exist.
        let mut rest: Vec<String> = self.providers.keys().cloned().collect();
        rest.sort();
        for p in &rest {
            push(p, &mut ordered);
        }

        // Prefer providers that are actually usable. If none are, keep the full list so the
        // caller still gets a precise "no API key" error instead of a vague one.
        let usable: Vec<String> = ordered.iter().filter(|n| configured(n)).cloned().collect();
        if usable.is_empty() {
            ordered
        } else {
            usable
        }
    }

    /// Ask for a completion, failing over across providers on retryable errors.
    pub async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.call(request, CallMode::Plain).await
    }

    /// Ask for a completion, forwarding each chunk to on_delta as it arrives.
    ///
    /// Providers that cannot stream fall back to one delta containing the whole answer, so the
    /// caller does not have to know which kind it is talking to.
    pub async fn complete_streaming(
        &self,
        request: ModelRequest,
        on_delta: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ModelResponse> {
        self.call(request, CallMode::Streaming(on_delta)).await
    }

    async fn call(&self, request: ModelRequest, mode: CallMode<'_>) -> Result<ModelResponse> {
        // What the model was actually sent is the first thing to check when a follow-up question
        // behaves as if it had no history.
        tracing::debug!(
            task = ?request.task,
            messages = request.messages.len(),
            tools = request.tools.len(),
            "model request"
        );
        let candidates = self.resolve(&request);
        if candidates.is_empty() {
            return Err(RuntimeError::model("no model provider is registered"));
        }
        let mut errors: Vec<String> = Vec::new();

        for (index, name) in candidates.iter().enumerate() {
            let provider = match self.providers.get(name) {
                Some(p) => p.clone(),
                None => continue,
            };
            let mut attempt = 0;
            while attempt < self.policy.max_retries_per_provider.max(1) {
                attempt += 1;
                let started = agentos_core::now_ms();
                let outcome = match &mode {
                    CallMode::Plain => provider.complete(request.clone()).await,
                    CallMode::Streaming(sink) => {
                        provider.complete_streaming(request.clone(), *sink).await
                    }
                };
                match outcome {
                    Ok(response) => {
                        self.record(name, true, agentos_core::now_ms().saturating_sub(started), response.usage.total_tokens as u64);
                        return Ok(response);
                    }
                    Err(err) => {
                        self.record(name, false, agentos_core::now_ms().saturating_sub(started), 0);
                        if err.is_retryable() {
                            tracing::warn!(
                                provider = name.as_str(),
                                attempt,
                                error = %err,
                                "model provider call failed, will retry or fail over"
                            );
                        } else {
                            tracing::debug!(
                                provider = name.as_str(),
                                error = %err,
                                "model provider is not usable, failing over"
                            );
                        }
                        let last = attempt >= self.policy.max_retries_per_provider.max(1);
                        if !err.is_retryable() || last {
                            errors.push(format!("{name}: {err}"));
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(
                            self.policy.retry_backoff_ms * attempt as u64,
                        ))
                        .await;
                    }
                }
            }
            let _ = index;
        }

        Err(RuntimeError::model(format!("every model provider failed: {}", errors.join(" | ")))
            .with_detail("attempted", serde_json::json!(candidates)))
    }

    pub async fn provider_infos(&self) -> Vec<ProviderInfo> {
        let mut out = Vec::new();
        let stats = self.stats.read().clone();
        for (name, provider) in &self.providers {
            let s = stats.get(name).cloned().unwrap_or_default();
            out.push(ProviderInfo {
                name: name.clone(),
                kind: provider.kind(),
                model: provider.model().to_string(),
                health: provider.health().await,
                health_reason: String::new(),
                calls: s.calls,
                failures: s.failures,
                avg_latency_ms: if s.calls == 0 { 0.0 } else { s.latency_sum / s.calls as f64 },
                total_tokens: s.tokens,
            });
        }
        // Filled in from the health that was just read, so no construction site has to remember.
        for info in &mut out {
            info.health_reason = info.health.reason();
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn record(&self, name: &str, ok: bool, latency_ms: u64, tokens: u64) {
        let mut stats = self.stats.write();
        let entry = stats.entry(name.to_string()).or_default();
        entry.calls += 1;
        if !ok {
            entry.failures += 1;
        }
        entry.latency_sum += latency_ms as f64;
        entry.tokens += tokens;
        let total = entry.calls as f64;
        if total > 0.0 {
            let _ = total;
        }
        agentos_core::telemetry::metrics().inc_by(
            agentos_core::telemetry::metric_names::MODEL_CALLS,
            &[("provider", name), ("outcome", if ok { "ok" } else { "error" })],
            1,
        );
        agentos_core::telemetry::metrics().observe(agentos_core::telemetry::metric_names::MODEL_LATENCY_MS, latency_ms as f64);
        agentos_core::telemetry::metrics().inc(agentos_core::telemetry::metric_names::MODEL_TOKENS, tokens as i64);
    }
}

/// A provider that always fails, used to prove failover works.
pub struct FailingProvider {
    name: String,
}

impl FailingProvider {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[async_trait::async_trait]
impl ModelProvider for FailingProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> ProviderKind {
        ProviderKind::Openai
    }
    fn model(&self) -> &str {
        "broken"
    }
    fn is_configured(&self) -> bool {
        true
    }
    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Down("intentionally broken".into())
    }
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(RuntimeError::model("provider is broken").retryable(false))
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::provider::ChatMessage;
    use parking_lot::Mutex;

    fn router() -> ModelRouter {
        let mut r = ModelRouter::new(RoutingPolicy {
            default_provider: "mock".into(),
            fallback_chain: vec![],
            max_retries_per_provider: 1,
            ..Default::default()
        });
        r.register(Arc::new(MockProvider::default()));
        r
    }

    #[tokio::test]
    async fn deltas_arrive_and_reassemble_into_the_answer() {
        let request = ModelRequest::new(
            ModelTask::Think,
            vec![ChatMessage::user("what is 6*7?")],
        );
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let seen = seen.clone();
            move |text: String| seen.lock().push(text)
        };

        let streamed = router().complete_streaming(request.clone(), &sink).await.unwrap();
        let pieces = seen.lock().clone();
        assert!(pieces.len() > 1, "the built-in provider streams in pieces: {pieces:?}");
        assert_eq!(
            pieces.concat().trim(),
            streamed.content.trim(),
            "the deltas are the answer, not a summary of it"
        );

        // And the streaming path returns exactly what the plain path returns.
        let plain = router().complete(request).await.unwrap();
        assert_eq!(plain.content, streamed.content);
    }

    #[tokio::test]
    async fn a_provider_that_cannot_stream_still_works() {
        // The default implementation emits the whole answer as one delta, so a caller never has to
        // ask whether a provider streams.
        struct OneShot;
        #[async_trait::async_trait]
        impl ModelProvider for OneShot {
            fn name(&self) -> &str { "oneshot" }
            fn kind(&self) -> ProviderKind { ProviderKind::Local }
            fn model(&self) -> &str { "oneshot-1" }
            async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
                Ok(ModelResponse {
                    content: "all at once".into(),
                    tool_calls: vec![],
                    provider: "oneshot".into(),
                    model: "oneshot-1".into(),
                    usage: crate::provider::Usage::default(),
                    latency_ms: 1,
                    finish_reason: "stop".into(),
                })
            }
        }
        let provider = OneShot;
        let pieces = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let pieces = pieces.clone();
            move |text: String| pieces.lock().push(text)
        };
        let response = provider
            .complete_streaming(ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("hi")]), &sink)
            .await
            .unwrap();
        assert_eq!(response.content, "all at once");
        assert_eq!(pieces.lock().len(), 1, "one delta is a valid stream");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::provider::ChatMessage;

    fn router() -> ModelRouter {
        let mut r = ModelRouter::new(RoutingPolicy {
            default_provider: "deepseek".into(),
            fallback_chain: vec!["mock".into()],
            max_retries_per_provider: 1,
            ..Default::default()
        });
        r.register(Arc::new(MockProvider::default()));
        r
    }

    #[tokio::test]
    async fn falls_back_to_a_working_provider() {
        let mut r = router();
        r.register(Arc::new(FailingProvider::new("deepseek")));
        let resp = r
            .complete(ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("hello")]))
            .await
            .unwrap();
        assert_eq!(resp.provider, "mock");
    }

    #[test]
    fn resolution_order_prefers_hint_then_task_then_default() {
        let mut r = router();
        r.register(Arc::new(FailingProvider::new("deepseek")));
        let mut req = ModelRequest::new(ModelTask::Plan, vec![ChatMessage::user("x")]);
        let order = r.resolve(&req);
        assert_eq!(order[0], "deepseek");
        assert!(order.contains(&"mock".to_string()));

        req.model_hint = Some("mock".into());
        assert_eq!(r.resolve(&req)[0], "mock");
    }

    #[tokio::test]
    async fn all_providers_failing_yields_a_model_error() {
        let mut r = ModelRouter::new(RoutingPolicy { default_provider: "broken".into(), ..Default::default() });
        r.register(Arc::new(FailingProvider::new("broken")));
        let err = r
            .complete(ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("x")]))
            .await
            .unwrap_err();
        assert_eq!(err.kind, agentos_core::ErrorKind::Model);
        assert!(err.message.contains("broken"));
    }

    #[tokio::test]
    async fn stats_are_recorded_per_provider() {
        let r = router();
        r.complete(ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("hi")]))
            .await
            .unwrap();
        let infos = r.provider_infos().await;
        let mock = infos.iter().find(|i| i.name == "mock").unwrap();
        assert_eq!(mock.calls, 1);
        assert_eq!(mock.failures, 0);
    }
}
