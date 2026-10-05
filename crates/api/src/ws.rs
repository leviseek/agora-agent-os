//! WebSocket fan-out: live events out, commands in.
//!
//! One socket carries both directions, which is what the desktop and web clients need:
//!   server -> client: {"type":"event", ...EventRecord}
//!                     {"type":"hello", ...meta}
//!                     {"type":"error", ...}
//!   client -> server: {"type":"goal","session_id":..,"text":..,"wait":false}
//!                     {"type":"cancel","session_id":..}
//!                     {"type":"subscribe","kinds":[..],"session_id":..}
//!                     {"type":"ping"}

use crate::ApiState;
use agentos_core::model::{EventFilter, EventKind};
use agentos_core::SessionId;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};

pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<ApiState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle(socket, state))
}

async fn handle(socket: WebSocket, state: ApiState) {
    let (mut sink, mut stream) = socket.split();
    let mut subscription = state.kernel.bus.subscribe(EventFilter { limit: 0, ..Default::default() });

    let hello = json!({
        "type": "hello",
        "node": state.config.node.name,
        "domain_version": agentos_core::DOMAIN_VERSION,
        "auth_required": state.config.api.auth_required(),
        "capabilities": state.kernel.registry.len(),
        "ts": agentos_core::now_ms(),
    });
    if sink.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            biased;
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(reply) = command(&state, &text).await {
                            if sink.send(Message::Text(reply.to_string().into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::debug!(error = %e, "websocket receive error");
                        break;
                    }
                }
            }
            event = subscription.recv() => {
                match event {
                    Ok(record) => {
                        let payload = json!({ "type": "event", "event": record });
                        if sink.send(Message::Text(payload.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let payload = json!({ "type": "error", "code": e.code(), "message": e.message });
                        let _ = sink.send(Message::Text(payload.to_string().into())).await;
                        // A lagged subscriber simply keeps going: dropping frames beats dropping the
                        // session, and the client can re-sync from /v1/events.
                    }
                }
            }
        }
    }
    tracing::debug!("websocket closed");
}

async fn command(state: &ApiState, text: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(text).ok()?;
    let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
    match kind {
        "ping" => Some(json!({ "type": "pong", "ts": agentos_core::now_ms() })),
        "goal" => {
            let session = value.get("session_id").and_then(|v| v.as_str())?;
            let text = value.get("goal").or_else(|| value.get("text")).and_then(|v| v.as_str())?;
            let wait = value.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);
            // Model and effort travel the same way over the socket as over HTTP: optional, and
            // for this one goal only.
            let model = value
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let effort = match value.get("effort").and_then(|v| v.as_str()) {
                None => None,
                Some(raw) if raw.trim().is_empty() => None,
                Some(raw) => match agentos_core::model::ReasoningEffort::parse(raw) {
                    Some(parsed) => Some(parsed),
                    None => {
                        return Some(json!({
                            "type": "error",
                            "code": "invalid_input",
                            "message": format!("unknown effort {raw:?}: use off, low, medium or high"),
                        }))
                    }
                },
            };
            // Images travel the same way over the socket as over HTTP: workspace paths and
            // uploaded attachment ids, both accepted on one goal.
            let strings = |key: &str| -> Vec<String> {
                value
                    .get(key)
                    .and_then(|v| v.as_array())
                    .map(|list| {
                        list.iter()
                            .filter_map(|item| item.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let images: Vec<String> = strings("images");
            let attachments: Vec<String> = strings("attachments");
            let session_id = match SessionId::parse(session) {
                Ok(id) => id,
                Err(e) => return Some(json!({ "type": "error", "code": "invalid_input", "message": e.to_string() })),
            };
            if wait {
                match state
                    .kernel
                    .sessions
                    .post_goal(&session_id, text, &images, &attachments, model, effort)
                    .await
                {
                    Ok(result) => Some(json!({ "type": "goal_result", "result": result })),
                    Err(e) => Some(json!({ "type": "error", "code": e.code(), "message": e.message })),
                }
            } else {
                match state
                    .kernel
                    .sessions
                    .post_goal_async(&session_id, text, &images, &attachments, model, effort)
                    .await
                {
                    Ok(()) => Some(json!({ "type": "accepted", "session_id": session })),
                    Err(e) => Some(json!({ "type": "error", "code": e.code(), "message": e.message })),
                }
            }
        }
        "cancel" => {
            let session = value.get("session_id").and_then(|v| v.as_str())?;
            let session_id = SessionId::parse(session).ok()?;
            match state.kernel.sessions.cancel(&session_id).await {
                Ok(cancelled) => Some(json!({ "type": "cancelled", "cancelled": cancelled })),
                Err(e) => Some(json!({ "type": "error", "code": e.code(), "message": e.message })),
            }
        }
        "snapshot" => {
            let session = value.get("session_id").and_then(|v| v.as_str())?;
            let session_id = SessionId::parse(session).ok()?;
            match state.kernel.sessions.snapshot(&session_id).await {
                Ok(checkpoint) => Some(json!({ "type": "snapshot", "checkpoint": checkpoint })),
                Err(e) => Some(json!({ "type": "error", "code": e.code(), "message": e.message })),
            }
        }
        "health" => match state.kernel.health().await {
            Ok(health) => Some(json!({ "type": "health", "health": health })),
            Err(e) => Some(json!({ "type": "error", "code": e.code(), "message": e.message })),
        },
        _ => Some(json!({
            "type": "error",
            "code": "invalid_input",
            "message": format!("unknown command type: {kind}"),
            "supported": ["ping", "goal", "cancel", "snapshot", "health"]
        })),
    }
}

/// Kinds the client may filter on, exposed so the UI can build its filter chips from the runtime.
pub fn subscribable_kinds() -> Vec<&'static str> {
    [
        EventKind::SessionCreated,
        EventKind::SessionMessageQueued,
        EventKind::RunCreated,
        EventKind::AgentStep,
        EventKind::ToolCall,
        EventKind::ToolResult,
        EventKind::TaskStarted,
        EventKind::TaskCompleted,
        EventKind::TaskFailed,
        EventKind::ActorSpawned,
        EventKind::ActorMigrated,
        EventKind::SnapshotCreated,
        EventKind::WorkerHeartbeat,
        EventKind::PolicyDenied,
    ]
    .iter()
    .map(|k| k.as_str())
    .collect()
}
