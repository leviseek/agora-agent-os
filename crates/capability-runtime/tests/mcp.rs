//! The MCP adapter, against a real child process speaking the protocol.

use agentos_capability_runtime::capability::{Capability, CallerContext, CapabilityContext};
use agentos_capability_runtime::mcp::{connect_stdio, McpCapability, McpServerConfig};
use agentos_core::model::CapabilityPermission;
use agentos_core::{CapabilityId, SessionId};
use serde_json::json;
use std::sync::Arc;

fn fixture(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: "node".to_string(),
        args: vec![concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-mcp-server.mjs").to_string()],
        env: Default::default(),
        enabled: true,
    }
}

fn context() -> CapabilityContext {
    CapabilityContext {
        capability_id: CapabilityId::new(),
        caller: CallerContext::new(SessionId::new()),
        permission: CapabilityPermission::pure().with_network().with_process_exec(),
        workspace: Arc::new(
            agentos_capability_runtime::workspace::Workspace::new(std::env::temp_dir()).unwrap(),
        ),
        artifacts: None,
        timeout_ms: 5_000,
    }
}

#[tokio::test]
async fn a_server_publishes_tools_and_answers_calls() {
    let connection = connect_stdio(&fixture("fake"), 5_000).await.unwrap();
    let names: Vec<String> = connection.tools.iter().map(|tool| tool.name.clone()).collect();
    assert!(names.contains(&"echo".to_string()), "got {names:?}");

    let client = connection.client.clone();
    let echoed = client.call_tool("echo", json!({ "text": "hi" })).await.unwrap();
    assert_eq!(echoed["text"], json!("echo: hi"));
    assert_eq!(echoed["is_error"], json!(false));

    // A tool that reports an error is a result, not a transport failure: the difference matters,
    // because one is the server working and the other is the server broken.
    let failed = client.call_tool("fail", json!({})).await.unwrap();
    assert_eq!(failed["is_error"], json!(true));
    assert_eq!(failed["text"], json!("deliberate failure"));

    // An unknown tool is a protocol error.
    let unknown = client.call_tool("nope", json!({})).await;
    assert!(unknown.is_err());

    client.shutdown().await;
}

#[tokio::test]
async fn tools_arrive_as_capabilities_with_untrusted_permissions() {
    let connection = connect_stdio(&fixture("fake"), 5_000).await.unwrap();
    let echo = connection
        .tools
        .iter()
        .find(|tool| tool.name == "echo")
        .cloned()
        .unwrap();
    let capability = McpCapability::new("fake", echo, connection.client.clone());

    let descriptor = capability.descriptor();
    assert_eq!(descriptor.name.as_str(), "mcp.fake.echo");
    assert!(descriptor.tags.contains(&"external".to_string()));
    // Code we did not write asks for the widest permissions; policy decides, not the server.
    assert!(descriptor.permission.process_exec);
    assert!(descriptor.permission.network);
    assert!(!descriptor.permission.fs_write);

    let output = capability.invoke(json!({ "text": "through the mesh" }), context()).await.unwrap();
    assert_eq!(output["text"], json!("echo: through the mesh"));
    assert_eq!(output["server"], json!("fake"));

    // An error result surfaces as a capability error, not as a silent empty success.
    let failing = connection
        .tools
        .iter()
        .find(|tool| tool.name == "fail")
        .cloned()
        .unwrap();
    let failing = McpCapability::new("fake", failing, connection.client.clone());
    let error = failing.invoke(json!({}), context()).await.unwrap_err();
    assert!(error.message.contains("deliberate failure"), "got: {}", error.message);

    connection.client.shutdown().await;
}

#[tokio::test]
async fn a_schema_we_cannot_validate_falls_back_to_a_permissive_one() {
    let connection = connect_stdio(&fixture("fake"), 5_000).await.unwrap();
    let weird = connection
        .tools
        .iter()
        .find(|tool| tool.name == "weird")
        .cloned()
        .unwrap();
    assert_eq!(
        weird.input_schema,
        json!({ "type": "object" }),
        "a union type is not something our validator understands, so we do not claim to check it"
    );
    connection.client.shutdown().await;
}

#[tokio::test]
async fn a_server_that_cannot_start_is_a_clean_error() {
    let mut config = fixture("missing");
    config.command = "definitely-not-a-real-binary-xyz".to_string();
    let error = connect_stdio(&config, 1_000).await.unwrap_err();
    assert!(error.message.contains("cannot start mcp server missing"), "got: {}", error.message);
}