//! MCP (Model Context Protocol) as a capability source.
//!
//! An MCP server is an external process. We cannot see inside it, so we do not pretend to: every
//! tool it publishes is registered with `process_exec` and `network` permissions, which means the
//! policy gate denies it until an operator allow-lists the server (or a prefix of its tools).
//! The alternative - trusting whatever a server says about itself - would make the policy seam
//! decorative at exactly the point where it matters most.
//!
//! Transport: newline-delimited JSON-RPC over stdio, the framing the MCP specification defines.

use crate::capability::{Capability, CapabilityContext};
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{
    CapabilityDescriptor, CapabilityKind, CapabilityPermission, CapabilityProvider,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

/// The configuration type lives in core: configuration is a core concern, and this crate depends
/// on core rather than the other way round.
pub use agentos_core::config::McpServerConfig;

/// A tool as the server describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>>>;

/// A connected MCP server over stdio.
#[derive(Debug)]
pub struct StdioMcpClient {
    server: String,
    child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    timeout_ms: u64,
}

/// A connection plus the tools it advertised.
#[derive(Debug)]
pub struct McpConnection {
    pub client: Arc<StdioMcpClient>,
    pub tools: Vec<McpTool>,
    pub server_info: Value,
}

impl StdioMcpClient {
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Send a request and wait for its response, bounded by the configured timeout.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);

        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        if let Err(error) = self.write_line(&line).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(Duration::from_millis(self.timeout_ms), receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(RuntimeError::capability(format!(
                "mcp server {} rejected {method}: {message}",
                self.server
            ))),
            Ok(Err(_)) => Err(RuntimeError::capability(format!(
                "mcp server {} closed the connection during {method}",
                self.server
            ))),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RuntimeError::timeout(format!(
                    "mcp server {} did not answer {method} within {} ms",
                    self.server, self.timeout_ms
                )))
            }
        }
    }

    /// Send a notification: no id, no response expected.
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let line = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
        self.write_line(&line).await
    }

    async fn write_line(&self, line: &str) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| RuntimeError::capability(format!("cannot write to mcp server {}: {error}", self.server)))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|error| RuntimeError::capability(format!("cannot write to mcp server {}: {error}", self.server)))?;
        stdin
            .flush()
            .await
            .map_err(|error| RuntimeError::capability(format!("cannot flush mcp server {}: {error}", self.server)))?;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let listed = self.request("tools/list", json!({})).await?;
        let tools = listed
            .get("tools")
            .and_then(|value| value.as_array())
            .map(|tools| tools.iter().filter_map(parse_tool).collect())
            .unwrap_or_default();
        Ok(tools)
    }

    /// Call a tool and normalise the result.
    pub async fn call_tool(&self, tool: &str, arguments: Value) -> Result<Value> {
        let result = self
            .request("tools/call", json!({ "name": tool, "arguments": arguments }))
            .await?;
        let is_error = result.get("isError").and_then(|value| value.as_bool()).unwrap_or(false);
        let content = result.get("content").cloned().unwrap_or(Value::Null);
        let text = content
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok(json!({
            "server": self.server,
            "text": text,
            "is_error": is_error,
            "content": content,
        }))
    }

    /// Stop the server. Dropping the client does this too, because the child is kill_on_drop.
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
    }
}

fn parse_tool(value: &Value) -> Option<McpTool> {
    let name = value.get("name")?.as_str()?.to_string();
    let description = value
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or_default()
        .to_string();
    let input_schema = sanitise_schema(value.get("inputSchema"));
    Some(McpTool { name, description, input_schema })
}

/// Keep a schema only if our validator can reason about it.
///
/// The runtime validates a documented subset of JSON Schema and treats an object root as the norm.
/// A server that publishes something else (a union type, a bare string) gets a permissive object
/// schema instead: the call still reaches the server, which remains the authority on its own
/// arguments, but we never claim to have validated what we did not understand.
fn sanitise_schema(schema: Option<&Value>) -> Value {
    let Some(schema) = schema else {
        return json!({ "type": "object" });
    };
    let Some(object) = schema.as_object() else {
        return json!({ "type": "object" });
    };
    match object.get("type") {
        None => json!({ "type": "object" }),
        Some(Value::String(kind)) if kind == "object" => schema.clone(),
        Some(_) => json!({ "type": "object" }),
    }
}

/// Start a server, complete the handshake and list its tools.
pub async fn connect_stdio(config: &McpServerConfig, timeout_ms: u64) -> Result<McpConnection> {
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A server that outlives the runtime is a leak the operator cannot see.
        .kill_on_drop(true);
    for (key, value) in &config.env {
        command.env(key, value);
    }

    let mut child = command.spawn().map_err(|error| {
        RuntimeError::capability(format!("cannot start mcp server {}: {error}", config.name))
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| RuntimeError::capability("mcp server has no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RuntimeError::capability("mcp server has no stdout"))?;
    let stderr = child.stderr.take();

    let client = Arc::new(StdioMcpClient {
        server: config.name.clone(),
        child: Mutex::new(child),
        stdin: Arc::new(Mutex::new(stdin)),
        pending: Arc::new(Mutex::new(HashMap::new())),
        next_id: AtomicU64::new(0),
        timeout_ms,
    });

    let server = config.name.clone();
    if let Some(stderr) = stderr {
        // Drain stderr: a chatty server must not block on a full pipe, and its complaints are
        // worth keeping even when nobody is looking at the terminal.
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(server = %server, "mcp: {line}");
            }
        });
    }

    spawn_reader(client.clone(), stdout);

    let info = client
        .request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "agora-agent-os", "version": env!("CARGO_PKG_VERSION") }
            }),
        )
        .await?;
    client.notify("notifications/initialized", json!({})).await?;
    let tools = client.list_tools().await?;
    Ok(McpConnection { client, tools, server_info: info })
}

fn spawn_reader(client: Arc<StdioMcpClient>, stdout: tokio::process::ChildStdout) {
    let pending = client.pending.clone();
    let stdin = client.stdin.clone();
    let server = client.server.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                tracing::debug!(server = %server, "mcp: unparseable line");
                continue;
            };
            if let Some(method) = message.get("method").and_then(|value| value.as_str()) {
                // Server-initiated traffic. v1 supports none of it (sampling, roots), and a
                // request left unanswered would hang the server, so requests get an error back.
                if let Some(id) = message.get("id") {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("{method} is not supported by this client") }
                    })
                    .to_string();
                    let mut guard = stdin.lock().await;
                    let _ = guard.write_all(reply.as_bytes()).await;
                    let _ = guard.write_all(b"\n").await;
                    let _ = guard.flush().await;
                }
                continue;
            }
            let Some(id) = message.get("id").and_then(|value| value.as_u64()) else {
                continue;
            };
            let Some(sender) = pending.lock().await.remove(&id) else {
                continue;
            };
            let outcome = match message.get("error") {
                Some(error) => Err(error
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown error")
                    .to_string()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = sender.send(outcome);
        }
        // The pipe closed: fail everything still waiting instead of letting it time out.
        for (_, sender) in pending.lock().await.drain() {
            let _ = sender.send(Err("connection closed".to_string()));
        }
    });
}

/// One MCP tool, dressed as a capability.
///
/// Callers cannot tell it from a built-in - which is the point of the mesh - but the descriptor
/// says exactly what it is: the tags mark it external, and the permission marks it as code we do
/// not control.
pub struct McpCapability {
    server: String,
    tool: McpTool,
    client: Arc<StdioMcpClient>,
}

impl McpCapability {
    pub fn new(server: impl Into<String>, tool: McpTool, client: Arc<StdioMcpClient>) -> Self {
        Self { server: server.into(), tool, client }
    }

    /// The capability name a policy entry has to cover.
    pub fn capability_name(server: &str, tool: &str) -> String {
        format!("mcp.{server}.{tool}")
    }
}

#[async_trait]
impl Capability for McpCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        let description = if self.tool.description.trim().is_empty() {
            format!("MCP tool {} from server {}", self.tool.name, self.server)
        } else {
            format!("[MCP {}] {}", self.server, self.tool.description.trim())
        };
        let mut d = crate::builtins::descriptor(
            &McpCapability::capability_name(&self.server, &self.tool.name),
            "1.0.0",
            &description,
            CapabilityKind::Remote,
            &["mcp", "external", "untrusted"],
            self.tool.input_schema.clone(),
            // An MCP result is free-form by specification, so the contract we can honestly state
            // is "an object"; the content lives in the content/text fields.
            json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "text": { "type": "string" },
                    "is_error": { "type": "boolean" },
                    "content": {}
                }
            }),
            // External process: unknown code, so it asks for the widest permissions and the
            // policy gate decides. Nothing here is trusted because the server said it was safe.
            CapabilityPermission::pure().with_network().with_process_exec(),
        );
        d.provider = CapabilityProvider::Endpoint(format!("mcp:{}", self.server));
        d
    }

    async fn invoke(&self, input: Value, _ctx: CapabilityContext) -> Result<Value> {
        let outcome = self.client.call_tool(&self.tool.name, input).await?;
        if outcome.get("is_error").and_then(|value| value.as_bool()).unwrap_or(false) {
            let text = outcome.get("text").and_then(|value| value.as_str()).unwrap_or_default();
            return Err(RuntimeError::capability(format!(
                "mcp tool {} on {} reported an error: {text}",
                self.tool.name, self.server
            )));
        }
        Ok(outcome)
    }
}
