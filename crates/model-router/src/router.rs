//! Provider selection, failover and per-provider statistics.

use crate::provider::{ModelProvider, ModelRequest, ModelResponse, ModelTask, ProviderHealth, Vision};
use agentos_core::config::ProviderKind;
use agentos_core::error::{Result, RuntimeError};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Does this provider kind only work once somebody supplies a credential?
///
/// The placeholder and a local endpoint work out of the box, so their presence says nothing about
/// intent; every other kind stays silent until a key is configured.
fn needs_a_credential(kind: ProviderKind) -> bool {
    !matches!(kind, ProviderKind::Mock | ProviderKind::Local)
}

#[derive(Debug, Clone)]
pub struct RoutingPolicy {
    pub default_provider: String,
    /// Preferred providers per task kind, in order.
    pub task_preferences: HashMap<ModelTask, Vec<String>>,
    /// Tried after task preferences and before giving up.
    pub fallback_chain: Vec<String>,
    pub max_retries_per_provider: u32,
    pub retry_backoff_ms: u64,
    /// Configured priority per provider, higher first. Empty means "no opinion", and the router
    /// falls back to the provider name so the order is at least deterministic.
    pub priorities: HashMap<String, u32>,
}

impl RoutingPolicy {
    /// Priority of a provider, defaulting to 0 when the configuration does not say.
    pub fn priority_of(&self, name: &str) -> u32 {
        self.priorities.get(name).copied().unwrap_or(0)
    }
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
            priorities: HashMap::new(),
        }
    }
}

/// Deserialisation default for `ProviderInfo::vision`: an older payload did not say, and saying
/// nothing is not the same as saying no.
fn unknown_vision() -> Vision {
    Vision::Unknown
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub kind: ProviderKind,
    pub model: String,
    /// Whether this model can be shown an image: yes, no, or unknown. A client needs this to say
    /// "this model cannot see" before a screenshot is posted, instead of after a 400.
    #[serde(default = "unknown_vision")]
    pub vision: Vision,
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
            // The operator's own ordering, so "deepseek first" does not depend on the alphabet.
            priorities: config
                .providers
                .iter()
                .map(|provider| (provider.name.clone(), provider.priority.max(0) as u32))
                .collect(),
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

    /// Whether a request carries images: the only thing that makes vision relevant.
    fn carries_images(request: &ModelRequest) -> bool {
        request.messages.iter().any(|message| !message.images.is_empty())
    }

    /// Ordered candidate list for a request: explicit hint, then task preference, then default,
    /// then the fallback chain. Duplicates are removed and unknown providers are dropped.
    ///
    /// When the request carries an image, the list is narrowed to providers that can be shown one:
    /// a text-only model answers HTTP 400 to an image_url, and the failover that follows produces
    /// a confident answer about a picture no model ever saw. Providers that are merely *unknown*
    /// stay in the list - "unknown" is not "no", and a local server running a vision model with an
    /// uninformative name must keep working.
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
        // The built-in placeholder is a stand-in, not a model. When it is the configured default
        // and a real provider is usable, the real one goes first: otherwise a deployment with a
        // working API key still gets canned placeholder prose until somebody picks a model by
        // hand, and those canned turns then sit in the conversation history.
        // Is the configured default the built-in placeholder?
        let default_is_placeholder = self
            .providers
            .get(&self.policy.default_provider)
            .map(|provider| provider.kind() == ProviderKind::Mock)
            .unwrap_or(false);
        // Real providers a request could actually use, best first. Priority comes from the
        // configuration, so an operator's preference is honoured instead of the alphabet's.
        let mut usable_real: Vec<(u32, String)> = self
            .providers
            .iter()
            .filter(|(name, provider)| {
                *name != &self.policy.default_provider
                    && provider.is_configured()
                    // "Configured" is not the same as "set up". A local endpoint needs no
                    // credential, so it always claims to be configured, and preferring it would
                    // send every request to a server that may not be running - pushing out the
                    // placeholder that would have answered instantly. A credential somebody had
                    // to supply is evidence that this provider was meant to be used.
                    && needs_a_credential(provider.kind())
            })
            .map(|(name, _)| (self.policy.priority_of(name), name.clone()))
            .collect();
        usable_real.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));

        if default_is_placeholder && !usable_real.is_empty() {
            // A real model answers; the placeholder is the fallback it was meant to be.
            for (_, name) in &usable_real {
                push(name, &mut ordered);
            }
            push(&self.policy.default_provider, &mut ordered);
        } else {
            push(&self.policy.default_provider, &mut ordered);
        }
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
        let usable = if usable.is_empty() { ordered } else { usable };

        if !Self::carries_images(request) {
            return usable;
        }
        let sighted: Vec<String> = usable
            .into_iter()
            .filter(|name| {
                self.providers
                    .get(name)
                    .map(|provider| provider.vision() != Vision::No)
                    .unwrap_or(false)
            })
            .collect();
        sighted
    }

    /// The providers that would be asked about an image. For a client that wants to say "this
    /// model cannot see" before a goal is posted, rather than after a 400.
    pub fn vision_capable(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .providers
            .iter()
            .filter(|(_, provider)| provider.is_configured() && provider.vision() == Vision::Yes)
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// What each provider is, including whether it can be shown an image. Reported by /v1/models,
    /// because "why did my screenshot get answered by a text model?" is a configuration question.
    pub fn provider_vision(&self) -> Vec<(String, Vision)> {
        let mut rows: Vec<(String, Vision)> = self
            .providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.vision()))
            .collect();
        rows.sort();
        rows
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
            // Two very different situations, told apart on purpose: nothing is registered at all,
            // or everything registered is blind to the image this request carries.
            if Self::carries_images(&request) {
                return Err(RuntimeError::model(format!(
                    "no configured model can be shown an image: {} cannot see, and no vision-capable \
                     provider is set up. Configure a model that accepts images (for example \
                     model=\"deepseek-flash\" on the deepseek provider), or send the goal without the image.",
                    self.names().join(", ")
                ))
                .retryable(false));
            }
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
                        let mut response = response;
                        // Hand the caller the trail: an answer that came from the second choice must
                        // not be credited to the first one.
                        if !errors.is_empty() {
                            response.failed_over_from = errors.clone();
                        }
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
                vision: provider.vision(),
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
mod vision_tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::provider::{ChatMessage, ImageInput, ModelRequest, ModelResponse, ModelTask};
    use agentos_core::config::ProviderKind;

    /// A provider whose eyesight is whatever the test says it is.
    struct Eyed {
        name: String,
        vision: Vision,
    }

    #[async_trait::async_trait]
    impl ModelProvider for Eyed {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> ProviderKind {
            ProviderKind::Deepseek
        }
        fn model(&self) -> &str {
            "eyed-model"
        }
        fn vision(&self) -> Vision {
            self.vision
        }
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            Ok(ModelResponse::text(&self.name, "eyed-model", "seen"))
        }
    }

    fn router(providers: Vec<(&str, Vision)>, default: &str) -> ModelRouter {
        let mut policy = RoutingPolicy {
            default_provider: default.to_string(),
            fallback_chain: vec![],
            max_retries_per_provider: 1,
            ..Default::default()
        };
        for (index, (name, _)) in providers.iter().enumerate() {
            policy
                .priorities
                .insert(name.to_string(), 100 - index as u32);
        }
        let mut router = ModelRouter::new(policy);
        for (name, vision) in providers {
            router.register(Arc::new(Eyed { name: name.to_string(), vision }));
        }
        router
    }

    fn request_with_an_image() -> ModelRequest {
        let image = ImageInput { mime: "image/png".into(), base64: "aGVsbG8=".into() };
        ModelRequest::new(
            ModelTask::Think,
            vec![ChatMessage::user("what is this?").with_images(vec![image])],
        )
    }

    #[test]
    fn an_image_skips_a_model_that_says_it_cannot_see() {
        // The measured failure: deepseek-chat answers HTTP 400 to an image_url, the router failed
        // over, and the placeholder described a picture nobody had looked at.
        let router = router(vec![("blind", Vision::No), ("sighted", Vision::Yes)], "blind");
        let candidates = router.resolve(&request_with_an_image());
        assert_eq!(candidates, vec!["sighted".to_string()], "got {candidates:?}");
    }

    #[test]
    fn an_image_still_reaches_a_model_whose_eyesight_is_unknown() {
        // "Unknown" is not "no": a local server running a vision model with a name nobody
        // recognises must keep working.
        let router = router(vec![("mystery", Vision::Unknown), ("blind", Vision::No)], "mystery");
        let candidates = router.resolve(&request_with_an_image());
        assert_eq!(candidates, vec!["mystery".to_string()], "got {candidates:?}");
    }

    #[test]
    fn without_an_image_nothing_is_filtered() {
        let router = router(vec![("blind", Vision::No), ("sighted", Vision::Yes)], "blind");
        let plain = ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("hello")]);
        let candidates = router.resolve(&plain);
        assert_eq!(candidates.len(), 2, "text asks every provider, blind or not: {candidates:?}");
        assert_eq!(candidates[0], "blind", "the default still leads for text");
    }

    #[tokio::test]
    async fn an_image_with_no_sighted_provider_fails_with_the_fix_in_the_message() {
        let router = router(vec![("blind", Vision::No)], "blind");
        let error = router.complete(request_with_an_image()).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("no configured model can be shown an image"), "got {message}");
        assert!(message.contains("blind cannot see"), "the message names who was asked: {message}");
        assert!(
            message.contains("deepseek-flash"),
            "the error has to name a model that would work: {message}"
        );
    }

    #[tokio::test]
    async fn the_default_placeholder_never_answers_an_image_alone() {
        // The built-in provider has no eyes, and the whole point is that it does not pretend.
        let mut policy = RoutingPolicy {
            default_provider: "mock".into(),
            fallback_chain: vec!["mock".into()],
            ..Default::default()
        };
        policy.priorities.insert("mock".into(), 0);
        let mut router = ModelRouter::new(policy);
        router.register(Arc::new(MockProvider::default()));
        assert_eq!(router.vision_capable(), Vec::<String>::new());
        assert!(router.complete(request_with_an_image()).await.is_err());
    }
}

#[cfg(test)]
mod placeholder_tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::provider::{ChatMessage, ModelRequest, ModelResponse, ModelTask};
    use agentos_core::config::ProviderKind;

    /// A stand-in for a real provider: it says whether it is usable, and nothing else.
    struct Real {
        name: String,
        usable: bool,
    }

    #[async_trait::async_trait]
    impl ModelProvider for Real {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> ProviderKind {
            ProviderKind::Deepseek
        }
        fn model(&self) -> &str {
            "real-model"
        }
        fn is_configured(&self) -> bool {
            self.usable
        }
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            unreachable!("the test only asks who would be chosen")
        }
    }

    /// A stand-in for a provider that needs no credential, like a local endpoint.
    struct Keyless {
        name: String,
    }

    #[async_trait::async_trait]
    impl ModelProvider for Keyless {
        fn name(&self) -> &str {
            &self.name
        }
        fn kind(&self) -> ProviderKind {
            ProviderKind::Local
        }
        fn model(&self) -> &str {
            "local-model"
        }
        fn is_configured(&self) -> bool {
            true
        }
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            unreachable!("the test only asks who would be chosen")
        }
    }

    fn router(real_usable: bool, priorities: &[(&str, u32)]) -> ModelRouter {
        let mut policy = RoutingPolicy {
            default_provider: "mock".into(),
            fallback_chain: vec!["mock".into()],
            ..Default::default()
        };
        policy.priorities = priorities
            .iter()
            .map(|(name, priority)| (name.to_string(), *priority))
            .collect();
        let mut router = ModelRouter::new(policy);
        router.register(Arc::new(MockProvider::default()));
        for (name, _) in priorities {
            router.register(Arc::new(Real { name: name.to_string(), usable: real_usable }));
        }
        router
    }

    fn ask(router: &ModelRouter) -> Vec<String> {
        router.resolve(&ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("hi")]))
    }

    #[test]
    fn a_configured_real_provider_answers_before_the_placeholder() {
        // The bug this pins: with the placeholder as the configured default, a deployment that
        // had a working API key still got canned placeholder prose - and those turns then sat in
        // the conversation, where a real model copied their style.
        let candidates = ask(&router(true, &[("openai", 20), ("deepseek", 30)]));
        assert_eq!(candidates[0], "deepseek", "highest priority wins: {candidates:?}");
        assert_eq!(candidates[1], "openai");
        assert_eq!(candidates.last().unwrap(), "mock", "the placeholder is the fallback");
    }

    #[test]
    fn a_keyless_local_endpoint_does_not_jump_ahead_of_the_placeholder() {
        // A local endpoint needs no credential, so it always claims to be configured. Preferring
        // it would point every request at a server that may not be running: the run would sit
        // there until it timed out instead of being answered by the placeholder.
        let mut policy = RoutingPolicy {
            default_provider: "mock".into(),
            fallback_chain: vec!["mock".into()],
            ..Default::default()
        };
        policy.priorities = [("local".to_string(), 5)].into_iter().collect();
        let mut router = ModelRouter::new(policy);
        router.register(Arc::new(MockProvider::default()));
        router.register(Arc::new(Keyless { name: "local".into() }));
        let candidates = ask(&router);
        assert_eq!(candidates[0], "mock", "got {candidates:?}");
    }

    #[test]
    fn with_nothing_else_usable_the_placeholder_still_answers() {
        // Offline deployments keep working: no key, no network, no problem.
        let candidates = ask(&router(false, &[("deepseek", 30)]));
        assert_eq!(candidates[0], "mock", "got {candidates:?}");
    }

    #[test]
    fn asking_for_the_placeholder_explicitly_still_gets_it() {
        let router = router(true, &[("deepseek", 30)]);
        let mut request = ModelRequest::new(ModelTask::Think, vec![ChatMessage::user("hi")]);
        request.model_hint = Some("mock".into());
        assert_eq!(router.resolve(&request)[0], "mock", "an explicit choice is not overridden");
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
                    failed_over_from: vec![],
                    reasoning: String::new(),
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
