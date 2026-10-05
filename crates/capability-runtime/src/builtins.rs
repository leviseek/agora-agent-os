//! Built-in capabilities plus the remote proxy that makes a network capability look local.

use crate::capability::{Capability, CapabilityContext};
use crate::transport::CapabilityTransport;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{
    CapabilityDescriptor, CapabilityKind, CapabilityPermission, CapabilityProvider,
};
use agentos_core::state::CapabilityHealth;
use agentos_core::{CapabilityId, now_ms};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

#[allow(clippy::too_many_arguments)]
pub(crate) fn descriptor(
    name: &str,
    version: &str,
    description: &str,
    kind: CapabilityKind,
    tags: &[&str],
    input_schema: Value,
    output_schema: Value,
    permission: CapabilityPermission,
) -> CapabilityDescriptor {
    CapabilityDescriptor {
        id: CapabilityId::new(),
        name: name.to_string(),
        version: version.to_string(),
        description: description.to_string(),
        kind,
        tags: tags.iter().map(|t| t.to_string()).collect(),
        input_schema,
        output_schema,
        permission,
        provider: CapabilityProvider::Local,
        timeout_ms: 5_000,
        idempotent: true,
        health: CapabilityHealth::Healthy,
        load: None,
    }
}

/// Echo. The smallest possible capability, and the one the tests use as a control.
pub struct EchoCapability;

impl EchoCapability {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EchoCapability {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Capability for EchoCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        descriptor(
            "echo",
            "1.0.0",
            "Return the input text unchanged. Useful for connectivity and policy checks.",
            CapabilityKind::Builtin,
            &["text", "diagnostic"],
            json!({
                "type": "object",
                "required": ["text"],
                "additionalProperties": false,
                "properties": { "text": { "type": "string", "maxLength": 65536 } }
            }),
            json!({
                "type": "object",
                "required": ["text", "length"],
                "properties": {
                    "text": { "type": "string" },
                    "length": { "type": "number" }
                }
            }),
            CapabilityPermission::pure(),
        )
    }

    async fn invoke(&self, input: Value, ctx: CapabilityContext) -> Result<Value> {
        let text = input
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::invalid_input("echo requires a text field"))?;
        if ctx.is_cancelled() {
            return Err(RuntimeError::cancelled("echo cancelled"));
        }
        Ok(json!({
            "text": text,
            "length": text.chars().count(),
            "capability_id": ctx.capability_id.as_str(),
        }))
    }
}

/// Calculator. A hand written recursive descent parser: no eval crate, no code execution.
pub struct CalculatorCapability;

impl CalculatorCapability {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CalculatorCapability {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Capability for CalculatorCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        descriptor(
            "calculator",
            "1.0.0",
            "Evaluate arithmetic (+, -, *, /, %, ^, parentheses). Several expressions may be \
             separated by ; or newlines, and each result comes back in order.",
            CapabilityKind::Builtin,
            &["math", "deterministic"],
            json!({
                "type": "object",
                "required": ["expression"],
                "additionalProperties": false,
                "properties": {
                    "expression": { "type": "string", "minLength": 1, "maxLength": 4096 },
                    // Listed on purpose, although the mesh drops unknown fields anyway: a schema that
                    // describes what a caller keeps trying to send is a better hint than a silent
                    // discard, and models do annotate their own calls.
                    "note": { "type": "string", "maxLength": 512 }
                }
            }),
            json!({
                "type": "object",
                "required": ["expression", "result"],
                "properties": {
                    "expression": { "type": "string" },
                    "result": { "type": "number" },
                    "results": { "type": "array", "items": { "type": "number" } }
                }
            }),
            CapabilityPermission::pure(),
        )
    }

    async fn invoke(&self, input: Value, _ctx: CapabilityContext) -> Result<Value> {
        let expression = input
            .get("expression")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::invalid_input("calculator requires an expression field"))?;
        // Several expressions in one call. Models write "a+b; c+d" when asked for two sums, and
        // answering that with "unexpected trailing input" fails the step - which then cascades into
        // an answer about the failure. Each expression is still evaluated strictly; only the
        // separator is new.
        let parts: Vec<&str> = expression
            .split([';', '\n'])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            return Err(RuntimeError::invalid_input("the expression is empty"));
        }
        let mut results = Vec::with_capacity(parts.len());
        for part in &parts {
            results.push(evaluate(part)?);
        }
        let last = *results.last().expect("at least one expression");
        Ok(json!({
            "expression": expression,
            // The single value answers the common single-expression case; the list is always there
            // for a caller that asked for several.
            "result": if results.len() == 1 { results[0] } else { last },
            "results": results,
        }))
    }
}

/// Evaluate an arithmetic expression. Rejects anything that is not arithmetic.
pub fn evaluate(expression: &str) -> Result<f64> {
    let mut parser = Parser { chars: expression.chars().collect(), pos: 0 };
    let value = parser.expression()?;
    parser.skip_ws();
    if parser.pos != parser.chars.len() {
        return Err(RuntimeError::invalid_input(format!(
            "unexpected trailing input at position {}",
            parser.pos
        )));
    }
    if !value.is_finite() {
        return Err(RuntimeError::invalid_input("result is not a finite number"));
    }
    Ok(value)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn skip_ws(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_ws();
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expression(&mut self) -> Result<f64> {
        let mut value = self.term()?;
        loop {
            match self.peek() {
                Some('+') => {
                    self.pos += 1;
                    value += self.term()?;
                }
                Some('-') => {
                    self.pos += 1;
                    value -= self.term()?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn term(&mut self) -> Result<f64> {
        let mut value = self.power()?;
        loop {
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    value *= self.power()?;
                }
                Some('/') => {
                    self.pos += 1;
                    let rhs = self.power()?;
                    if rhs == 0.0 {
                        return Err(RuntimeError::invalid_input("division by zero"));
                    }
                    value /= rhs;
                }
                Some('%') => {
                    self.pos += 1;
                    let rhs = self.power()?;
                    if rhs == 0.0 {
                        return Err(RuntimeError::invalid_input("modulo by zero"));
                    }
                    value %= rhs;
                }
                _ => return Ok(value),
            }
        }
    }

    fn power(&mut self) -> Result<f64> {
        let base = self.unary()?;
        if self.eat('^') {
            let exponent = self.power()?;
            return Ok(base.powf(exponent));
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<f64> {
        match self.peek() {
            Some('-') => {
                self.pos += 1;
                Ok(-self.unary()?)
            }
            Some('+') => {
                self.pos += 1;
                self.unary()
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<f64> {
        if self.eat('(') {
            let value = self.expression()?;
            if !self.eat(')') {
                return Err(RuntimeError::invalid_input("missing closing parenthesis"));
            }
            return Ok(value);
        }
        self.skip_ws();
        let start = self.pos;
        while self.pos < self.chars.len()
            && (self.chars[self.pos].is_ascii_digit() || self.chars[self.pos] == '.')
        {
            self.pos += 1;
        }
        if start == self.pos {
            return Err(RuntimeError::invalid_input(format!(
                "expected a number at position {start}"
            )));
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f64>()
            .map_err(|_| RuntimeError::invalid_input(format!("invalid number: {text}")))
    }
}

/// Read a file inside the workspace. The workspace jail rejects traversal and absolute paths
/// before any IO happens.
pub struct FilesystemReadCapability {
    max_bytes: u64,
}

impl FilesystemReadCapability {
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

impl Default for FilesystemReadCapability {
    fn default() -> Self {
        Self::new(256 * 1024)
    }
}

#[async_trait]
impl Capability for FilesystemReadCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        let mut d = descriptor(
            "filesystem-read",
            "1.0.0",
            "Read a UTF-8 file from the workspace. Absolute paths and traversal are denied.",
            CapabilityKind::Builtin,
            &["fs", "workspace", "read"],
            json!({
                "type": "object",
                "required": ["path"],
                "additionalProperties": false,
                "properties": { "path": { "type": "string", "minLength": 1, "maxLength": 512 } }
            }),
            json!({
                "type": "object",
                "required": ["path", "content", "bytes"],
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "bytes": { "type": "number" }
                }
            }),
            CapabilityPermission::read_only_fs(),
        );
        d.timeout_ms = 3_000;
        d
    }

    async fn invoke(&self, input: Value, ctx: CapabilityContext) -> Result<Value> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::invalid_input("filesystem-read requires a path field"))?;
        if !ctx.permission.fs_read {
            return Err(RuntimeError::policy_denied("filesystem read permission was not granted"));
        }
        let content = ctx.workspace.read_to_string(path, self.max_bytes).await?;
        Ok(json!({ "path": path, "content": content, "bytes": content.len() }))
    }
}

/// List a directory inside the workspace.
pub struct FilesystemListCapability;

#[async_trait]
impl Capability for FilesystemListCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        descriptor(
            "filesystem-list",
            "1.0.0",
            "List a directory inside the workspace.",
            CapabilityKind::Builtin,
            &["fs", "workspace", "read"],
            json!({
                "type": "object",
                "required": ["path"],
                "additionalProperties": false,
                "properties": { "path": { "type": "string", "maxLength": 512 } }
            }),
            json!({
                "type": "object",
                "required": ["path", "entries"],
                "properties": {
                    "path": { "type": "string" },
                    "entries": { "type": "array", "items": { "type": "object" } }
                }
            }),
            CapabilityPermission::read_only_fs(),
        )
    }

    async fn invoke(&self, input: Value, ctx: CapabilityContext) -> Result<Value> {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        if !ctx.permission.fs_read {
            return Err(RuntimeError::policy_denied("filesystem read permission was not granted"));
        }
        let entries = ctx.workspace.list(path, 500).await?;
        Ok(json!({ "path": path, "entries": entries }))
    }
}

/// Write a file inside the workspace. Denied by default policy, because it mutates the host.
pub struct FilesystemWriteCapability {
    max_bytes: u64,
}

impl FilesystemWriteCapability {
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

#[async_trait]
impl Capability for FilesystemWriteCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        let mut d = descriptor(
            "filesystem-write",
            "1.0.0",
            "Write a UTF-8 file inside the workspace. Requires an explicit policy grant.",
            CapabilityKind::Builtin,
            &["fs", "workspace", "write", "mutating"],
            json!({
                "type": "object",
                "required": ["path", "content"],
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string", "minLength": 1, "maxLength": 512 },
                    "content": { "type": "string", "maxLength": 1048576 }
                }
            }),
            json!({
                "type": "object",
                "required": ["path", "bytes"],
                "properties": { "path": { "type": "string" }, "bytes": { "type": "number" } }
            }),
            CapabilityPermission::read_only_fs().with_fs_write(),
        );
        d.idempotent = false;
        d
    }

    async fn invoke(&self, input: Value, ctx: CapabilityContext) -> Result<Value> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::invalid_input("filesystem-write requires a path field"))?;
        let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if !ctx.permission.fs_write {
            return Err(RuntimeError::policy_denied("filesystem write permission was not granted"));
        }
        let bytes = ctx.workspace.write(path, content, self.max_bytes).await?;
        Ok(json!({ "path": path, "bytes": bytes }))
    }
}

/// Current time. Included because agents constantly need a clock and it must not be faked.
pub struct ClockCapability;

#[async_trait]
impl Capability for ClockCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        descriptor(
            "clock",
            "1.0.0",
            "Return the current time of the runtime node.",
            CapabilityKind::Builtin,
            &["time", "deterministic-input"],
            json!({ "type": "object", "additionalProperties": false, "properties": {} }),
            json!({
                "type": "object",
                "required": ["epoch_ms", "rfc3339"],
                "properties": { "epoch_ms": { "type": "number" }, "rfc3339": { "type": "string" } }
            }),
            CapabilityPermission::pure(),
        )
    }

    async fn invoke(&self, _input: Value, _ctx: CapabilityContext) -> Result<Value> {
        let now = now_ms();
        Ok(json!({ "epoch_ms": now, "rfc3339": agentos_core::time::to_rfc3339(now) }))
    }
}

/// Wraps a capability that executes somewhere else. Callers cannot tell the difference, which is
/// the whole point of the mesh.
pub struct RemoteCapability {
    descriptor: CapabilityDescriptor,
    transport: Arc<dyn CapabilityTransport>,
    endpoint: String,
}

impl RemoteCapability {
    pub fn new(
        mut descriptor: CapabilityDescriptor,
        transport: Arc<dyn CapabilityTransport>,
        endpoint: impl Into<String>,
    ) -> Self {
        let endpoint = endpoint.into();
        descriptor.kind = CapabilityKind::Remote;
        descriptor.provider = CapabilityProvider::Endpoint(endpoint.clone());
        Self { descriptor, transport, endpoint }
    }
}

#[async_trait]
impl Capability for RemoteCapability {
    fn descriptor(&self) -> CapabilityDescriptor {
        self.descriptor.clone()
    }

    async fn invoke(&self, input: Value, ctx: CapabilityContext) -> Result<Value> {
        self.transport
            .call(
                &self.endpoint,
                &self.descriptor.name,
                &self.descriptor.version,
                input,
                ctx.timeout_ms,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use agentos_core::SessionId;

    fn ctx(permission: CapabilityPermission, root: &std::path::Path) -> CapabilityContext {
        CapabilityContext {
            capability_id: CapabilityId::new(),
            caller: crate::capability::CallerContext::new(SessionId::new()),
            permission,
            workspace: Arc::new(Workspace::new(root).unwrap()),
            artifacts: None,
            timeout_ms: 1000,
        }
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agentos-cap-{}", agentos_core::now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn echo_returns_its_input() {
        let cap = EchoCapability::new();
        let out = cap
            .invoke(json!({"text":"hi"}), ctx(CapabilityPermission::pure(), &tmpdir()))
            .await
            .unwrap();
        assert_eq!(out["text"], "hi");
        assert_eq!(out["length"], 2);
    }

    async fn calc(expression: &str, root: &std::path::Path) -> Result<Value> {
        CalculatorCapability::new()
            .invoke(json!({ "expression": expression }), ctx(CapabilityPermission::pure(), root))
            .await
    }

    #[tokio::test]
    async fn calculator_handles_precedence_and_errors() {
        let root = tmpdir();
        assert_eq!(calc("2+3*4", &root).await.unwrap()["result"], 14.0);
        assert_eq!(calc("(2+3)*4", &root).await.unwrap()["result"], 20.0);
        assert_eq!(calc("2^10", &root).await.unwrap()["result"], 1024.0);
        assert_eq!(calc("-5 + 2", &root).await.unwrap()["result"], -3.0);
        assert_eq!(calc("7 % 4", &root).await.unwrap()["result"], 3.0);
        assert!(calc("1/0", &root).await.is_err());
        assert!(calc("2 +", &root).await.is_err());
        assert!(calc("alert(1)", &root).await.is_err());
    }

    #[tokio::test]
    async fn filesystem_read_stays_inside_the_workspace() {
        let root = tmpdir();
        std::fs::write(root.join("ok.txt"), "inside").unwrap();
        let cap = FilesystemReadCapability::new(1024);
        let good = cap
            .invoke(json!({"path":"ok.txt"}), ctx(CapabilityPermission::read_only_fs(), &root))
            .await
            .unwrap();
        assert_eq!(good["content"], "inside");
        let bad = cap
            .invoke(json!({"path":"../../etc/passwd"}), ctx(CapabilityPermission::read_only_fs(), &root))
            .await
            .unwrap_err();
        assert_eq!(bad.kind, agentos_core::ErrorKind::PolicyDenied);
    }

    #[tokio::test]
    async fn filesystem_write_requires_the_permission() {
        let root = tmpdir();
        let cap = FilesystemWriteCapability::new(1024);
        let denied = cap
            .invoke(json!({"path":"a.txt","content":"x"}), ctx(CapabilityPermission::read_only_fs(), &root))
            .await
            .unwrap_err();
        assert_eq!(denied.kind, agentos_core::ErrorKind::PolicyDenied);
        let allowed = cap
            .invoke(
                json!({"path":"a.txt","content":"x"}),
                ctx(CapabilityPermission::read_only_fs().with_fs_write(), &root),
            )
            .await
            .unwrap();
        assert_eq!(allowed["bytes"], 1);
    }

    #[test]
    fn descriptors_are_complete() {
        let d = EchoCapability::new().descriptor();
        assert_eq!(d.name, "echo");
        assert!(!d.version.is_empty());
        assert!(d.input_schema.is_object());
        assert!(d.output_schema.is_object());
        assert_eq!(d.permission, CapabilityPermission::pure());
    }
}
