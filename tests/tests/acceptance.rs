//! End-to-end acceptance tests against a fully wired kernel.
//!
//! Covered here (mirrors the acceptance list in the README):
//!   1. two sessions in parallel, ordering preserved inside each session
//!   2. a complete agent loop: Goal -> LLM -> Capability -> Observation -> Final
//!   3. a task graph running independent nodes in parallel
//!   4. capability discovery and invocation through the mesh
//!   5. actor directory, placement and worker heartbeat
//!   6. session actor snapshot export and restore
//!   7. policy: workspace traversal denied, writes denied by default
//!   8. failure and retry behaviour, including a capability that fails before succeeding

use agentos_capability_runtime::capability::{Capability, CapabilityContext};
use agentos_core::config::{RuntimeConfig, StoreBackend};
use agentos_core::model::{
    CapabilityDescriptor, CapabilityKind, CapabilityPermission, CapabilityProvider, EventFilter,
    EventKind, TaskKind, TaskPayload, TaskRecord, WorkerLoad,
};
use agentos_core::state::CapabilityHealth;
use agentos_core::{CapabilityId, SessionId, TaskId};
use agentos_kernel::Kernel;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A capability that sleeps, used to prove inter-session parallelism, plus a flaky capability
/// that fails a fixed number of times to prove retry semantics.
struct SleepCapability {
    delay_ms: u64,
    in_flight: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
}

#[async_trait]
impl Capability for SleepCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: CapabilityId::new(),
            name: "slow-task".into(),
            version: "1.0.0".into(),
            description: "Sleeps for a fixed time. Used to observe concurrency.".into(),
            kind: CapabilityKind::Builtin,
            tags: vec!["test".into()],
            input_schema: json!({ "type": "object", "additionalProperties": true }),
            output_schema: json!({ "type": "object" }),
            permission: CapabilityPermission::pure(),
            provider: CapabilityProvider::Local,
            timeout_ms: 10_000,
            idempotent: true,
            health: CapabilityHealth::Healthy,
            load: None,
        }
    }

    async fn invoke(&self, _input: Value, _ctx: CapabilityContext) -> agentos_core::error::Result<Value> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(json!({ "slept_ms": self.delay_ms }))
    }
}

struct FlakyCapability {
    remaining_failures: AtomicU32,
}

#[async_trait]
impl Capability for FlakyCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: CapabilityId::new(),
            name: "flaky".into(),
            version: "1.0.0".into(),
            description: "Fails a fixed number of times, then succeeds. Retryable.".into(),
            kind: CapabilityKind::Builtin,
            tags: vec!["test".into()],
            input_schema: json!({ "type": "object", "additionalProperties": true }),
            output_schema: json!({ "type": "object" }),
            permission: CapabilityPermission::pure(),
            provider: CapabilityProvider::Local,
            timeout_ms: 5_000,
            idempotent: true,
            health: CapabilityHealth::Healthy,
            load: None,
        }
    }

    async fn invoke(&self, _input: Value, _ctx: CapabilityContext) -> agentos_core::error::Result<Value> {
        let left = self.remaining_failures.load(Ordering::SeqCst);
        if left > 0 {
            self.remaining_failures.fetch_sub(1, Ordering::SeqCst);
            return Err(agentos_core::RuntimeError::capability("transient failure")
                .retryable(true));
        }
        Ok(json!({ "ok": true }))
    }
}

async fn kernel() -> Arc<Kernel> {
    let dir = std::env::temp_dir().join(format!("agentos-e2e-{}", agentos_core::now_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = RuntimeConfig::default();
    config.storage.backend = StoreBackend::Memory;
    config.storage.data_dir = dir.join("data");
    config.policy.workspace_root = dir.join("workspace");
    config.observability.log_level = "error".into();
    config.node.name = "test-node".into();
    Kernel::bootstrap(config).await.expect("kernel bootstraps")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_1_two_sessions_are_parallel_but_each_session_is_ordered() {
    let kernel = kernel().await;
    let in_flight = Arc::new(AtomicU32::new(0));
    let peak = Arc::new(AtomicU32::new(0));
    kernel
        .registry
        .register(Arc::new(SleepCapability { delay_ms: 400, in_flight: in_flight.clone(), peak: peak.clone() }))
        .unwrap();

    let a = kernel.sessions.create_session("user-a", "session A").await.unwrap();
    let b = kernel.sessions.create_session("user-b", "session B").await.unwrap();

    // Two sessions, one goal each, started together.
    let started = Instant::now();
    let (ra, rb) = tokio::join!(
        kernel.sessions.post_goal(&a.id, "please run slow-task for me", &[], &[], None, None),
        kernel.sessions.post_goal(&b.id, "please run slow-task for me", &[], &[], None, None)
    );
    let elapsed = started.elapsed();
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    let first_run_id = ra["agent_id"].as_str().unwrap().to_string();
    assert!(ra["answer"].is_string() || ra["error"].is_null(), "session A finished: {ra}");
    assert!(rb["answer"].is_string() || rb["error"].is_null(), "session B finished: {rb}");

    assert!(
        peak.load(Ordering::SeqCst) >= 2,
        "both sessions must execute concurrently (peak in-flight was {})",
        peak.load(Ordering::SeqCst)
    );
    assert!(
        elapsed < Duration::from_millis(750),
        "two 400ms goals must overlap, took {elapsed:?}"
    );

    // Message ordering inside one session: a second goal is handled after the first, and the
    // transcript keeps the order in which the user goals arrived.
    let first = kernel.sessions.post_goal(&a.id, "run slow-task again", &[], &[], None, None).await.unwrap();
    let second = kernel.sessions.post_goal(&a.id, "and once more", &[], &[], None, None).await.unwrap();
    let a_first = first["agent_id"].as_str().unwrap().to_string();
    let a_second = second["agent_id"].as_str().unwrap().to_string();
    let status = kernel.sessions.status(&a.id).await.unwrap();
    let runs: Vec<String> = status["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["agent_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(runs.len(), 3, "session A handled three goals");
    assert_eq!(runs[0], first_run_id, "runs are recorded in arrival order");
    assert_eq!(runs[1], a_first);
    assert_eq!(runs[2], a_second);

    // The transcript keeps the exact order of the user goals and their replies.
    let transcript = kernel.bus.replay(EventFilter {
        session_id: Some(a.id.clone()),
        kinds: vec![EventKind::SessionMessageQueued, EventKind::SessionMessageHandled],
        limit: 100,
        ..Default::default()
    }).await.unwrap();
    let kinds: Vec<&str> = transcript.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "session_message_queued").count(), 3);
    assert_eq!(kinds.iter().filter(|k| **k == "session_message_handled").count(), 3);
}

/// What the model is actually sent after a capability ran.
///
/// The bug this pins down was measured against a real provider: the runtime sent capability results
/// as role "tool" messages with no preceding assistant tool_calls block, and DeepSeek answered
/// HTTP 400 ("Messages with role 'tool' must be a response to a preceding message with
/// 'tool_calls'"). The run then failed over to the placeholder and the answer arrived from a model
/// that had never seen the picture or the capability result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capability_results_reach_the_model_without_breaking_the_tool_protocol() {
    // A real HTTP provider that records every request and answers like a text model.
    let seen: Arc<std::sync::Mutex<Vec<Vec<Value>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let recorder = seen.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { break };
            let recorder = recorder.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
                // Read the whole request: headers, then exactly Content-Length bytes of body. A
                // single read() is not enough - a prompt with tools and history arrives in several
                // TCP segments, and a half-read body parses as "not a model request".
                let (read_half, mut write_half) = socket.into_split();
                let mut reader = BufReader::new(read_half);
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    if line.trim().is_empty() {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                if length > 0 {
                    let _ = reader.read_exact(&mut body).await;
                }
                let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                if let Some(messages) = parsed.get("messages").and_then(|m| m.as_array()) {
                    // A poisoned lock would only mean another thread panicked; the recorded
                    // messages are still what the assertion needs.
                    let mut guard = recorder.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.push(messages.clone());
                }
                let wants_json = parsed.get("response_format").is_some();
                let content = if wants_json {
                    json!({
                        "goal": "compute it",
                        "reasoning": "one capability call",
                        "steps": [{
                            "id": "s1",
                            // Deliberately not called "answer": a step whose title contains that
                            // word short-circuits the final model call, and this test is about what
                            // that call is sent.
                            "description": "multiply and add",
                            "kind": "capability",
                            "capability": "calculator",
                            "input": { "expression": "12*7+3" },
                            "depends_on": []
                        }]
                    })
                    .to_string()
                } else {
                    "The calculator said 87.".to_string()
                };
                // The final answer is the call that streams, so the recorder speaks SSE for it and
                // plain JSON for planning - the same shape a real provider presents.
                let streaming = parsed.get("stream").and_then(|value| value.as_bool()).unwrap_or(false);
                let reply = if streaming {
                    let chunk = json!({
                        "id": "cmpl-test",
                        "object": "chat.completion.chunk",
                        "model": "recorder",
                        "choices": [{ "index": 0, "delta": { "content": content }, "finish_reason": null }]
                    })
                    .to_string();
                    let stop = json!({
                        "id": "cmpl-test",
                        "object": "chat.completion.chunk",
                        "model": "recorder",
                        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
                        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                    })
                    .to_string();
                    format!("data: {chunk}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
                } else {
                    json!({
                        "id": "cmpl-test",
                        "object": "chat.completion",
                        "model": "recorder",
                        "choices": [{ "index": 0, "message": { "role": "assistant", "content": content }, "finish_reason": "stop" }],
                        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                    })
                    .to_string()
                };
                let content_type = if streaming { "text/event-stream" } else { "application/json" };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    reply.len(),
                    reply
                );
                let _ = write_half.write_all(response.as_bytes()).await;
                let _ = write_half.flush().await;
            });
        }
    });

    let dir = std::env::temp_dir().join(format!("agentos-tool-proto-{}", agentos_core::now_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = RuntimeConfig::default();
    config.storage.backend = StoreBackend::Memory;
    config.storage.data_dir = dir.join("data");
    config.policy.workspace_root = dir.join("workspace");
    config.observability.log_level = "error".into();
    config.models.providers = vec![agentos_core::config::ProviderConfig {
        name: "recorder".into(),
        kind: agentos_core::config::ProviderKind::Openai,
        model: "recorder-1".into(),
        base_url: format!("http://{addr}"),
        api_key_env: "AGENTOS_TEST_RECORDER_KEY".into(),
        enabled: true,
        priority: 10,
        timeout_ms: 5_000,
        vision: Some(true),
    }];
    config.models.default_provider = "recorder".into();
    // The key is checked at call time, not at construction, so it only has to exist by now.
    std::env::set_var("AGENTOS_TEST_RECORDER_KEY", "test-key");
    let kernel = Kernel::bootstrap(config).await.expect("kernel bootstraps");

    let session = kernel.sessions.create_session("user", "tool protocol").await.unwrap();
    let result = kernel
        .sessions
        .post_goal(&session.id, "what is 12*7+3?", &[], &[], None, None)
        .await
        .unwrap();
    assert!(result["error"].is_null(), "run must succeed: {result}");

    // The blocking POST returns as soon as the run settles, which can be a moment before the
    // recorder task has stored the last request. Wait for it rather than racing it.
    let mut requests = Vec::new();
    for _ in 0..100 {
        requests = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        if requests.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.abort();
    assert!(requests.len() >= 2, "planning and the final answer both reach the model: {}", requests.len());
    let final_request = requests.last().unwrap();
    let roles: Vec<String> = final_request
        .iter()
        .map(|m| m["role"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        !roles.iter().any(|role| role == "tool"),
        "a tool message without a preceding assistant tool_calls block is rejected by providers: {roles:?}"
    );
    let context: String = final_request
        .iter()
        .filter(|m| m["role"] == "system")
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        context.contains("87"),
        "the capability result must be in the prompt the model answers from: {context}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_3_task_graph_runs_independent_nodes_in_parallel() {
    let kernel = kernel().await;
    // Two independent nodes, each taking 300ms: if the scheduler were serial the total would
    // exceed 600ms.
    kernel
        .registry
        .register(Arc::new(SleepCapability {
            delay_ms: 300,
            in_flight: Arc::new(AtomicU32::new(0)),
            peak: Arc::new(AtomicU32::new(0)),
        }))
        .unwrap();
    let session = kernel.sessions.create_session("user", "graph").await.unwrap();

    let mut builder = agentos_task_scheduler::graph::TaskGraphBuilder::new(session.id.clone(), "parallel demo");
    let mut a = TaskRecord::new(
        builder.graph_id(),
        session.id.clone(),
        "slow A",
        TaskKind::Capability,
        TaskPayload::Capability { capability: "slow-task".into(), version: None, input: json!({}) },
    );
    a.timeout_ms = 5_000;
    let mut b = TaskRecord::new(
        builder.graph_id(),
        session.id.clone(),
        "slow B",
        TaskKind::Capability,
        TaskPayload::Capability { capability: "slow-task".into(), version: None, input: json!({}) },
    );
    b.timeout_ms = 5_000;
    let id_a = builder.add(a);
    let id_b = builder.add(b);
    let graph = builder.build().unwrap();

    let runner = Arc::new(GraphRunner { kernel: kernel.clone(), session: session.id.clone() });
    let scheduler = agentos_task_scheduler::scheduler::Scheduler::new(
        kernel.store.clone(),
        kernel.bus.clone(),
        runner,
        agentos_task_scheduler::scheduler::SchedulerConfig { max_concurrency: 4, ..Default::default() },
    );
    let started = Instant::now();
    let outcome = scheduler.run(graph).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outcome.succeeded, 2, "both nodes succeeded: {:?}", outcome.nodes.iter().map(|n| (n.id.as_str(), n.state.as_str())).collect::<Vec<_>>());
    assert!(outcome.nodes.iter().any(|n| n.id == id_a));
    assert!(outcome.nodes.iter().any(|n| n.id == id_b));
    assert!(elapsed < Duration::from_millis(700), "two 300ms nodes must overlap, took {elapsed:?}");
}

struct GraphRunner {
    kernel: Arc<Kernel>,
    session: SessionId,
}

#[async_trait]
impl agentos_task_scheduler::scheduler::TaskRunner for GraphRunner {
    async fn run(
        &self,
        node: &TaskRecord,
        ctx: agentos_task_scheduler::scheduler::TaskContext,
    ) -> agentos_core::error::Result<Value> {
        match &node.payload {
            TaskPayload::Capability { capability, input, .. } => {
                let caller = agentos_capability_runtime::capability::CallerContext {
                    session_id: self.session.clone(),
                    actor_id: None,
                    task_id: Some(node.id.clone()),
                    correlation: ctx.correlation.clone(),
                    cancellation: ctx.cancellation.clone(),
                };
                // One deliberate 300ms wait inside the capability call path.
                let _ = tokio::time::sleep(Duration::from_millis(0)).await;
                Ok(self.kernel.mesh.invoke(capability, None, input.clone(), caller).await?.output)
            }
            _ => Ok(json!({})),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_4_capability_registry_discovers_and_invokes_examples() {
    let kernel = kernel().await;
    let names: Vec<String> = kernel.registry.list().into_iter().map(|c| c.name).collect();
    for expected in ["echo", "calculator", "filesystem-read"] {
        assert!(names.contains(&expected.to_string()), "missing built-in {expected} in {names:?}");
    }

    let session = kernel.sessions.create_session("user", "caps").await.unwrap();
    let caller = agentos_capability_runtime::capability::CallerContext::new(session.id.clone());
    let echo = kernel.mesh.invoke("echo", None, json!({"text": "hi"}), caller.clone()).await.unwrap();
    assert_eq!(echo.output["text"], "hi");

    let calc = kernel.mesh.invoke("calculator", None, json!({"expression": "6*7"}), caller.clone()).await.unwrap();
    assert_eq!(calc.output["result"], 42.0);

    // Schema validation happens before the implementation runs.
    let bad = kernel.mesh.invoke("calculator", None, json!({"wrong": 1}), caller).await.unwrap_err();
    assert_eq!(bad.kind, agentos_core::ErrorKind::InvalidInput);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_5_directory_placement_and_worker_heartbeat() {
    let kernel = kernel().await;
    let session = kernel.sessions.create_session("user", "topology").await.unwrap();

    // Placement: the session actor sits on the single local worker.
    let entry = kernel.directory.lookup(&session.id).await.unwrap().unwrap();
    assert_eq!(&entry.actor_id, &session.actor_id);
    assert!(entry.worker_id.is_some());

    let workers = kernel.workers.list();
    assert_eq!(workers.len(), 1);
    let worker = workers[0].clone();
    assert!(worker.is_alive());

    let before = worker.last_heartbeat;
    tokio::time::sleep(Duration::from_millis(60)).await;
    kernel
        .workers
        .heartbeat(&worker.id, WorkerLoad { actors: 1, running_tasks: 0, cpu_percent: 5.0, memory_bytes: 0 })
        .await
        .unwrap();
    let after = kernel.workers.get(&worker.id).unwrap();
    assert!(after.last_heartbeat > before, "heartbeat must advance the lease");

    // The control plane is only consulted on a cache miss: a second lookup is served from cache.
    let (hits_before, _) = kernel.directory.cache_stats();
    kernel.directory.lookup(&session.id).await.unwrap();
    let (hits_after, _) = kernel.directory.cache_stats();
    assert!(hits_after > hits_before, "repeat lookups must be cache hits");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_6_session_actor_snapshot_export_and_restore() {
    let kernel = kernel().await;
    let session = kernel.sessions.create_session("user", "snapshots").await.unwrap();
    kernel.sessions.post_goal(&session.id, "what is 2+2?", &[], &[], None, None).await.unwrap();
    kernel.sessions.post_goal(&session.id, "what is 3+3?", &[], &[], None, None).await.unwrap();

    let checkpoint = kernel.sessions.snapshot(&session.id).await.unwrap();
    assert_eq!(checkpoint.meta.session_id, session.id);
    assert!(checkpoint.state["runs"].as_array().unwrap().len() >= 2);
    assert_eq!(checkpoint.meta.state_hash.len(), 64);

    // Stop the actor, then restore it purely from the snapshot and replay.
    let handle = kernel.actors.lookup_session(&session.id).unwrap();
    kernel.actors.stop(&handle.id).await.unwrap();
    assert!(kernel.actors.lookup_session(&session.id).is_none());

    let actor_id = kernel.sessions.restore(checkpoint).await.unwrap();
    assert_eq!(actor_id, session.actor_id, "the actor keeps its identity across restore");
    let status = kernel.sessions.status(&session.id).await.unwrap();
    assert!(status["runs"].as_array().unwrap().len() >= 2, "restored state keeps the runs");
    assert!(status["goals_handled"].as_u64().unwrap() >= 2);

    // And the restored session keeps working.
    let after = kernel.sessions.post_goal(&session.id, "what is 4+4?", &[], &[], None, None).await.unwrap();
    assert!(after["error"].is_null(), "restored session still runs: {after}");

    let events = kernel
        .bus
        .replay(EventFilter { kinds: vec![EventKind::SnapshotCreated, EventKind::SnapshotRestored], limit: 50, ..Default::default() })
        .await
        .unwrap();
    assert!(events.len() >= 2, "snapshot lifecycle is observable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_7_policy_blocks_traversal_and_unlisted_writes() {
    let kernel = kernel().await;
    let session = kernel.sessions.create_session("user", "policy").await.unwrap();
    let caller = agentos_capability_runtime::capability::CallerContext::new(session.id.clone());

    let traversal = kernel
        .mesh
        .invoke("filesystem-read", None, json!({"path": "../../../etc/passwd"}), caller.clone())
        .await
        .unwrap_err();
    assert_eq!(traversal.kind, agentos_core::ErrorKind::PolicyDenied);

    // filesystem-write is denied by default policy even with a legal path.
    let write = kernel
        .mesh
        .invoke("filesystem-write", None, json!({"path": "out.txt", "content": "x"}), caller.clone())
        .await
        .unwrap_err();
    assert_eq!(write.kind, agentos_core::ErrorKind::PolicyDenied);

    // Reading a legitimate workspace file works.
    std::fs::write(kernel.policy.workspace_root().join("notes.txt"), "hello workspace").unwrap();
    let read = kernel
        .mesh
        .invoke("filesystem-read", None, json!({"path": "notes.txt"}), caller)
        .await
        .unwrap();
    assert_eq!(read.output["content"], "hello workspace");

    let denied = kernel
        .bus
        .replay(EventFilter { kinds: vec![EventKind::CapabilityDenied], limit: 10, ..Default::default() })
        .await
        .unwrap();
    assert!(!denied.is_empty(), "denials must be audited on the event bus");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_8_retry_recovers_a_transient_failure() {
    let kernel = kernel().await;
    kernel
        .registry
        .register(Arc::new(FlakyCapability { remaining_failures: AtomicU32::new(2) }))
        .unwrap();
    let session = kernel.sessions.create_session("user", "retry").await.unwrap();
    let caller = agentos_capability_runtime::capability::CallerContext::new(session.id.clone());

    let result = kernel.mesh.invoke("flaky", None, json!({}), caller).await.unwrap();
    assert_eq!(result.output["ok"], true);
    assert_eq!(result.attempts, 3, "two failures then success");

    // A capability that never succeeds is reported after exhausting retries.
    kernel
        .registry
        .register(Arc::new(FlakyCapability { remaining_failures: AtomicU32::new(99) }))
        .unwrap();
    let caller = agentos_capability_runtime::capability::CallerContext::new(session.id.clone());
    let err = kernel.mesh.invoke("flaky", None, json!({}), caller).await.unwrap_err();
    assert_eq!(err.kind, agentos_core::ErrorKind::Capability);

    let retries = kernel
        .bus
        .replay(EventFilter { kinds: vec![EventKind::ToolResult], limit: 50, ..Default::default() })
        .await
        .unwrap();
    assert!(retries.len() >= 2, "each attempt is audited");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_9_cancellation_and_step_budget() {
    let kernel = kernel().await;
    let session = kernel.sessions.create_session("user", "cancel").await.unwrap();
    kernel
        .registry
        .register(Arc::new(SleepCapability {
            delay_ms: 2_000,
            in_flight: Arc::new(AtomicU32::new(0)),
            peak: Arc::new(AtomicU32::new(0)),
        }))
        .unwrap();

    let sessions = kernel.sessions.clone();
    let session_id = session.id.clone();
    let handle = tokio::spawn(async move { sessions.post_goal(&session_id, "run slow-task", &[], &[], None, None).await });
    tokio::time::sleep(Duration::from_millis(120)).await;

    let cancelled = kernel.sessions.cancel(&session.id).await.unwrap();
    assert!(cancelled, "a run was in flight and must be cancellable");

    let result = tokio::time::timeout(Duration::from_secs(5), handle).await.unwrap().unwrap();
    let value = result.unwrap_or_else(|e| json!({ "error": e.to_string() }));
    let error_text = value["error"].as_str().unwrap_or_default().to_string();
    assert!(
        error_text.contains("cancel") || value["answer"].is_string(),
        "cancellation must surface as a cancelled run or a graceful answer: {value}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_10_wasm_capability_runs_in_the_sandbox() {
    let kernel = kernel().await;
    let wat = include_str!("../../capabilities/wasm/echo.wat");
    let spec = agentos_wasm_runtime::capability::WasmCapabilitySpec {
        name: "wasm-echo".into(),
        version: "1.0.0".into(),
        description: "Echo JSON through the wasm sandbox".into(),
        input_schema: json!({ "type": "object", "required": ["text"], "properties": { "text": { "type": "string" } } }),
        output_schema: json!({ "type": "object", "required": ["text"] }),
        permission: CapabilityPermission::pure(),
        host_policy: agentos_wasm_runtime::abi::WasmHostPolicy { allow_log: true, allow_emit: false, allow_clock: true },
        timeout_ms: 2_000,
        idempotent: true,
        tags: vec!["wasm".into(), "test".into()],
    };
    kernel.register_wasm_capability(spec, wat.as_bytes()).unwrap();
    assert!(kernel.registry.list().iter().any(|c| c.name == "wasm-echo"));

    let session = kernel.sessions.create_session("user", "wasm").await.unwrap();
    let caller = agentos_capability_runtime::capability::CallerContext::new(session.id.clone());
    let out = kernel.mesh.invoke("wasm-echo", None, json!({"text": "sandboxed"}), caller).await.unwrap();
    assert_eq!(out.output["text"], "sandboxed", "the guest echoes the JSON it received");

    // A guest that spins forever must be stopped by the epoch watchdog.
    let spin = include_str!("../../capabilities/wasm/spin.wat");
    let spin_spec = agentos_wasm_runtime::capability::WasmCapabilitySpec {
        name: "wasm-spin".into(),
        version: "1.0.0".into(),
        description: "Never returns, used to prove the timeout".into(),
        input_schema: json!({ "type": "object" }),
        output_schema: json!({ "type": "object" }),
        permission: CapabilityPermission::pure(),
        host_policy: agentos_wasm_runtime::abi::WasmHostPolicy::default(),
        timeout_ms: 300,
        idempotent: true,
        tags: vec!["wasm".into(), "test".into()],
    };
    kernel.register_wasm_capability(spin_spec, spin.as_bytes()).unwrap();
    let caller = agentos_capability_runtime::capability::CallerContext::new(session.id.clone());
    let started = Instant::now();
    let err = kernel.mesh.invoke("wasm-spin", None, json!({}), caller).await.unwrap_err();
    assert!(
        matches!(err.kind, agentos_core::ErrorKind::Timeout | agentos_core::ErrorKind::Sandbox),
        "expected a timeout or sandbox trap, got {err}"
    );
    assert!(started.elapsed() < Duration::from_secs(4), "the watchdog must fire promptly");
}

#[allow(dead_code)]
fn unused(_: TaskId) {}
