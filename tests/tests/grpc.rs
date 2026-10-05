//! The kernel served over gRPC, driven by the gRPC clients.
//!
//! This is the multi-node shape: a client on one machine creates a session, posts a goal and
//! reads the result through the AgentService, and invokes capabilities through the
//! CapabilityService.

use agentos_core::config::{RuntimeConfig, StoreBackend};
use agentos_kernel::Kernel;
use agentos_network::grpc::{GrpcAgentClient, GrpcCapabilityClient, RemoteInvocation};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn started_kernel() -> (Arc<Kernel>, CancellationToken) {
    let dir = std::env::temp_dir().join(format!("agentos-grpc-{}", agentos_core::now_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = RuntimeConfig::default();
    config.storage.backend = StoreBackend::Memory;
    config.storage.data_dir = dir.join("data");
    config.policy.workspace_root = dir.join("workspace");
    config.observability.log_level = "error".into();
    let kernel = Kernel::bootstrap(config).await.unwrap();
    (kernel, CancellationToken::new())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_client_can_run_a_goal_and_invoke_a_capability() {
    let (kernel, shutdown) = started_kernel().await;
    let addr = agentos_kernel::transports::serve_grpc(kernel.clone(), "127.0.0.1:0".into(), shutdown.clone())
        .await
        .expect("grpc server starts");
    let endpoint = format!("http://{addr}");

    // --- AgentService ---------------------------------------------------------------------
    let agent = GrpcAgentClient::connect(endpoint.clone()).await.expect("agent client connects");
    let (session, actor) = agent
        .create_session("remote-user", "created over grpc")
        .await
        .expect("session created remotely");
    assert!(session.as_str().starts_with("ses_"));
    assert!(actor.as_str().starts_with("act_"));

    let result = agent
        .post_goal(&session, "what is 11*11?", true)
        .await
        .expect("goal accepted");
    assert!(result["error"].is_null(), "remote run failed: {result}");
    assert!(
        result["answer"].as_str().unwrap_or_default().contains("121"),
        "answer must carry the capability result: {result}"
    );

    let session_view = agent.get_session(&session).await.expect("session readable");
    assert_eq!(session_view["id"], session.as_str());

    let snapshot = agent.snapshot(&session).await.expect("snapshot over grpc");
    assert!(snapshot["meta"]["id"].as_str().unwrap().starts_with("ckp_"));

    let health = agent.health().await.expect("health over grpc");
    assert_eq!(health["capabilities"].as_u64().unwrap() >= 6, true);

    // --- CapabilityService ----------------------------------------------------------------
    let capabilities = GrpcCapabilityClient::connect(endpoint.clone()).await.expect("capability client");
    let listed = capabilities.list("calc").await.expect("list over grpc");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "calculator");

    let invoked = capabilities
        .invoke(RemoteInvocation {
            capability: "calculator".into(),
            version: "1".into(),
            input: json!({ "expression": "6*8" }),
            session_id: Some(session.clone()),
            actor_id: None,
            task_id: None,
            workspace_id: None,
            timeout_ms: 5_000,
        })
        .await
        .expect("remote capability call");
    assert_eq!(invoked.output["result"], 48.0);

    // Policy is enforced on the remote path too.
    let denied = capabilities
        .invoke(RemoteInvocation {
            capability: "filesystem-read".into(),
            version: String::new(),
            input: json!({ "path": "../../etc/passwd" }),
            session_id: Some(session.clone()),
            actor_id: None,
            task_id: None,
            workspace_id: None,
            timeout_ms: 5_000,
        })
        .await
        .expect_err("traversal denied remotely as well");
    assert_eq!(denied.kind, agentos_core::ErrorKind::PolicyDenied);

    shutdown.cancel();
}
