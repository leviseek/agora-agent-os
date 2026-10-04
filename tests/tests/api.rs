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

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let response = self.client.post(self.url(path)).json(&body).send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let parsed = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
        (status, parsed)
    }
}

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
    let hello = next_json(&mut socket).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["domain_version"], agentos_core::DOMAIN_VERSION);

    // 2. command round trip
    socket.send(Message::Text(json!({"type":"ping"}).to_string().into())).await.unwrap();
    let pong = next_json(&mut socket).await;
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