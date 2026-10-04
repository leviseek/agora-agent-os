//! gRPC round trip: server, client and the capability transport adapter.
//!
//! Uses an in-process handler, so the test proves the generated stubs, the error mapping and the
//! transport adapter without needing a kernel.

use agentos_capability_runtime::transport::CapabilityTransport;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::SessionId;
use agentos_network::grpc::{
    CapabilityHandler, GrpcCapabilityClient, GrpcServer, RemoteInvocation, RemoteInvocationResult,
};
use agentos_network::rpc;
use agentos_network::GrpcCapabilityTransport;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct StubHandler {
    calls: AtomicU32,
}

#[async_trait]
impl CapabilityHandler for StubHandler {
    async fn invoke(&self, request: RemoteInvocation) -> Result<RemoteInvocationResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match request.capability.as_str() {
            "echo" => Ok(RemoteInvocationResult {
                output: request.input,
                duration_ms: 1,
                attempts: 1,
                provider: "stub".into(),
            }),
            "calculator" => {
                let expression = request.input.get("expression").and_then(|v| v.as_str()).unwrap_or("");
                if expression == "boom" {
                    return Err(RuntimeError::capability("the calculator exploded"));
                }
                Ok(RemoteInvocationResult {
                    output: json!({ "expression": expression, "result": 42 }),
                    duration_ms: 2,
                    attempts: 2,
                    provider: "stub".into(),
                })
            }
            other => Err(RuntimeError::not_found(format!("capability {other} is not registered"))),
        }
    }

    async fn list(&self) -> Result<Vec<rpc::CapabilityInfo>> {
        Ok(vec![rpc::CapabilityInfo {
            id: "cap_test".into(),
            name: "echo".into(),
            version: "1.0.0".into(),
            description: "stub".into(),
            kind: "builtin".into(),
            tags: vec!["test".into()],
            input_schema_json: "{}".into(),
            output_schema_json: "{}".into(),
            permission: String::new(),
            health: "healthy".into(),
            inflight: 0,
            total_calls: self.calls.load(Ordering::SeqCst) as u64,
        }])
    }
}

async fn start_server() -> (String, Arc<StubHandler>, CancellationToken) {
    let handler = Arc::new(StubHandler { calls: AtomicU32::new(0) });
    let token = CancellationToken::new();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = GrpcServer::new("127.0.0.1:0".parse().unwrap())
        .with_capability(handler.clone() as Arc<dyn CapabilityHandler>);
    let shutdown = token.clone();
    tokio::spawn(async move {
        let _ = server.serve(shutdown, Some(tx)).await;
    });
    let addr = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .expect("server starts")
        .expect("address is reported");
    (format!("http://{addr}"), handler, token)
}

#[tokio::test]
async fn capability_service_round_trips_and_maps_errors() {
    let (endpoint, handler, shutdown) = start_server().await;
    let client = GrpcCapabilityClient::connect(endpoint).await.expect("client connects");

    let ok = client
        .invoke(RemoteInvocation {
            capability: "echo".into(),
            version: "1.0.0".into(),
            input: json!({"text": "over the wire"}),
            session_id: Some(SessionId::new()),
            actor_id: None,
            task_id: None,
            timeout_ms: 5_000,
        })
        .await
        .expect("echo succeeds");
    assert_eq!(ok.output["text"], "over the wire");
    assert_eq!(ok.provider, "stub");

    // A capability error must arrive as a structured RuntimeError, not as a gRPC status.
    let err = client
        .invoke(RemoteInvocation {
            capability: "calculator".into(),
            version: String::new(),
            input: json!({"expression": "boom"}),
            session_id: None,
            actor_id: None,
            task_id: None,
            timeout_ms: 5_000,
        })
        .await
        .expect_err("capability error propagates");
    assert_eq!(err.kind, agentos_core::ErrorKind::Capability);
    assert!(err.message.contains("exploded"));

    // Unknown capability keeps its not_found kind across the wire.
    let missing = client
        .invoke(RemoteInvocation {
            capability: "nope".into(),
            version: String::new(),
            input: json!({}),
            session_id: None,
            actor_id: None,
            task_id: None,
            timeout_ms: 5_000,
        })
        .await
        .expect_err("missing capability");
    assert_eq!(missing.kind, agentos_core::ErrorKind::NotFound);

    let listed = client.list("").await.expect("list works");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "echo");

    assert_eq!(handler.calls.load(Ordering::SeqCst), 3);
    shutdown.cancel();
}

#[tokio::test]
async fn transport_adapter_speaks_the_capability_transport_trait() {
    let (endpoint, _handler, shutdown) = start_server().await;
    let transport = GrpcCapabilityTransport::connect(endpoint.clone()).await.expect("transport connects");
    assert_eq!(transport.name(), "grpc");

    let output: Value = transport
        .call(&endpoint, "echo", "1.0.0", json!({"text": "through the mesh"}), 2_000)
        .await
        .expect("call succeeds");
    assert_eq!(output["text"], "through the mesh");

    // A mismatched endpoint is a programming error, not a transient failure.
    let wrong = transport.call("http://127.0.0.1:1", "echo", "1.0.0", json!({}), 1_000).await;
    let err = wrong.expect_err("endpoint mismatch is rejected");
    assert!(!err.is_retryable());
    shutdown.cancel();
}
