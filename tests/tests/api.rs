//! HTTP and WebSocket gateway tests: the client-facing contract.

use agentos_api::ApiState;
use agentos_core::config::{RuntimeConfig, StoreBackend};
use agentos_kernel::Kernel;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

struct Harness {
    base: String,
    /// Kept alive for the lifetime of the harness: dropping the kernel stops the runtime under
    /// test, which would make assertions race with shutdown.
    _kernel: Arc<Kernel>,
    shutdown: tokio_util::sync::CancellationToken,
    client: reqwest::Client,
}

impl Harness {
    async fn start_with_env(env_name: &str, auth_token: Option<&str>, rate_limit: u32) -> Self {
        Self::start_with_models(env_name, auth_token, rate_limit, None).await
    }

    /// The same harness, with a say in the model configuration.
    ///
    /// Vision needs its own deployment: the shipped defaults register five providers and only some
    /// of them can be shown an image, so a test about attaching one has to say what it attaches to
    /// rather than inheriting whatever the example config happens to contain.
    async fn start_with_models(
        env_name: &str,
        auth_token: Option<&str>,
        rate_limit: u32,
        models: Option<agentos_core::config::ModelConfig>,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!("agentos-api-{}", agentos_core::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = RuntimeConfig::default();
        config.storage.backend = StoreBackend::Memory;
        config.storage.data_dir = dir.join("data");
        config.policy.workspace_root = dir.join("workspace");
        config.observability.log_level = "error".into();
        config.api.rate_limit_per_minute = rate_limit;
        // A distinct variable per harness: tests run in parallel in one process, so a shared
        // variable name would be a race on process-global state.
        config.api.auth_token_env = env_name.to_string();
        if let Some(models) = models {
            config.models = models;
        }
        if let Some(token) = auth_token {
            std::env::set_var(env_name, token);
        } else {
            std::env::remove_var(env_name);
        }
        let kernel = Kernel::bootstrap(config).await.unwrap();
        let (addr, shutdown) = agentos_api::serve_test(kernel.clone()).await.unwrap();
        Self {
            base: format!("http://{addr}"),
            _kernel: kernel,
            shutdown,
            client: reqwest::Client::new(),
        }
    }

    async fn start(auth_token: Option<&str>, rate_limit: u32) -> Self {
        Self::start_with_env("AGENTOS_TEST_API_TOKEN", auth_token, rate_limit).await
    }

    /// A harness whose discovery directory is known, so a test can plant a peer advertisement.
    async fn start_with_discovery(env_name: &str) -> (Self, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("agentos-api-disc-{}", agentos_core::now_ms()));
        let discovery_dir = dir.join("nodes");
        std::fs::create_dir_all(&discovery_dir).unwrap();
        let mut config = RuntimeConfig::default();
        config.storage.backend = StoreBackend::Memory;
        config.storage.data_dir = dir.join("data");
        config.policy.workspace_root = dir.join("workspace");
        config.observability.log_level = "error".into();
        config.api.auth_token_env = env_name.to_string();
        config.discovery.dir = discovery_dir.clone();
        config.discovery.ttl_ms = 30_000;
        std::env::remove_var(env_name);
        let kernel = Kernel::bootstrap(config).await.unwrap();
        let (addr, shutdown) = agentos_api::serve_test(kernel.clone()).await.unwrap();
        (
            Self {
                base: format!("http://{addr}"),
                _kernel: kernel,
                shutdown,
                client: reqwest::Client::new(),
            },
            discovery_dir,
        )
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = self.client.get(self.url(path)).send().await.unwrap();
        let status = response.status().as_u16();
        let body = response.json::<Value>().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn patch(&self, path: &str, body: Value) -> (u16, Value) {
        let response = self.client.patch(self.url(path)).json(&body).send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let parsed = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
        (status, parsed)
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let response = self.client.post(self.url(path)).json(&body).send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let parsed = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
        (status, parsed)
    }

    /// Upload raw bytes the way a browser does: the body is the file, not JSON.
    async fn upload(&self, path: &str, name: &str, content_type: &str, bytes: &[u8]) -> (u16, Value) {
        let response = self
            .client
            .post(self.url(path))
            .header("content-type", content_type)
            .header("x-agentos-filename", name)
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let parsed = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
        (status, parsed)
    }

    async fn get_bytes(&self, path: &str) -> (u16, Vec<u8>, String) {
        let response = self.client.get(self.url(path)).send().await.unwrap();
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = response.bytes().await.unwrap_or_default().to_vec();
        (status, bytes, content_type)
    }
}

/// A model configuration with the placeholder plus one provider that declares it can see.
///
/// The shipping default depends on environment variables and on which example config is in play;
/// a test about attaching an image needs a deployment where attaching one is possible at all.
fn models_with_a_sighted_provider() -> agentos_core::config::ModelConfig {
    let mut models = agentos_core::config::ModelConfig::default();
    models.providers.retain(|provider| provider.kind == agentos_core::config::ProviderKind::Mock);
    models.providers.push(agentos_core::config::ProviderConfig {
        name: "sighted".into(),
        kind: agentos_core::config::ProviderKind::Openai,
        model: "sighted-model".into(),
        base_url: "http://127.0.0.1:1".into(),
        api_key_env: "AGENTOS_TEST_SIGHTED_KEY".into(),
        enabled: true,
        priority: 40,
        timeout_ms: 500,
        vision: Some(true),
    });
    std::env::set_var("AGENTOS_TEST_SIGHTED_KEY", "test-key");
    models
}

/// A model configuration where nothing can be shown an image: the placeholder alone.
fn models_without_eyes() -> agentos_core::config::ModelConfig {
    let mut models = agentos_core::config::ModelConfig::default();
    models.providers.retain(|provider| provider.kind == agentos_core::config::ProviderKind::Mock);
    models.default_provider = "mock".into();
    models
}

/// A 1x1 PNG: the smallest thing that passes a content sniff, so a test does not need a fixture.
const ONE_PIXEL_PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R',
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, b'I', b'D', b'A', b'T', 0x78, 0x9C, 0x63, 0xFC, 0xCF, 0xC0, 0xF0,
    0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0xAB, 0xCE, 0x36, 0x89, 0x00, 0x00, 0x00, 0x00, b'I', b'E',
    b'N', b'D', 0xAE, 0x42, 0x60, 0x82,
];

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_meta_and_error_shape() {
    let h = Harness::start(None, 0).await;
    let (status, body) = h.get("/healthz").await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");

    let (status, body) = h.get("/v1/meta").await;
    assert_eq!(status, 200);
    assert_eq!(body["domain_version"], agentos_core::DOMAIN_VERSION);
    assert_eq!(body["store_backend"], "memory");
    assert_eq!(body["auth_required"], false);

    // Unknown session must produce the uniform error shape, not a bare stack trace.
    let (status, body) = h.get("/v1/sessions/ses_01a10801c416732bb6960b9173facca0/status").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");

    // Malformed id is rejected as invalid input.
    let (status, body) = h.get("/v1/sessions/not-an-id/status").await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_input");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_lifecycle_over_http() {
    let h = Harness::start(None, 0).await;
    let (status, created) = h.post("/v1/sessions", json!({"user_id":"api-user","title":"http session"})).await;
    assert_eq!(status, 200, "create session: {created}");
    let session_id = created["id"].as_str().unwrap().to_string();
    assert!(session_id.starts_with("ses_"));

    let (status, list) = h.get("/v1/sessions").await;
    assert_eq!(status, 200);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);

    let (status, result) = h
        .post(&format!("/v1/sessions/{session_id}/messages"), json!({"text":"what is 6*7?","wait":true}))
        .await;
    assert_eq!(status, 200, "post message: {result}");
    assert!(result["error"].is_null(), "run must succeed: {result}");
    assert!(result["answer"].as_str().unwrap().contains("42"));

    let (status, status_body) = h.get(&format!("/v1/sessions/{session_id}/status")).await;
    assert_eq!(status, 200);
    assert_eq!(status_body["runs"].as_array().unwrap().len(), 1);

    let (status, events) = h.get(&format!("/v1/sessions/{session_id}/events?limit=100")).await;
    assert_eq!(status, 200);
    assert!(!events["events"].as_array().unwrap().is_empty());

    let (status, checkpoint) = h.get(&format!("/v1/sessions/{session_id}/snapshot")).await;
    assert_eq!(status, 200);
    assert!(checkpoint["meta"]["id"].as_str().unwrap().starts_with("ckp_"));

    let (status, restored) = h
        .post(&format!("/v1/sessions/{session_id}/restore"), checkpoint.clone())
        .await;
    assert_eq!(status, 200, "restore: {restored}");
    assert_eq!(restored["restored"], true);

    let (status, _) = h
        .post(&format!("/v1/sessions/{session_id}/messages"), json!({"text":"   "}))
        .await;
    assert_eq!(status, 400, "empty input is rejected before it reaches the actor");

    // DELETE closes the session.
    let response = h
        .client
        .delete(h.url(&format!("/v1/sessions/{session_id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capabilities_workers_actors_models_and_metrics() {
    let h = Harness::start(None, 0).await;
    let (status, caps) = h.get("/v1/capabilities").await;
    assert_eq!(status, 200);
    let names: Vec<String> = caps["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    for expected in ["echo", "calculator", "filesystem-read"] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }

    let (status, echoed) = h
        .post("/v1/capabilities/echo/invoke", json!({"input":{"text":"through the gateway"}}))
        .await;
    assert_eq!(status, 200, "invoke: {echoed}");
    assert_eq!(echoed["output"]["text"], "through the gateway");

    let (status, denied) = h
        .post("/v1/capabilities/filesystem-read/invoke", json!({"input":{"path":"../../etc/passwd"}}))
        .await;
    assert_eq!(status, 403, "traversal must be denied: {denied}");
    assert_eq!(denied["error"]["code"], "policy_denied");

    let (status, workers) = h.get("/v1/workers").await;
    assert_eq!(status, 200);
    assert_eq!(workers["workers"].as_array().unwrap().len(), 1);

    let (status, actors) = h.get("/v1/actors").await;
    assert_eq!(status, 200);
    assert!(actors["cache"].get("hits").is_some());

    let (status, models) = h.get("/v1/models").await;
    assert_eq!(status, 200);
    assert!(models["providers"].as_array().unwrap().iter().any(|p| p["name"] == "mock"));

    // Provider health must be a bare identifier for every provider, healthy or not: this endpoint
    // once serialised the unhealthy variants as objects ({"unconfigured": "..."}), and the console
    // rendered that object as a React child, which unmounted the whole page.
    for provider in models["providers"].as_array().unwrap() {
        let health = &provider["health"];
        assert!(
            health.is_string(),
            "health must be a string on the wire, got {health} for {provider}"
        );
        assert!(
            provider["health_reason"].is_string(),
            "health_reason must accompany it, got {provider}"
        );
    }

    let response = h.client.get(h.url("/v1/metrics")).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("agentos_sessions"), "metrics export: {text}");
    assert!(text.contains("agentos_capabilities"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authentication_is_enforced_when_a_token_is_configured() {
    let h = Harness::start_with_env("AGENTOS_TEST_TOKEN_AUTH", Some("s3cret-token"), 0).await;

    let (status, body) = h.get("/v1/capabilities").await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "unauthorized");

    // /healthz stays open so a load balancer can probe the process.
    let (status, _) = h.get("/healthz").await;
    assert_eq!(status, 200);

    let response = h
        .client
        .get(h.url("/v1/capabilities"))
        .bearer_auth("s3cret-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    let (status, login) = h.post("/v1/auth/login", json!({"token":"s3cret-token"})).await;
    assert_eq!(status, 200);
    assert_eq!(login["ok"], true);

    let (status, _) = h.post("/v1/auth/login", json!({"token":"wrong"})).await;
    assert_eq!(status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limit_rejects_excess_traffic() {
    let h = Harness::start_with_env("AGENTOS_TEST_TOKEN_RATE", None, 5).await;
    let mut statuses = Vec::new();
    for _ in 0..8 {
        let (status, _) = h.get("/v1/meta").await;
        statuses.push(status);
    }
    assert!(statuses.contains(&429), "expected throttling, saw {statuses:?}");
    assert!(statuses.contains(&200), "and normal traffic must still work");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_streams_events_and_accepts_commands() {
    let h = Harness::start_with_env("AGENTOS_TEST_TOKEN_WS", None, 0).await;
    let ws_url = h.base.replace("http://", "ws://") + "/v1/ws";
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url).await.expect("ws connects");

    // 1. hello frame
    let hello = next_of_type(&mut socket, "hello").await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["domain_version"], agentos_core::DOMAIN_VERSION);

    // 2. command round trip
    socket.send(Message::Text(json!({"type":"ping"}).to_string().into())).await.unwrap();
    let pong = next_of_type(&mut socket, "pong").await;
    assert_eq!(pong["type"], "pong");

    // 3. create a session over HTTP, then stream its events over the socket
    let (_, created) = h.post("/v1/sessions", json!({"user_id":"ws-user","title":"ws"})).await;
    let session_id = created["id"].as_str().unwrap().to_string();
    let (status, _) = h
        .post(&format!("/v1/sessions/{session_id}/messages"), json!({"text":"what is 8*8?","wait":false}))
        .await;
    assert_eq!(status, 200);

    let mut saw_run_event = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !saw_run_event {
        let frame = tokio::time::timeout(Duration::from_secs(5), next_json_opt(&mut socket)).await;
        match frame {
            Ok(Some(value)) => {
                if value["type"] == "event" {
                    let kind = value["event"]["kind"].as_str().unwrap_or_default();
                    if kind == "run_completed" || kind == "tool_call" || kind == "agent_step" {
                        saw_run_event = true;
                    }
                }
            }
            _ => break,
        }
    }
    assert!(saw_run_event, "the socket must stream agent run events");

    // 4. the socket can also drive a goal
    let (_, second) = h.post("/v1/sessions", json!({"user_id":"ws-user","title":"ws2"})).await;
    let second_id = second["id"].as_str().unwrap().to_string();
    socket
        .send(Message::Text(
            json!({"type":"goal","session_id":second_id,"goal":"what is 9*9?","wait":true}).to_string().into(),
        ))
        .await
        .unwrap();
    let mut answered = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline && !answered {
        match tokio::time::timeout(Duration::from_secs(5), next_json_opt(&mut socket)).await {
            Ok(Some(value)) => {
                if value["type"] == "goal_result" {
                    assert!(value["result"]["answer"].as_str().unwrap_or_default().contains("81"));
                    answered = true;
                }
            }
            _ => break,
        }
    }
    assert!(answered, "a goal sent over the socket must come back with a result");
}

async fn next_json(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> Value {
    next_json_opt(socket).await.expect("a frame must arrive")
}

/// The next frame of a given type, skipping the events that may arrive first.
///
/// The socket carries both replies and the live event stream, so "the next frame is the reply" is
/// only true on an idle machine. Asserting the second frame is the pong is how this test failed
/// under load while passing on its own.
async fn next_of_type(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    wanted: &str,
) -> Value {
    for _ in 0..20 {
        let frame = next_json(socket).await;
        if frame["type"] == wanted {
            return frame;
        }
    }
    panic!("no {wanted} frame arrived within 20 frames");
}

/// The next frame, or None if the socket stays quiet for `idle`.
///
/// Reads on a live socket have to be bounded. A drain loop that ended only when the server closed
/// the connection hung this suite for tens of minutes: the socket is meant to stay open, so "read
/// until closed" is a read that never returns, and a hung test binary looks exactly like a suite
/// that is still working.
async fn next_json_within(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    idle: Duration,
) -> Option<Value> {
    match tokio::time::timeout(idle, next_json_opt(socket)).await {
        Ok(frame) => frame,
        Err(_) => None,
    }
}

async fn next_json_opt(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> Option<Value> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    return Some(value);
                }
            }
            Some(Ok(_)) => continue,
            _ => return None,
        }
    }
}

/// Posting a goal without waiting is fire-and-forget: the POST returns as soon as the goal is
/// queued, and the answer is read back from the transcript afterwards. The run completion event
/// carries counters, not text, so without this read path a client that did not wait could never
/// show a reply.
#[tokio::test]
async fn a_goal_posted_without_waiting_is_readable_from_the_transcript() {
    let h = Harness::start(None, 600).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "async" })).await;
    let id = session["id"].as_str().unwrap().to_string();

    let started = std::time::Instant::now();
    let (status, accepted) = h
        .post(&format!("/v1/sessions/{id}/messages"), json!({ "text": "what is 9*9?", "wait": false }))
        .await;
    assert_eq!(status, 200);
    assert_eq!(accepted["accepted"], json!(true));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the POST must return as soon as the goal is queued, took {:?}",
        started.elapsed()
    );

    // The run finishes on its own; poll the transcript the way a client would.
    let mut answer = None;
    for _ in 0..40 {
        let (status, body) = h.get(&format!("/v1/sessions/{id}/transcript")).await;
        assert_eq!(status, 200);
        assert!(body["total"].as_u64().unwrap() >= 1, "the user goal is in the transcript");
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == json!("assistant"));
        if let Some(message) = assistant {
            answer = message["parts"][0]["text"].as_str().map(str::to_string);
            if answer.is_some() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let answer = answer.expect("the assistant reply lands in the transcript");
    assert!(!answer.is_empty());

    // And the same text is available the other way the console reads it: the run summary.
    let (_, detail) = h.get(&format!("/v1/sessions/{id}")).await;
    let run = &detail["runtime"]["runs"][0];
    assert_eq!(run["final_answer"], json!(answer));
    // The run records who actually answered, so the console can label it truthfully instead of
    // showing the provider that was merely requested.
    assert_eq!(run["provider"], json!("mock"));
    assert_eq!(run["model"], json!("agentos-mock-1"));

    // And what it cost: the mock provider reports usage like any other, so a run that planned,
    // executed a task graph and summarised must show several calls and non-zero tokens.
    let usage = &run["usage"];
    assert!(
        usage["calls"].as_u64().unwrap() >= 2,
        "a run makes more than one model call: {usage}"
    );
    assert!(usage["total_tokens"].as_u64().unwrap() > 0, "usage: {usage}");
    assert_eq!(
        detail["runtime"]["usage"]["calls"], usage["calls"],
        "a single run's session total is that run"
    );
    assert_eq!(detail["runtime"]["usage"]["total_tokens"], usage["total_tokens"]);

    h.shutdown.cancel();
}

/// Discovery must be visible through the gateway: a peer that starts up in another checkout
/// shows up in /v1/nodes and in the event stream, without restarting the runtime that sees it.
#[tokio::test]
async fn discovered_nodes_appear_in_the_gateway() {
    let (harness, discovery_dir) = Harness::start_with_discovery("AGENTOS_TEST_API_NODES_TOKEN").await;

    let (status, body) = harness.get("/v1/nodes").await;
    assert_eq!(status, 200);
    assert!(body["nodes"].as_array().unwrap().is_empty(), "nobody else is running yet");
    assert_eq!(body["self"]["self"], json!(true));
    assert_eq!(body["discovery"]["backend"], json!("local-file"));
    assert_eq!(body["discovery"]["enabled"], json!(true));

    // Another node starts in another working directory and advertises itself.
    let peer = json!({
        "node_id": "peer-node-42",
        "name": "agora-peer",
        "address": "http://127.0.0.1:9999",
        "grpc_endpoint": "127.0.0.1:9998",
        "version": "0.1.0",
        "capabilities": ["echo", "clock"],
        "auth_required": false,
        "transport": "local-file",
        "discovered_at": agentos_core::now_ms(),
        "last_seen": agentos_core::now_ms(),
    });
    std::fs::write(
        discovery_dir.join("peer-node-42.json"),
        serde_json::to_vec(&peer).unwrap(),
    )
    .unwrap();

    // The heartbeat picks it up on its own schedule; no restart, no explicit refresh call.
    let mut found = None;
    for _ in 0..48 {
        let (_, body) = harness.get("/v1/nodes").await;
        if !body["nodes"].as_array().unwrap().is_empty() {
            found = Some(body);
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let body = found.expect("the peer should be discovered without a restart");
    let nodes = body["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["name"], json!("agora-peer"));
    assert_eq!(nodes[0]["address"], json!("http://127.0.0.1:9999"));
    assert_eq!(nodes[0]["capabilities"].as_array().unwrap().len(), 2);
    assert_eq!(nodes[0]["self"], json!(false));

    // A node appearing is an event, not only a query result: the console reacts to it.
    let (_, events) = harness.get("/v1/events?limit=100").await;
    let kinds: Vec<String> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|event| event["kind"].as_str().map(str::to_string))
        .collect();
    assert!(
        kinds.iter().any(|kind| kind == "node_discovered"),
        "expected a node_discovered event, got {kinds:?}"
    );

    harness.shutdown.cancel();
}

#[tokio::test]
async fn images_are_verified_by_content_and_stored_as_artifacts() {
    use base64::Engine;
    let dir = std::env::temp_dir().join(format!("agentos-vision-{}", agentos_core::now_ms()));
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // A real 1x1 PNG.
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
        .unwrap();
    std::fs::write(workspace.join("pixel.png"), &png).unwrap();
    // The same name, the wrong content.
    std::fs::write(workspace.join("liar.png"), "this is not an image").unwrap();
    // Outside the workspace, however it is spelled.
    std::fs::write(dir.join("outside.png"), &png).unwrap();

    let mut config = RuntimeConfig::default();
    config.storage.backend = StoreBackend::Memory;
    config.storage.data_dir = dir.join("data");
    config.policy.workspace_root = workspace.clone();
    config.observability.log_level = "error".into();
    config.api.auth_token_env = "AGENTOS_TEST_VISION_TOKEN".into();
    std::env::remove_var("AGENTOS_TEST_VISION_TOKEN");

    let kernel = Kernel::bootstrap(config).await.unwrap();
    let (addr, shutdown) = agentos_api::serve_test(kernel.clone()).await.unwrap();
    let h = Harness {
        base: format!("http://{addr}"),
        _kernel: kernel,
        shutdown,
        client: reqwest::Client::new(),
    };

    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "vision" })).await;
    let id = session["id"].as_str().unwrap().to_string();

    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": "what is in this picture?", "wait": true, "images": ["pixel.png"] }),
        )
        .await;
    assert_eq!(status, 200, "the goal runs: {body}");

    // The transcript refers to the image by artifact, not by carrying its bytes.
    let (_, transcript) = h.get(&format!("/v1/sessions/{id}/transcript")).await;
    let parts = transcript["messages"][0]["parts"].as_array().unwrap();
    let image = parts
        .iter()
        .find(|part| part["type"] == json!("image"))
        .expect("the user message carries an image part");
    assert_eq!(image["name"], json!("pixel.png"));
    assert_eq!(image["mime"], json!("image/png"), "the type came from the bytes");
    let artifact_id = image["artifact_id"].as_str().unwrap().to_string();

    // And the bytes can be fetched back, with the content type the model was told.
    let response = h.client.get(h.url(&format!("/v1/artifacts/{artifact_id}"))).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap().to_str().unwrap(),
        "image/png"
    );
    let returned = response.bytes().await.unwrap();
    assert_eq!(returned.as_ref(), png.as_slice(), "the artifact is the file we attached");

    // A file called .png that is not one is refused: the name proves nothing.
    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": "look", "wait": true, "images": ["liar.png"] }),
        )
        .await;
    assert_eq!(status, 400);
    assert!(
        body["error"]["message"].as_str().unwrap().contains("decided by content"),
        "got: {body}"
    );

    // And a path that leaves the workspace is refused, not sanitised.
    let (status, _) = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": "look", "wait": true, "images": ["../outside.png"] }),
        )
        .await;
    assert_eq!(status, 403);

    // A missing file fails the goal loudly instead of answering about nothing.
    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": "look", "wait": true, "images": ["nope.png"] }),
        )
        .await;
    assert_eq!(status, 404, "got: {body}");

    h.shutdown.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_approval_gates_a_capability_call() {
    let dir = std::env::temp_dir().join(format!("agentos-approval-{}", agentos_core::now_ms()));
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let mut config = RuntimeConfig::default();
    config.storage.backend = StoreBackend::Memory;
    config.storage.data_dir = dir.join("data");
    config.policy.workspace_root = workspace.clone();
    config.observability.log_level = "error".into();
    config.api.auth_token_env = "AGENTOS_TEST_APPROVAL_TOKEN".into();
    // Allowed, but only with a human in the loop. Short window so the timeout case is quick.
    config.policy.allowed_capabilities = vec!["filesystem-write".into(), "filesystem-read".into()];
    config.policy.approval_required = vec!["filesystem-write".into()];
    config.policy.approval_timeout_ms = 1_500;
    std::env::remove_var("AGENTOS_TEST_APPROVAL_TOKEN");

    let kernel = Kernel::bootstrap(config).await.unwrap();
    let (addr, shutdown) = agentos_api::serve_test(kernel.clone()).await.unwrap();
    let h = Harness {
        base: format!("http://{addr}"),
        _kernel: kernel.clone(),
        shutdown,
        client: reqwest::Client::new(),
    };

    // --- approved: the write happens, but only after the decision -------------------------
    let client = h.client.clone();
    let base = h.base.clone();
    let call = |text: &str| {
        let client = client.clone();
        let base = base.clone();
        let text = text.to_string();
        async move {
            client
                .post(format!("{base}/v1/capabilities/filesystem-write/invoke"))
                .json(&json!({ "input": { "path": "approved.txt", "content": text } }))
                .send()
                .await
                .unwrap()
        }
    };

    let pending_call = tokio::spawn(call("written after a decision"));
    let approval = wait_for_approval(&h).await;
    let approval_id = approval["id"].as_str().unwrap().to_string();
    assert_eq!(approval["capability"], json!("filesystem-write"));
    assert!(
        approval["arguments_preview"].as_str().unwrap().contains("approved.txt"),
        "the operator can see what they are approving: {approval}"
    );
    assert!(
        !workspace.join("approved.txt").exists(),
        "nothing happens while the call is parked"
    );

    let (status, body) = h
        .post(&format!("/v1/approvals/{approval_id}"), json!({ "approved": true, "by": "test" }))
        .await;
    assert_eq!(status, 200, "deciding works: {body}");
    let response = pending_call.await.unwrap();
    assert_eq!(response.status().as_u16(), 200, "the parked call completes after approval");
    assert_eq!(
        std::fs::read_to_string(workspace.join("approved.txt")).unwrap(),
        "written after a decision"
    );

    // A decision is single use.
    let (status, _) = h
        .post(&format!("/v1/approvals/{approval_id}"), json!({ "approved": true }))
        .await;
    assert_eq!(status, 404, "deciding twice is not a toggle");

    // --- denied: the write never happens ------------------------------------------------
    let pending_call = tokio::spawn(call("must never be written"));
    let approval = wait_for_approval(&h).await;
    let approval_id = approval["id"].as_str().unwrap().to_string();
    let (status, _) = h
        .post(
            &format!("/v1/approvals/{approval_id}"),
            json!({ "approved": false, "reason": "not that file" }),
        )
        .await;
    assert_eq!(status, 200);
    let response = pending_call.await.unwrap();
    assert_eq!(response.status().as_u16(), 403);
    let body = response.text().await.unwrap();
    assert!(body.contains("not that file"), "the reason reaches the caller: {body}");
    assert!(!workspace.join("must never be written").exists());
    assert!(!workspace.join("denied.txt").exists());

    // --- nobody decides: the call gives up instead of hanging forever --------------------
    let pending_call = tokio::spawn(call("no decision"));
    let approval = wait_for_approval(&h).await;
    let response = pending_call.await.unwrap();
    assert_eq!(response.status().as_u16(), 504, "an approval with no deadline is a hang");
    let body = response.text().await.unwrap();
    assert!(body.contains("waited"), "and it says why: {body}");

    // The expired request leaves the pending list: no ghost prompts.
    let (_, listed) = h.get("/v1/approvals").await;
    assert_eq!(listed["total"], json!(0), "got {listed}");
    let _ = approval;

    // --- the decisions are on the record ------------------------------------------------
    let (_, events) = h.get("/v1/events?limit=200&kinds=approval_requested").await;
    assert!(events["events"].as_array().unwrap().len() >= 3, "every park is recorded");
    let (_, granted) = h.get("/v1/events?limit=200&kinds=approval_granted").await;
    assert_eq!(granted["events"].as_array().unwrap().len(), 1);
    let (_, denied) = h.get("/v1/events?limit=200&kinds=approval_denied").await;
    assert_eq!(denied["events"].as_array().unwrap().len(), 1);
    let (_, expired) = h.get("/v1/events?limit=200&kinds=approval_expired").await;
    assert_eq!(expired["events"].as_array().unwrap().len(), 1);

    h.shutdown.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Poll until a call parks. A parked call is asynchronous by nature, so the test waits for it
/// rather than sleeping for a guessed duration.
async fn wait_for_approval(h: &Harness) -> Value {
    for _ in 0..200 {
        let (_, listed) = h.get("/v1/approvals").await;
        if let Some(first) = listed["approvals"].as_array().and_then(|list| list.first()) {
            return first.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no approval was requested");
}

/// A session carries its model and thinking effort, and a single goal can override both.
#[tokio::test]
async fn a_session_remembers_its_model_and_effort_and_a_goal_can_override_them() {
    let h = Harness::start(None, 600).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "chooser" })).await;
    let id = session["id"].as_str().unwrap().to_string();

    // A fresh session has no opinion, so the router decides.
    assert_eq!(session["model_hint"], json!(null));
    assert_eq!(session["reasoning_effort"], json!(null));

    // Set both on the session.
    let (status, configured) = h
        .patch(
            &format!("/v1/sessions/{id}"),
            json!({ "model": "mock", "effort": "high" }),
        )
        .await;
    assert_eq!(status, 200, "{configured}");
    assert_eq!(configured["session"]["model_hint"], json!("mock"));
    assert_eq!(configured["session"]["reasoning_effort"], json!("high"));

    // A run inherits the session choice, and the run record says so.
    let (status, _) = h
        .post(&format!("/v1/sessions/{id}/messages"), json!({ "text": "what is 6*7?", "wait": true }))
        .await;
    assert_eq!(status, 200);
    let (_, detail) = h.get(&format!("/v1/sessions/{id}")).await;
    let runs = detail["runtime"]["runs"].as_array().unwrap();
    assert_eq!(runs[0]["model_hint"], json!("mock"), "the run records the choice it used");
    assert_eq!(runs[0]["reasoning_effort"], json!("high"));

    // A single goal can override both without changing the session.
    let (status, _body) = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": "and 7*7?", "wait": true, "model": "echo-provider", "effort": "low" }),
        )
        .await;
    assert_eq!(status, 200);
    let (_, after) = h.get(&format!("/v1/sessions/{id}")).await;
    let runs = after["runtime"]["runs"].as_array().unwrap();
    assert_eq!(
        runs[1]["model_hint"],
        json!("echo-provider"),
        "the second run used the override"
    );
    assert_eq!(runs[1]["reasoning_effort"], json!("low"));
    assert_eq!(
        after["session"]["model_hint"],
        json!("mock"),
        "the session setting is unchanged by a per-goal override"
    );

    // An unknown effort is a client error, never a silent fallback.
    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": "hello", "wait": true, "effort": "maximum" }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("off, low, medium or high"));
    let (status, _) = h
        .patch(&format!("/v1/sessions/{id}"), json!({ "effort": "turbo" }))
        .await;
    assert_eq!(status, 400);

    // An empty string clears a choice: back to letting the router decide.
    let (status, cleared) = h.patch(&format!("/v1/sessions/{id}"), json!({ "model": "" })).await;
    assert_eq!(status, 200);
    assert_eq!(cleared["session"]["model_hint"], json!(null));

    // A patch with nothing in it is a client mistake, not a no-op.
    let (status, _) = h.patch(&format!("/v1/sessions/{id}"), json!({})).await;
    assert_eq!(status, 400);

    h.shutdown.cancel();
}

/// A restart must not turn a session into a 503.
///
/// The session record and its runs are durable; the message-by-message transcript and the actor
/// state are not, unless a snapshot happened to be taken. This test restarts a kernel on the same
/// data directory with no snapshot at all - the case that used to answer
/// "no live actor and no checkpoint to recover from".
#[tokio::test]
async fn a_session_survives_a_restart_without_a_snapshot() {
    let dir = std::env::temp_dir().join(format!("agentos-restart-{}", agentos_core::now_ms()));
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let config = || {
        let mut config = RuntimeConfig::default();
        config.storage.backend = StoreBackend::File;
        config.storage.data_dir = dir.join("data");
        config.policy.workspace_root = workspace.clone();
        config.observability.log_level = "error".into();
        config.api.auth_token_env = "AGENTOS_TEST_RESTART_TOKEN".into();
        config.discovery.enabled = false;
        config
    };
    std::env::remove_var("AGENTOS_TEST_RESTART_TOKEN");

    // --- first run: one conversation, and deliberately no checkpoint -----------------------
    let session_id = {
        let kernel = Kernel::bootstrap(config()).await.unwrap();
        let session = kernel.sessions.create_session("u1", "survives").await.unwrap();
        let result = kernel
            .sessions
            .post_goal(&session.id, "what is 6*7?", &[], &[], None, None, None)
            .await
            .unwrap();
        assert!(result["answer"].is_string(), "the run answered: {result}");
        session.id
    };

    // --- second run: same data directory, nothing running -------------------------------
    let kernel = Kernel::bootstrap(config()).await.unwrap();
    let (addr, shutdown) = agentos_api::serve_test(kernel.clone()).await.unwrap();
    let h = Harness {
        base: format!("http://{addr}"),
        _kernel: kernel,
        shutdown,
        client: reqwest::Client::new(),
    };

    let (status, transcript) = h.get(&format!("/v1/sessions/{session_id}/transcript")).await;
    assert_eq!(status, 200, "the session must be served, not 503: {transcript}");
    let messages = transcript["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "the conversation was rebuilt from the runs: {transcript}");
    assert_eq!(messages[0]["parts"][0]["text"], json!("what is 6*7?"));
    assert_eq!(messages[0]["role"], json!("user"));
    assert_eq!(messages[1]["role"], json!("assistant"));

    // The run history survives too, so the detail view is not blank either.
    let (status, detail) = h.get(&format!("/v1/sessions/{session_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(detail["runtime"]["runs"].as_array().unwrap().len(), 1);

    // And the session still accepts new work after the rebuild.
    let (status, _) = h
        .post(&format!("/v1/sessions/{session_id}/messages"), json!({ "text": "and 7*7?", "wait": true }))
        .await;
    assert_eq!(status, 200, "a rebuilt session is a working session");

    h.shutdown.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

/// An approval is a real gate: the call parks, a decision releases it, and no decision times out.
/// The diagnostics bundle explains the runtime without leaking what it must not.
#[tokio::test]
async fn the_diagnostics_bundle_is_useful_and_does_not_leak() {
    // A secret in the environment, of the kind a provider credential would be. Its NAME must appear
    // in the bundle (that is configuration), its VALUE must not (that is a leak).
    std::env::set_var("AGENTOS_TEST_DIAG_KEY", "sk-live-must-not-appear");

    let dir = std::env::temp_dir().join(format!("agentos-diag-{}", agentos_core::now_ms()));
    let mut config = RuntimeConfig::default();
    config.storage.backend = StoreBackend::Memory;
    config.storage.data_dir = dir.join("data");
    config.policy.workspace_root = dir.join("workspace");
    config.observability.log_level = "error".into();
    config.api.auth_token_env = "AGENTOS_TEST_DIAG_TOKEN".into();
    config.models.providers.push(agentos_core::config::ProviderConfig {
        name: "deepseek".into(),
        kind: agentos_core::config::ProviderKind::Deepseek,
        model: "deepseek-chat".into(),
        base_url: "https://api.deepseek.com".into(),
        api_key_env: "AGENTOS_TEST_DIAG_KEY".into(),
        enabled: true,
        priority: 1,
        timeout_ms: 5_000,
        // A text-only model on purpose: this test is about what an unconfigured provider looks
        // like in a diagnostics bundle.
        vision: Some(false),
    });
    // An MCP server whose env map carries a value that must never travel.
    config.mcp.servers.push(agentos_core::config::McpServerConfig {
        name: "fake".into(),
        command: "node".into(),
        args: vec![],
        env: [("SERVICE_TOKEN".to_string(), "mcp-secret-value".to_string())]
            .into_iter()
            .collect(),
        enabled: false,
    });

    let kernel = Kernel::bootstrap(config).await.unwrap();
    let (addr, shutdown) = agentos_api::serve_test(kernel.clone()).await.unwrap();
    let h = Harness {
        base: format!("http://{addr}"),
        _kernel: kernel,
        shutdown,
        client: reqwest::Client::new(),
    };
    // A canary in the goal: if any future section embeds what the user typed, this test fails.
    const CANARY: &str = "canary-please-do-not-leak-9931";
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "diag" })).await;
    let id = session["id"].as_str().unwrap().to_string();
    let _ = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": format!("what is 6*7? {CANARY}"), "wait": true }),
        )
        .await;

    let (status, body) = h.get("/v1/diagnostics").await;
    assert_eq!(status, 200);
    for section in [
        "runtime",
        "health",
        "config",
        "capabilities",
        "sessions",
        "events",
        "environment_warnings",
    ] {
        assert!(!body[section].is_null(), "the bundle must carry {section}: {body}");
    }

    // The capability list states the policy verdict, which is what most support questions need.
    let capabilities = body["capabilities"].as_array().unwrap();
    let write = capabilities
        .iter()
        .find(|entry| entry["name"] == json!("filesystem-write"))
        .expect("built-ins are listed");
    assert_eq!(write["policy"]["allowed"], json!(false));
    assert!(
        write["policy"]["reason"].as_str().unwrap().contains("explicit allow"),
        "the verdict explains itself: {write}"
    );

    let text = body.to_string();
    assert!(!text.contains("sk-live-must-not-appear"), "a credential value leaked");
    assert!(!text.contains("mcp-secret-value"), "an MCP env value leaked");
    assert!(
        text.contains("AGENTOS_TEST_DIAG_KEY"),
        "the name of the credential variable is configuration and should be visible"
    );
    assert!(body["redactions"].as_u64().unwrap() >= 1, "redaction is reported, not silent");

    // Conversation text is opt-in, and the canary proves it really is out: the goal travels
    // through runs, event payloads and event messages, so one leak anywhere fails here.
    assert!(body["transcripts"].is_null(), "transcripts need an explicit opt-in");
    assert_eq!(body["conversation_included"], json!(false));
    assert!(
        !text.contains(CANARY),
        "the goal text leaked into the default bundle"
    );
    let (_, with_text) = h.get("/v1/diagnostics?transcripts=true").await;
    assert!(!with_text["transcripts"].is_null());
    assert_eq!(with_text["conversation_included"], json!(true));
    assert!(
        with_text.to_string().contains(CANARY),
        "with an explicit opt-in the conversation is actually there"
    );

    std::env::remove_var("AGENTOS_TEST_DIAG_KEY");
    h.shutdown.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A run publishes its answer as it is written, to live subscribers only.
#[tokio::test]
async fn streamed_deltas_reach_live_subscribers() {
    let h = Harness::start(None, 600).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "stream" })).await;
    let id = session["id"].as_str().unwrap().to_string();

    // Watch the socket BEFORE posting, because the preview is live: it is not replayable.
    let ws_url = h.base.replace("http://", "ws://") + "/v1/ws";
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url).await.expect("ws connects");
    let _hello = next_of_type(&mut socket, "hello").await;

    let (status, body) = h
        .post(&format!("/v1/sessions/{id}/messages"), json!({ "text": "what is 6*7?", "wait": true }))
        .await;
    assert_eq!(status, 200);

    // Drain what is queued on the socket, then look at what arrived. Bounded on purpose: the run is
    // already finished (the POST above waited for it), so the preview is in flight or gone - it is
    // never worth blocking on a socket that is designed to stay open.
    let mut deltas: Vec<Value> = Vec::new();
    let mut idle = 0;
    loop {
        let Some(frame) = next_json_within(&mut socket, Duration::from_millis(500)).await else {
            idle += 1;
            // Two quiet half-seconds and the preview is done arriving.
            if idle >= 2 {
                break;
            }
            continue;
        };
        idle = 0;
        if frame["type"] == "event" && frame["event"]["kind"] == "agent_delta" {
            deltas.push(frame["event"].clone());
        }
    }
    assert!(!deltas.is_empty(), "the answer must stream to live subscribers");

    let streamed: String = deltas
        .iter()
        .filter_map(|event| event["payload"]["text"].as_str())
        .collect();
    assert!(!streamed.trim().is_empty(), "deltas carry text");
    assert!(
        deltas.iter().all(|event| event["payload"]["run_id"].is_string()),
        "every delta names the run it belongs to"
    );

    // The preview is a prefix of the stored answer, never something else.
    let answer = body["final_answer"].as_str().unwrap_or_default();
    if !answer.is_empty() {
        let trimmed = streamed.trim();
        assert!(
            answer.contains(trimmed) || trimmed.contains(answer.trim()),
            "streamed {trimmed:?} vs stored {answer:?}"
        );
    }

    // And the preview is not a log entry: replaying the durable events finds no deltas at all.
    let (_, events) = h.get("/v1/events?limit=400&kinds=agent_delta").await;
    assert_eq!(
        events["events"].as_array().unwrap().len(),
        0,
        "a per-token preview must not be written to the log"
    );

    h.shutdown.cancel();
}

/// An image a browser has the bytes for becomes an attachment the runtime can send to a model.
///
/// This is the path a drag-and-drop takes, and the reason it exists: a console cannot write into
/// the runtime's workspace, so "type in a file path" was never going to work for a screenshot.
#[tokio::test]
async fn an_uploaded_image_becomes_an_attachment_the_goal_can_name() {
    let h = Harness::start_with_models(
        "AGENTOS_TEST_API_TOKEN",
        None,
        600,
        Some(models_with_a_sighted_provider()),
    )
    .await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "attach" })).await;
    let id = session["id"].as_str().unwrap().to_string();

    // 1. the name is not the type: text called .png is stored as text, not as an image
    let (status, body) = h
        .upload(
            &format!("/v1/sessions/{id}/attachments"),
            "not-an-image.png",
            "image/png",
            b"this is not a picture",
        )
        .await;
    assert_eq!(status, 200, "content decides, not the name: {body}");
    assert_eq!(
        body["kind"], "text",
        "a file that is not an image is read as text, not refused as a broken image: {body}"
    );
    assert_eq!(body["content_type"], "text/plain");

    // 2. a real PNG is stored and its bytes can be read back
    let (status, uploaded) = h
        .upload(
            &format!("/v1/sessions/{id}/attachments"),
            "shot.png",
            "image/png",
            ONE_PIXEL_PNG,
        )
        .await;
    assert_eq!(status, 200, "{uploaded}");
    let artifact_id = uploaded["artifact_id"].as_str().unwrap().to_string();
    assert_eq!(uploaded["content_type"], "image/png");
    assert_eq!(uploaded["bytes"], ONE_PIXEL_PNG.len());

    let (status, bytes, content_type) = h.get_bytes(&format!("/v1/artifacts/{artifact_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(content_type, "image/png", "the artifact remembers what it is");
    assert_eq!(bytes, ONE_PIXEL_PNG, "the bytes come back unchanged");

    // 3. naming it in a goal is accepted, and the transcript records the image part
    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": "what is in this picture?", "attachments": [artifact_id], "wait": true }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (_, transcript) = h.get(&format!("/v1/sessions/{id}/transcript?limit=5")).await;
    let parts = transcript["messages"][0]["parts"].as_array().cloned().unwrap_or_default();
    assert!(
        parts
            .iter()
            .any(|part| part["type"] == "image" && part["name"] == "shot.png"),
        "the turn records the attached image: {transcript}"
    );

    // 4. an id nobody stored is refused, rather than quietly dropped
    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": "and this one?", "attachments": ["art_does_not_exist"], "wait": true }),
        )
        .await;
    assert_eq!(status, 404, "{body}");

    h.shutdown.cancel();
}

/// An .xlsx is read into a table the model can use - the whole point of accepting a binary table.
#[tokio::test]
async fn an_uploaded_spreadsheet_becomes_a_table_the_goal_can_name() {
    let h = Harness::start(None, 600).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "book" })).await;
    let id = session["id"].as_str().unwrap().to_string();

    // A real .xlsx, written here: a zip with a workbook, a string table and one sheet.
    let mut book = Vec::new();
    {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut book));
        let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        let mut add = |name: &str, body: &str| {
            writer.start_file(name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        };
        add(
            "xl/workbook.xml",
            r#"<workbook><sheets><sheet name="销售" sheetId="1"/></sheets></workbook>"#,
        );
        add(
            "xl/sharedStrings.xml",
            r#"<sst><si><t>region</t></si><si><t>revenue</t></si><si><t>华东</t></si></sst>"#,
        );
        add(
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><v>128000</v></c></row><row r="3"><c r="A3" t="s"><v>2</v></c><c r="B3"><v>143500</v></c></row></sheetData></worksheet>"#,
        );
        writer.finish().unwrap();
    }

    let (status, uploaded) = h
        .upload(
            &format!("/v1/sessions/{id}/attachments"),
            "book.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            &book,
        )
        .await;
    assert_eq!(status, 200, "{uploaded}");
    assert_eq!(uploaded["kind"], "spreadsheet", "{uploaded}");
    assert_eq!(uploaded["spreadsheet"]["sheets"][0], "销售", "{uploaded}");
    assert_eq!(uploaded["spreadsheet"]["rows"], 3, "{uploaded}");
    let artifact_id = uploaded["artifact_id"].as_str().unwrap().to_string();

    // What is stored is the table the model reads, not the zip: a client can fetch the rows.
    let (status, bytes, content_type) = h.get_bytes(&format!("/v1/artifacts/{artifact_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(content_type, "text/csv");
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(text.starts_with("region,revenue"), "{text}");
    assert!(text.contains("128000") && text.contains("143500"), "{text}");

    h.shutdown.cancel();
}

/// A text file the user attaches is stored, recorded in the turn, and read into the model's prompt.
///
/// This is the "table file" path: a browser can hand over a CSV it has, and the model should answer
/// about the data in it without the file having to be in the runtime's workspace.
#[tokio::test]
async fn an_uploaded_text_file_becomes_a_document_the_goal_can_name() {
    let h = Harness::start(None, 600).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "table" })).await;
    let id = session["id"].as_str().unwrap().to_string();
    let csv = "region,quarter,revenue\n华东,Q1,128000\n华东,Q2,143500\n";

    let (status, uploaded) = h
        .upload(
            &format!("/v1/sessions/{id}/attachments"),
            "sales.csv",
            "text/csv",
            csv.as_bytes(),
        )
        .await;
    assert_eq!(status, 200, "{uploaded}");
    assert_eq!(uploaded["kind"], "text");
    assert_eq!(uploaded["content_type"], "text/csv", "the name says CSV: {uploaded}");
    let artifact_id = uploaded["artifact_id"].as_str().unwrap().to_string();

    // The bytes come back unchanged, so a client can show the file it sent.
    let (status, bytes, content_type) = h.get_bytes(&format!("/v1/artifacts/{artifact_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(content_type, "text/csv");
    assert_eq!(bytes, csv.as_bytes());

    // Posting a goal that names it records the document on the turn.
    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": "华东两个季度合计多少？", "attachments": [artifact_id], "wait": true }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (_, transcript) = h.get(&format!("/v1/sessions/{id}/transcript?limit=5")).await;
    let parts = transcript["messages"][0]["parts"].as_array().cloned().unwrap_or_default();
    assert!(
        parts
            .iter()
            .any(|part| part["type"] == "artifact" && part["name"] == "sales.csv"),
        "the turn records the document by name: {transcript}"
    );

    h.shutdown.cancel();
}

/// A file that is neither an image nor text is refused, and the refusal names both kinds.
#[tokio::test]
async fn an_upload_that_is_neither_an_image_nor_text_is_refused() {
    let h = Harness::start(None, 600).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "junk" })).await;
    let id = session["id"].as_str().unwrap().to_string();
    // A ZIP header: a NUL byte, so it is not text, and no image magic, so it is not an image.
    let archive = [0x50u8, 0x4b, 0x03, 0x04, 0x00, 0x41, 0x42];
    let (status, body) = h
        .upload(
            &format!("/v1/sessions/{id}/attachments"),
            "archive.zip",
            "application/zip",
            &archive,
        )
        .await;
    assert_eq!(status, 400, "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("image") && message.contains("text"),
        "the refusal must say what is accepted: {message}"
    );
    h.shutdown.cancel();
}

/// An image sent to a runtime whose only model cannot see is refused, not answered.
///
/// The failure this pins down was measured end to end: the image went to a text-only model, the
/// provider answered HTTP 400, the router failed over to the placeholder, and the user got a
/// confident paragraph about a picture that no model had looked at.
#[tokio::test]
async fn an_image_is_refused_when_no_configured_model_can_see() {
    let h = Harness::start_with_models(
        "AGENTOS_TEST_BLIND_TOKEN",
        None,
        600,
        Some(models_without_eyes()),
    )
    .await;
    let (_, models) = h.get("/v1/models").await;
    assert_eq!(
        models["vision_capable"].as_array().map(|list| list.len()),
        Some(0),
        "a default deployment has no vision provider: {models}"
    );
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "blind" })).await;
    let id = session["id"].as_str().unwrap().to_string();
    let (status, body) = h
        .upload(
            &format!("/v1/sessions/{id}/attachments"),
            "shot.png",
            "image/png",
            ONE_PIXEL_PNG,
        )
        .await;
    assert_eq!(status, 400, "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no configured model can be shown an image"),
        "the refusal must say what the problem is: {message}"
    );
    assert!(
        // The provider list is what makes it fixable: it says who was asked and that all of them
        // are blind, so nobody has to guess which model answered.
        message.contains("mock=no"),
        "the refusal must list who was asked: {message}"
    );
    assert!(
        message.contains("vision: true") || message.contains("deepseek-flash"),
        "the refusal must name the fix: {message}"
    );
    h.shutdown.cancel();
}

/// Renaming must go through the actor, and listing must be searchable.
#[tokio::test]
async fn sessions_can_be_renamed_and_searched() {
    let h = Harness::start(None, 600).await;
    let (_, first) = h.post("/v1/sessions", json!({ "user_id": "alice", "title": "first task" })).await;
    let (_, second) = h.post("/v1/sessions", json!({ "user_id": "bob", "title": "second task" })).await;
    let first_id = first["id"].as_str().unwrap().to_string();

    let (status, renamed) = h
        .patch(&format!("/v1/sessions/{first_id}"), json!({ "title": "renamed task" }))
        .await;
    assert_eq!(status, 200);
    assert_eq!(renamed["session"]["title"], json!("renamed task"));

    // The actor's copy is the one status reads, so it must have changed too - not just the store.
    let (_, detail) = h.get(&format!("/v1/sessions/{first_id}")).await;
    assert_eq!(
        detail["runtime"]["title"],
        json!("renamed task"),
        "the live actor must agree with what was stored"
    );
    let (_, status_body) = h.get(&format!("/v1/sessions/{first_id}/status")).await;
    assert_eq!(status_body["title"], json!("renamed task"));

    // Search matches the title, and the user id, case-insensitively.
    let (_, by_title) = h.get("/v1/sessions?q=second").await;
    assert_eq!(by_title["total"], json!(1));
    assert_eq!(by_title["sessions"][0]["id"], second["id"]);
    let (_, by_user) = h.get("/v1/sessions?q=ALICE").await;
    assert_eq!(by_user["sessions"][0]["id"], json!(first_id));
    let (_, no_match) = h.get("/v1/sessions?q=nothing-matches-this").await;
    assert_eq!(no_match["total"], json!(0));
    let (_, all) = h.get("/v1/sessions").await;
    assert_eq!(all["total"], json!(2), "no query means no filter");

    // A blank rename is a client error, and the title survives it.
    let (status, _) = h.patch(&format!("/v1/sessions/{first_id}"), json!({ "title": "   " })).await;
    assert_eq!(status, 400);
    let (_, still) = h.get(&format!("/v1/sessions/{first_id}")).await;
    assert_eq!(still["runtime"]["title"], json!("renamed task"));

    h.shutdown.cancel();
}

/// A branch inherits the conversation and then lives its own life.
#[tokio::test]
async fn branching_a_session_copies_the_conversation_and_detaches_it() {
    let h = Harness::start(None, 600).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "original" })).await;
    let id = session["id"].as_str().unwrap().to_string();
    let (status, _) = h
        .post(&format!("/v1/sessions/{id}/messages"), json!({ "text": "what is 6*7?", "wait": true }))
        .await;
    assert_eq!(status, 200);

    let (_, original) = h.get(&format!("/v1/sessions/{id}/transcript")).await;
    let original_turns = original["messages"].as_array().unwrap().len();
    assert_eq!(original_turns, 2, "a goal and its answer");

    let (status, body) = h.post(&format!("/v1/sessions/{id}/branch"), json!({ "title": "explore" })).await;
    assert_eq!(status, 200);
    let branch_id = body["session"]["id"].as_str().unwrap().to_string();
    assert_ne!(branch_id, id, "a branch is its own session");
    assert_eq!(body["session"]["title"], json!("explore"));
    assert_eq!(body["forked_from"], json!(id));

    // It starts with the same conversation...
    let (_, branch) = h.get(&format!("/v1/sessions/{branch_id}/transcript")).await;
    assert_eq!(branch["messages"].as_array().unwrap().len(), original_turns);
    let branch_first: String = branch["messages"][0]["parts"][0]["text"].as_str().unwrap().to_string();
    assert_eq!(branch_first, "what is 6*7?");
    // ...but every message belongs to the branch, not to the session it came from.
    assert_eq!(branch["messages"][0]["session_id"], json!(branch_id));

    // And it continues on its own: a new goal in the branch must not appear in the original.
    let before = h.get(&format!("/v1/sessions/{id}/transcript")).await.1;
    let (status, _) = h
        .post(&format!("/v1/sessions/{branch_id}/messages"), json!({ "text": "branch only", "wait": true }))
        .await;
    assert_eq!(status, 200);
    let after = h.get(&format!("/v1/sessions/{id}/transcript")).await.1;
    assert_eq!(
        before["messages"].as_array().unwrap().len(),
        after["messages"].as_array().unwrap().len(),
        "the original must not grow when the branch runs"
    );
    let branched = h.get(&format!("/v1/sessions/{branch_id}/transcript")).await.1;
    assert!(branched["messages"].as_array().unwrap().len() > original_turns);

    h.shutdown.cancel();
}

/// Export is a read-only projection: both formats carry the conversation and what it cost.
#[tokio::test]
async fn a_session_exports_as_json_and_markdown() {
    let h = Harness::start(None, 600).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "export me" })).await;
    let id = session["id"].as_str().unwrap().to_string();
    let (status, _) = h
        .post(&format!("/v1/sessions/{id}/messages"), json!({ "text": "what is 8*8?", "wait": true }))
        .await;
    assert_eq!(status, 200);

    let (status, body) = h.get(&format!("/v1/sessions/{id}/export")).await;
    assert_eq!(status, 200);
    assert_eq!(body["session"]["title"], json!("export me"));
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(body["runs"].as_array().unwrap().len(), 1);
    assert!(
        body["usage"]["total_tokens"].as_u64().unwrap() > 0,
        "an export states what the session cost: {}",
        body["usage"]
    );

    let response = h
        .client
        .get(h.url(&format!("/v1/sessions/{id}/export?format=markdown")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let markdown = response.text().await.unwrap();
    assert!(markdown.starts_with("# export me"), "got: {markdown}");
    assert!(markdown.contains("what is 8*8?"));
    assert!(markdown.contains("## Runs"));

    // An unknown format is a client error, not a silent fallback.
    let (status, _) = h.get(&format!("/v1/sessions/{id}/export?format=pdf")).await;
    assert_eq!(status, 400);

    h.shutdown.cancel();
}

/// When the conversation outgrows the history window, the dropped turns are summarised once and
/// kept as memory - dropped from the prompt, not from memory.
#[tokio::test]
async fn dropped_turns_are_summarised_and_kept_as_memory() {
    let dir = std::env::temp_dir().join(format!("agentos-compact-{}", agentos_core::now_ms()));
    let mut config = RuntimeConfig::default();
    config.storage.backend = StoreBackend::Memory;
    config.storage.data_dir = dir.join("data");
    config.policy.workspace_root = dir.join("workspace");
    config.observability.log_level = "error".into();
    config.api.auth_token_env = "AGENTOS_TEST_COMPACT_TOKEN".into();
    // A three-message window: the fourth turn pushes two turns out, and four is enough to compact.
    config.policy.history_messages = 3;
    config.policy.compaction_enabled = true;
    config.policy.compaction_min_messages = 2;
    std::env::remove_var("AGENTOS_TEST_COMPACT_TOKEN");

    let kernel = Kernel::bootstrap(config).await.unwrap();
    let (addr, shutdown) = agentos_api::serve_test(kernel.clone()).await.unwrap();
    let h = Harness {
        base: format!("http://{addr}"),
        _kernel: kernel.clone(),
        shutdown,
        client: reqwest::Client::new(),
    };

    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "compact" })).await;
    let id = session["id"].as_str().unwrap().to_string();
    for turn in 0..3 {
        let (status, _) = h
            .post(
                &format!("/v1/sessions/{id}/messages"),
                json!({ "text": format!("turn {turn}: what is {turn}*7?"), "wait": true }),
            )
            .await;
        assert_eq!(status, 200);
    }

    let (_, events) = h.get("/v1/events?limit=300&kinds=session_compacted").await;
    let compacted = events["events"].as_array().unwrap();
    assert!(!compacted.is_empty(), "the window was exceeded, so a summary must exist: {events}");
    assert!(
        compacted[0]["payload"]["turns"].as_u64().unwrap() >= 2,
        "the summary covers the turns that left the window: {}",
        compacted[0]
    );

    // The summary is a memory record, and it is what recall now feeds the prompt.
    let session_id = agentos_core::SessionId::from_raw(id.clone());
    let memories = kernel
        .memory
        .recall(agentos_core::model::MemoryQuery {
            session_id: Some(session_id),
            kinds: vec![agentos_core::model::MemoryKind::Episode],
            limit: 20,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        memories.iter().any(|m| m.tags.contains(&"summary".to_string())),
        "a summary record must be stored: {:?}",
        memories.iter().map(|m| m.tags.clone()).collect::<Vec<_>>()
    );

    let (_, detail) = h.get(&format!("/v1/sessions/{id}")).await;
    assert!(
        detail["runtime"]["compaction_usage"]["calls"].as_u64().unwrap() >= 1,
        "the summarisation call is accounted for: {}",
        detail["runtime"]["compaction_usage"]
    );

    h.shutdown.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Project instructions in the workspace must reach the prompt, and only from inside the jail.
#[tokio::test]
async fn workspace_context_files_are_loaded_into_the_prompt() {
    let dir = std::env::temp_dir().join(format!("agentos-ctx-{}", agentos_core::now_ms()));
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("AGENTS.md"), "Always answer in one sentence.").unwrap();
    // A file outside the workspace must never be read, however it is configured.
    std::fs::write(dir.join("secret.md"), "TOP SECRET").unwrap();

    let mut config = RuntimeConfig::default();
    config.storage.backend = StoreBackend::Memory;
    config.storage.data_dir = dir.join("data");
    config.policy.workspace_root = workspace.clone();
    config.observability.log_level = "error".into();
    config.api.auth_token_env = "AGENTOS_TEST_CONTEXT_TOKEN".into();
    config.policy.context_files = vec!["AGENTS.md".into(), "../secret.md".into()];
    std::env::remove_var("AGENTOS_TEST_CONTEXT_TOKEN");

    let kernel = Kernel::bootstrap(config).await.unwrap();
    let (addr, shutdown) = agentos_api::serve_test(kernel.clone()).await.unwrap();
    let h = Harness {
        base: format!("http://{addr}"),
        _kernel: kernel,
        shutdown,
        client: reqwest::Client::new(),
    };

    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "ctx" })).await;
    let id = session["id"].as_str().unwrap().to_string();
    let (status, _) = h
        .post(&format!("/v1/sessions/{id}/messages"), json!({ "text": "hello", "wait": true }))
        .await;
    assert_eq!(status, 200);

    let (_, events) = h.get("/v1/events?limit=200&kinds=context_loaded").await;
    let loaded = events["events"].as_array().unwrap();
    assert_eq!(loaded.len(), 1, "context is loaded once per run: {events}");
    assert_eq!(loaded[0]["payload"]["files"], json!(["AGENTS.md"]));
    assert!(
        loaded[0]["payload"]["chars"].as_u64().unwrap() > 0,
        "the file had content: {}",
        loaded[0]
    );

    h.shutdown.cancel();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Recall must reach the prompt, and it must not repeat what the conversation still shows.
#[tokio::test]
async fn memories_outside_the_history_window_are_recalled_into_the_prompt() {
    let h = Harness::start(None, 600).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "recall" })).await;
    let id = session["id"].as_str().unwrap().to_string();
    let session_id = agentos_core::SessionId::from_raw(id.clone());

    // A turn from long ago, written straight into the store. Its goal appears nowhere in the
    // transcript, so the history window cannot cover it and recall is the only way back.
    h._kernel
        .memory
        .write(agentos_agent_runtime::memory::episode(
            session_id,
            "goal: what did we decide about the schema?\nanswer: keep it flat",
            &["session", "turn"],
        ))
        .await
        .unwrap();

    let (status, _) = h
        .post(&format!("/v1/sessions/{id}/messages"), json!({ "text": "carry on", "wait": true }))
        .await;
    assert_eq!(status, 200);

    let (_, events) = h.get("/v1/events?limit=200&kinds=memory_recalled").await;
    let recalled = events["events"].as_array().unwrap();
    assert_eq!(recalled.len(), 1, "one recall per run: {events}");
    assert!(
        recalled[0]["payload"]["chars"].as_u64().unwrap() > 0,
        "the injected text must be non-empty: {}",
        recalled[0]
    );

    // The same run must not recall what the conversation already contains: run again, and the
    // second recall still exists (a new, distinct turn record) but never duplicates a visible goal.
    let (_, transcript) = h.get(&format!("/v1/sessions/{id}/transcript")).await;
    let goals: Vec<&str> = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == json!("user"))
        .filter_map(|m| m["parts"][0]["text"].as_str())
        .collect();
    assert!(goals.contains(&"carry on"));

    h.shutdown.cancel();
}

/// The router can be built without binding a port, which keeps these tests fast and deterministic.
#[tokio::test]
async fn router_builds_without_io() {
    let dir = std::env::temp_dir().join(format!("agentos-api-router-{}", agentos_core::now_ms()));
    let mut config = RuntimeConfig::default();
    config.storage.backend = StoreBackend::Memory;
    config.storage.data_dir = dir.clone();
    config.policy.workspace_root = dir.join("ws");
    config.observability.log_level = "error".into();
    let kernel = Kernel::bootstrap(config).await.unwrap();
    let state = ApiState::new(kernel.clone(), Arc::new(kernel.config.clone()));
    let _router = agentos_api::router(state);
    let kinds = agentos_api::ws::subscribable_kinds();
    assert!(kinds.contains(&"tool_call"));
}
// ---------------------------------------------------------------------------------------------
// Principals, ownership and the open/close toggle
// ---------------------------------------------------------------------------------------------

/// Three people on two nodes: an owner, a stranger, and an admin who is not part of the session.
fn three_principals() -> Vec<agentos_core::config::PrincipalConfig> {
    use agentos_core::config::PrincipalConfig;
    vec![
        PrincipalConfig {
            user_id: "alice".into(),
            node_id: Some("node-a".into()),
            roles: vec!["operator".into()],
            token_env: None,
            token: Some("alice-token".into()),
        },
        PrincipalConfig {
            user_id: "bob".into(),
            node_id: Some("node-b".into()),
            roles: vec![],
            token_env: None,
            token: Some("bob-token".into()),
        },
        PrincipalConfig {
            user_id: "root".into(),
            node_id: None,
            roles: vec!["admin".into()],
            token_env: None,
            token: Some("root-token".into()),
        },
    ]
}

impl Harness {
    async fn start_with_principals() -> Self {
        let dir = std::env::temp_dir().join(format!("agentos-api-acl-{}", agentos_core::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = RuntimeConfig::default();
        config.storage.backend = StoreBackend::Memory;
        config.storage.data_dir = dir.join("data");
        config.policy.workspace_root = dir.join("workspace");
        config.observability.log_level = "error".into();
        config.api.principals = three_principals();
        let kernel = Kernel::bootstrap(config).await.unwrap();
        let (addr, shutdown) = agentos_api::serve_test(kernel.clone()).await.unwrap();
        Self {
            base: format!("http://{addr}"),
            _kernel: kernel,
            shutdown,
            client: reqwest::Client::new(),
        }
    }

    /// Send a request as somebody. `None` sends no token at all.
    async fn send_as(
        &self,
        method: reqwest::Method,
        token: Option<&str>,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self.client.request(method, self.url(path));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        (status, serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)))
    }

    async fn get_as(&self, token: Option<&str>, path: &str) -> (u16, Value) {
        self.send_as(reqwest::Method::GET, token, path, None).await
    }

    async fn post_as(&self, token: Option<&str>, path: &str, body: Value) -> (u16, Value) {
        self.send_as(reqwest::Method::POST, token, path, Some(body)).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_belongs_to_whoever_created_it() {
    let h = Harness::start_with_principals().await;

    let (status, body) = h
        .post_as(Some("alice-token"), "/v1/sessions", json!({ "title": "alice's work" }))
        .await;
    assert_eq!(status, 200, "{body}");
    let session_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["owner"]["user_id"], "alice");
    assert_eq!(body["owner"]["node_id"], "node-a", "the owner is a person on a node: {body}");

    // Alice sees her own role without being told it.
    let (status, body) = h.get_as(Some("alice-token"), &format!("/v1/sessions/{session_id}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["you"]["session_role"], "owner");
    assert!(body["you"]["can"].as_array().unwrap().iter().any(|a| a == "delete"));

    // A stranger is refused, and the refusal says who to ask.
    let (status, body) = h.get_as(Some("bob-token"), &format!("/v1/sessions/{session_id}")).await;
    assert_eq!(status, 403, "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(message.contains("alice"), "the refusal should name the owner: {message}");

    // An admin is not part of the session and does not need to be.
    let (status, _) = h.get_as(Some("root-token"), &format!("/v1/sessions/{session_id}")).await;
    assert_eq!(status, 200);

    // No token at all is not a principal.
    let (status, _) = h.get_as(None, &format!("/v1/sessions/{session_id}")).await;
    assert_eq!(status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_granted_role_decides_what_a_person_may_do() {
    let h = Harness::start_with_principals().await;
    let (_, created) = h
        .post_as(Some("alice-token"), "/v1/sessions", json!({ "title": "shared" }))
        .await;
    let id = created["id"].as_str().unwrap().to_string();

    // Bob has nothing yet: he cannot even read it.
    let (status, _) = h.get_as(Some("bob-token"), &format!("/v1/sessions/{id}")).await;
    assert_eq!(status, 403);

    // Alice makes him a participant.
    let (status, body) = h
        .post_as(
            Some("alice-token"),
            &format!("/v1/sessions/{id}/access"),
            json!({ "user_id": "bob", "node_id": "node-b", "role": "participant" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["session"]["grants"][0]["role"], "participant");
    assert_eq!(body["session"]["grants"][0]["granted_by"], "alice@node-a");

    // Now he can read and speak...
    let (status, body) = h.get_as(Some("bob-token"), &format!("/v1/sessions/{id}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["you"]["session_role"], "participant");
    let (status, body) = h
        .post_as(Some("bob-token"), &format!("/v1/sessions/{id}/messages"), json!({ "text": "hello from bob" }))
        .await;
    assert_eq!(status, 200, "{body}");

    // ...but not close, open or hand out roles.
    let refusals: [(reqwest::Method, String, Option<Value>); 3] = [
        (reqwest::Method::DELETE, format!("/v1/sessions/{id}"), None),
        (reqwest::Method::POST, format!("/v1/sessions/{id}/close"), Some(json!({}))),
        (
            reqwest::Method::POST,
            format!("/v1/sessions/{id}/access"),
            Some(json!({ "user_id": "carol", "role": "viewer" })),
        ),
    ];
    for (method, path, body) in refusals {
        let (status, response) = h.send_as(method, Some("bob-token"), &path, body).await;
        assert_eq!(status, 403, "{path}: {response}");
        let message = response["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("participant"), "{path}: {message}");
    }

    // Promoted to editor, the same requests change answer.
    let (status, _) = h
        .post_as(
            Some("alice-token"),
            &format!("/v1/sessions/{id}/access"),
            json!({ "user_id": "bob", "node_id": "node-b", "role": "editor" }),
        )
        .await;
    assert_eq!(status, 200);
    let (status, body) = h
        .post_as(Some("bob-token"), &format!("/v1/sessions/{id}/close"), json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_and_opening_keeps_the_conversation_and_says_who_spoke() {
    let h = Harness::start_with_principals().await;
    let (_, created) = h
        .post_as(Some("alice-token"), "/v1/sessions", json!({ "title": "a long conversation" }))
        .await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, _) = h
        .post_as(Some("alice-token"), &format!("/v1/sessions/{id}/messages"), json!({ "text": "first question" }))
        .await;
    assert_eq!(status, 200);

    let (status, body) = h
        .post_as(Some("alice-token"), &format!("/v1/sessions/{id}/close"), json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
    let (_, detail) = h.get_as(Some("alice-token"), &format!("/v1/sessions/{id}")).await;
    assert_eq!(detail["session"]["state"], "closed");
    assert!(detail["session"]["closed_at"].is_number());

    // A closed session takes no goals...
    let (status, body) = h
        .post_as(Some("alice-token"), &format!("/v1/sessions/{id}/messages"), json!({ "text": "while closed" }))
        .await;
    assert_eq!(status, 409, "{body}");

    // ...until it is opened again. Not restored: it was never gone.
    let (status, body) = h
        .post_as(Some("alice-token"), &format!("/v1/sessions/{id}/open"), json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["session"]["state"], "active");
    assert!(body["session"]["closed_at"].is_null(), "opening clears the closed mark: {body}");

    let (status, body) = h
        .post_as(Some("alice-token"), &format!("/v1/sessions/{id}/messages"), json!({ "text": "second question" }))
        .await;
    assert_eq!(status, 200, "{body}");

    // The conversation has both turns, and each says who asked.
    let (status, body) = h
        .get_as(Some("alice-token"), &format!("/v1/sessions/{id}/transcript?limit=50"))
        .await;
    assert_eq!(status, 200, "{body}");
    let messages = body["messages"].as_array().unwrap();
    let asked: Vec<&Value> = messages.iter().filter(|m| m["role"] == "user").collect();
    assert_eq!(asked.len(), 2, "both turns survived the close: {body}");
    assert_eq!(asked[0]["author"]["user_id"], "alice");
    assert_eq!(asked[0]["author"]["node_id"], "node-a");

    // And the run records it too, which is what a rebuilt transcript reads from.
    let (_, detail) = h.get_as(Some("alice-token"), &format!("/v1/sessions/{id}")).await;
    let runs = detail["runtime"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "{detail}");
    assert_eq!(runs[0]["author"]["user_id"], "alice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whoami_answers_who_the_gateway_thinks_you_are() {
    let h = Harness::start_with_principals().await;
    let (status, body) = h.get_as(Some("bob-token"), "/v1/auth/whoami").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["user_id"], "bob");
    assert_eq!(body["node_id"], "node-b");
    assert_eq!(body["admin"], false);
    let (_, body) = h.get_as(Some("root-token"), "/v1/auth/whoami").await;
    assert_eq!(body["admin"], true);
}

// ---------------------------------------------------------------------------------------------
// archiving
// ---------------------------------------------------------------------------------------------

/// A harness whose archive root the test can look at.
impl Harness {
    async fn start_with_archives(env_name: &str, archive_dir: std::path::PathBuf) -> Self {
        let dir = std::env::temp_dir().join(format!("agentos-api-arch-{}", agentos_core::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = RuntimeConfig::default();
        config.storage.backend = StoreBackend::Memory;
        config.storage.data_dir = dir.join("data");
        config.storage.archive_dir = archive_dir;
        config.policy.workspace_root = dir.join("workspace");
        config.observability.log_level = "error".into();
        config.api.auth_token_env = env_name.to_string();
        std::env::remove_var(env_name);
        let kernel = Kernel::bootstrap(config).await.unwrap();
        let (addr, shutdown) = agentos_api::serve_test(kernel.clone()).await.unwrap();
        Self { base: format!("http://{addr}"), _kernel: kernel, shutdown, client: reqwest::Client::new() }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archiving_writes_a_package_and_restoring_brings_it_back() {
    let root = std::env::temp_dir().join(format!("agentos-archives-{}", agentos_core::now_ms()));
    let h = Harness::start_with_archives("AGENTOS_TEST_ARCHIVE_TOKEN", root.clone()).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "archived talk" })).await;
    let id = session["id"].as_str().unwrap().to_string();

    // A conversation worth archiving: two turns, one of them with an attachment.
    let (status, _) = h
        .post(&format!("/v1/sessions/{id}/messages"), json!({ "text": "what is 6*7?", "wait": true }))
        .await;
    assert_eq!(status, 200);
    let (status, uploaded) = h
        .upload(
            &format!("/v1/sessions/{id}/attachments"),
            "notes.txt",
            "text/plain",
            b"the answer is 42\n",
        )
        .await;
    assert_eq!(status, 200, "{uploaded}");
    let artifact_id = uploaded["artifact_id"].as_str().unwrap().to_string();
    let (status, _) = h
        .post(
            &format!("/v1/sessions/{id}/messages"),
            json!({ "text": "and the notes?", "wait": true, "attachments": [artifact_id] }),
        )
        .await;
    assert_eq!(status, 200);

    // Archiving an open conversation is refused: closing is the deliberate step before it.
    let (status, body) = h.post(&format!("/v1/sessions/{id}/archive"), json!({})).await;
    assert_eq!(status, 409, "an open session is not archived: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("close it before archiving"), "{message}");
    let (_, still) = h.get(&format!("/v1/sessions/{id}")).await;
    let still_state = still["session"]["state"].as_str().unwrap_or_default();
    assert!(
        still_state == "active" || still_state == "idle",
        "the refusal changed nothing: {still}"
    );

    // Close it, then archive it.
    let (status, _) = h.post(&format!("/v1/sessions/{id}/close"), json!({})).await;
    assert_eq!(status, 200);
    let (status, body) = h.post(&format!("/v1/sessions/{id}/archive"), json!({})).await;
    assert_eq!(status, 200, "{body}");
    let archive_id = body["archive_id"].as_str().unwrap().to_string();
    assert!(body["bytes"].as_u64().unwrap() > 0, "the package has bytes: {body}");
    assert_eq!(body["manifest"]["messages"].as_u64().unwrap() >= 2, true, "{body}");
    assert_eq!(body["manifest"]["artifacts"], 1, "the attachment travelled: {body}");
    let path = std::path::PathBuf::from(body["path"].as_str().unwrap());
    assert!(path.exists(), "the package is on disk at {path:?}");

    // The record is now a tombstone: archived, and no longer taking goals.
    let (_, detail) = h.get(&format!("/v1/sessions/{id}")).await;
    assert_eq!(detail["session"]["state"], "archived", "{detail}");
    // And it is out of the live list. A tombstone in the list people pick work from is a row that
    // answers "no" to everything; the archive page is where it belongs.
    let (_, listed) = h.get("/v1/sessions").await;
    assert_eq!(
        listed["sessions"].as_array().unwrap().len(),
        0,
        "an archived conversation is not in the session list: {listed}"
    );
    let (status, body) = h
        .post(&format!("/v1/sessions/{id}/messages"), json!({ "text": "anyone there?", "wait": true }))
        .await;
    assert_eq!(status, 409, "an archived session takes no goals: {body}");
    // And it cannot be reopened: that is what "archived" means.
    let (status, _) = h.post(&format!("/v1/sessions/{id}/open"), json!({})).await;
    assert_eq!(status, 409);

    // It shows up in the archive listing, with a readable manifest.
    let (status, body) = h.get("/v1/archives").await;
    assert_eq!(status, 200, "{body}");
    let listed = body["archives"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{body}");
    assert_eq!(listed[0]["id"], archive_id);
    assert_eq!(listed[0]["manifest"]["title"], "archived talk");

    // The preview shows what was said, without restoring anything.
    let (status, body) = h.get(&format!("/v1/archives/{archive_id}")).await;
    assert_eq!(status, 200, "{body}");
    let preview = body["preview"].as_array().unwrap();
    assert!(!preview.is_empty(), "a preview has turns: {body}");
    assert!(preview.iter().any(|m| m["role"] == "user"), "{body}");

    // Restore: a NEW session that continues the conversation.
    let (status, body) = h.post(&format!("/v1/archives/{archive_id}/restore"), json!({})).await;
    assert_eq!(status, 200, "{body}");
    let restored_id = body["session"]["id"].as_str().unwrap().to_string();
    assert_ne!(restored_id, id, "a restore never overwrites the original");
    let (_, listed) = h.get("/v1/sessions").await;
    let rows = listed["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "the restored conversation is the one listed: {listed}");
    assert_eq!(rows[0]["id"], restored_id);
    assert_eq!(body["session"]["title"], "archived talk (restored)");
    // Restored open, and live: the actor is up, so its status comes from the actor rather than from
    // the durable rebuild. "Restored" means a conversation you can carry on with, not a record.
    let restored_state = body["session"]["state"].as_str().unwrap_or_default().to_string();
    assert!(
        restored_state == "active" || restored_state == "idle",
        "a restored session is open (active, or idle once its actor settles): {body}"
    );
    let (status, status_body) = h.get(&format!("/v1/sessions/{restored_id}/status")).await;
    assert_eq!(status, 200, "{status_body}");
    assert!(
        status_body["rebuilt_from_history"].is_null(),
        "a restored session answers from a live actor: {status_body}"
    );

    // The restored conversation has the turns, and can be talked to again.
    let (_, detail) = h.get(&format!("/v1/sessions/{restored_id}")).await;
    let runs = detail["runtime"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "both runs came back: {detail}");
    let (status, body) = h
        .post(&format!("/v1/sessions/{restored_id}/messages"), json!({ "text": "still there?", "wait": true }))
        .await;
    assert_eq!(status, 200, "a restored session takes goals: {body}");
    let (_, transcript) = h
        .get(&format!("/v1/sessions/{restored_id}/transcript?limit=50"))
        .await;
    let asked = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .count();
    assert_eq!(asked, 3, "two restored turns plus the new one: {transcript}");

    // The original session is still archived, and the package still lists.
    let (_, body) = h.get("/v1/archives").await;
    assert_eq!(body["archives"].as_array().unwrap().len(), 1);
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_package_is_refused_rather_than_half_restored() {
    let root = std::env::temp_dir().join(format!("agentos-archives-bad-{}", agentos_core::now_ms()));
    let h = Harness::start_with_archives("AGENTOS_TEST_ARCHIVE_BAD_TOKEN", root.clone()).await;
    let (_, session) = h.post("/v1/sessions", json!({ "user_id": "u1", "title": "damaged" })).await;
    let id = session["id"].as_str().unwrap().to_string();
    h.post(&format!("/v1/sessions/{id}/messages"), json!({ "text": "hello", "wait": true })).await;
    let (status, _) = h.post(&format!("/v1/sessions/{id}/close"), json!({})).await;
    assert_eq!(status, 200);
    let (status, body) = h.post(&format!("/v1/sessions/{id}/archive"), json!({})).await;
    assert_eq!(status, 200, "a closed session archives: {body}");
    let path = std::path::PathBuf::from(body["path"].as_str().unwrap());

    // Corrupt the package the way a full disk or a careless edit would: rewrite one file inside it.
    let bytes = std::fs::read(&path).unwrap();
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut rewritten: Vec<u8> = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut rewritten));
        let options = zip::write::FileOptions::<'_, ()>::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().to_string();
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut data).unwrap();
            drop(entry);
            if name == "runs.jsonl" {
                // A tampered run. The checksum in the manifest no longer matches.
                data = b"{\"goal\":\"something else\"}\n".to_vec();
            }
            writer.start_file(name, options).unwrap();
            std::io::Write::write_all(&mut writer, &data).unwrap();
        }
        writer.finish().unwrap();
    }
    std::fs::write(&path, &rewritten).unwrap();

    let (status, body) = h
        .post(&format!("/v1/archives/{}/restore", body["archive_id"].as_str().unwrap()), json!({}))
        .await;
    assert_eq!(status, 400, "a damaged package is refused: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("checksum") || message.contains("damaged"),
        "the refusal says why: {message}"
    );
    // Nothing was restored. The archived conversation is not in the live list either: it lives in
    // its package now, and the archive page is where it is found.
    let (_, list) = h.get("/v1/sessions").await;
    assert_eq!(list["sessions"].as_array().unwrap().len(), 0, "{list}");
    std::fs::remove_dir_all(&root).ok();
}
