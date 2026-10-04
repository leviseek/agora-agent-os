//! Observability primitives: tracing bootstrap, correlation context, in-process metrics.
//!
//! Every log line and every metric in the runtime is expected to carry at least: request_id,
//! trace_id, and - when applicable - session_id, actor_id, task_id. That is what makes a
//! distributed agent run debuggable without a debugger.

use crate::error::Result;
use crate::ids::{ActorId, RequestId, SessionId, TaskId, TraceId};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Install the global subscriber. Safe to call more than once.
pub fn init_tracing(level: &str, json: bool) -> Result<()> {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);
    if json {
        let layer = tracing_subscriber::fmt::layer().json().with_target(true);
        let _ = registry.with(layer).try_init();
    } else {
        let layer = tracing_subscriber::fmt::layer().with_target(true).compact();
        let _ = registry.with(layer).try_init();
    }
    Ok(())
}

/// Correlation identifiers propagated across gateway -> control plane -> data plane.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Correlation {
    pub request_id: Option<String>,
    pub trace_id: Option<String>,
    pub session_id: Option<String>,
    pub actor_id: Option<String>,
    pub task_id: Option<String>,
}

impl Correlation {
    /// Fresh correlation for an inbound request.
    pub fn new() -> Self {
        Self {
            request_id: Some(RequestId::new().into_string()),
            trace_id: Some(TraceId::new().into_string()),
            ..Default::default()
        }
    }

    pub fn with_session(mut self, id: &SessionId) -> Self {
        self.session_id = Some(id.to_string());
        self
    }
    pub fn with_actor(mut self, id: &ActorId) -> Self {
        self.actor_id = Some(id.to_string());
        self
    }
    pub fn with_task(mut self, id: &TaskId) -> Self {
        self.task_id = Some(id.to_string());
        self
    }

    pub fn trace(&self) -> String {
        self.trace_id.clone().unwrap_or_default()
    }
    pub fn request(&self) -> String {
        self.request_id.clone().unwrap_or_default()
    }

    /// Build a tracing span carrying the full correlation set.
    pub fn span(&self, name: &'static str) -> tracing::Span {
        tracing::info_span!(
            "agentos",
            op = name,
            request_id = %self.request(),
            trace_id = %self.trace(),
            session_id = %self.session_id.clone().unwrap_or_default(),
            actor_id = %self.actor_id.clone().unwrap_or_default(),
            task_id = %self.task_id.clone().unwrap_or_default(),
        )
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "request_id": self.request_id,
            "trace_id": self.trace_id,
            "session_id": self.session_id,
            "actor_id": self.actor_id,
            "task_id": self.task_id,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Histogram {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

impl Histogram {
    pub fn observe(&mut self, v: f64) {
        if self.count == 0 {
            self.min = v;
            self.max = v;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.count += 1;
        self.sum += v;
    }
    pub fn mean(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.sum / self.count as f64 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub counters: BTreeMap<String, i64>,
    pub gauges: BTreeMap<String, f64>,
    pub histograms: BTreeMap<String, Histogram>,
}

/// A tiny, dependency-free metrics registry. Deliberately not Prometheus-client: the surface
/// we need is counters, gauges and summary histograms, all exportable as text.
#[derive(Default)]
pub struct Metrics {
    counters: RwLock<BTreeMap<String, i64>>,
    gauges: RwLock<BTreeMap<String, f64>>,
    histograms: RwLock<BTreeMap<String, Histogram>>,
}

/// Render labels deterministically: sorted, quoted, comma separated.
pub fn labels(pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<_> = pairs.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let inner = sorted
        .iter()
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{inner}}}")
}

impl Metrics {
    pub fn inc(&self, name: &str, by: i64) {
        *self.counters.write().entry(name.to_string()).or_insert(0) += by;
    }

    pub fn inc_by(&self, name: &str, pairs: &[(&str, &str)], by: i64) {
        self.inc(&(name.to_string() + &labels(pairs)), by);
    }

    pub fn gauge(&self, name: &str, value: f64) {
        self.gauges.write().insert(name.to_string(), value);
    }

    pub fn observe(&self, name: &str, value: f64) {
        self.histograms.write().entry(name.to_string()).or_default().observe(value);
    }

    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();
        for (k, v) in self.counters.read().iter() {
            out.push_str(&format!("{k} {v}\n"));
        }
        for (k, v) in self.gauges.read().iter() {
            out.push_str(&format!("{k} {v}\n"));
        }
        for (k, h) in self.histograms.read().iter() {
            out.push_str(&format!("{k}_count {}\n{k}_sum {}\n{k}_min {}\n{k}_max {}\n", h.count, h.sum, h.min, h.max));
        }
        out
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            counters: self.counters.read().clone(),
            gauges: self.gauges.read().clone(),
            histograms: self.histograms.read().clone(),
        }
    }
}

static GLOBAL_METRICS: OnceLock<Metrics> = OnceLock::new();

/// Process-wide metrics handle used by every plane.
pub fn metrics() -> &'static Metrics {
    GLOBAL_METRICS.get_or_init(Metrics::default)
}

/// Canonical metric names, so dashboards and tests agree on spelling.
pub mod metric_names {
    pub const HTTP_REQUESTS: &str = "agentos_http_requests_total";
    pub const HTTP_LATENCY_MS: &str = "agentos_http_latency_ms";
    pub const SESSION_CREATED: &str = "agentos_sessions_created_total";
    pub const SESSION_MESSAGES: &str = "agentos_session_messages_total";
    pub const ACTOR_MAILBOX_DEPTH: &str = "agentos_actor_mailbox_depth";
    pub const ACTOR_RESTARTS: &str = "agentos_actor_restarts_total";
    pub const TASK_STATE: &str = "agentos_tasks_total";
    pub const TASK_LATENCY_MS: &str = "agentos_task_latency_ms";
    pub const CAPABILITY_CALLS: &str = "agentos_capability_calls_total";
    pub const CAPABILITY_LATENCY_MS: &str = "agentos_capability_latency_ms";
    pub const MODEL_CALLS: &str = "agentos_model_calls_total";
    pub const MODEL_LATENCY_MS: &str = "agentos_model_latency_ms";
    pub const MODEL_TOKENS: &str = "agentos_model_tokens_total";
    pub const EVENTS_PUBLISHED: &str = "agentos_events_published_total";
    pub const MIGRATIONS: &str = "agentos_actor_migrations_total";
    pub const WORKERS_ONLINE: &str = "agentos_workers_online";
    pub const POLICY_DENIED: &str = "agentos_policy_denied_total";
    pub const WASM_INVOCATIONS: &str = "agentos_wasm_invocations_total";
    pub const CACHE_HITS: &str = "agentos_directory_cache_hits_total";
    pub const CACHE_MISSES: &str = "agentos_directory_cache_misses_total";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_sorted_and_quoted() {
        assert_eq!(labels(&[("b", "2"), ("a", "1")]), "{a=\"1\",b=\"2\"}");
        assert_eq!(labels(&[]), "");
    }

    #[test]
    fn counters_accumulate_and_export() {
        let m = Metrics::default();
        m.inc("x_total", 1);
        m.inc("x_total", 2);
        m.gauge("y", 4.5);
        m.observe("z_ms", 10.0);
        m.observe("z_ms", 20.0);
        let snap = m.snapshot();
        assert_eq!(snap.counters["x_total"], 3);
        assert_eq!(snap.gauges["y"], 4.5);
        assert_eq!(snap.histograms["z_ms"].count, 2);
        assert_eq!(snap.histograms["z_ms"].min, 10.0);
        assert_eq!(snap.histograms["z_ms"].max, 20.0);
        assert!(m.render_prometheus().contains("x_total 3"));
    }

    #[test]
    fn correlation_carries_ids() {
        let c = Correlation::new();
        assert!(c.trace().starts_with("trc_"));
        assert!(c.request().starts_with("req_"));
        let c = c.with_session(&SessionId::new());
        assert!(c.to_json()["session_id"].as_str().unwrap().starts_with("ses_"));
    }
}
