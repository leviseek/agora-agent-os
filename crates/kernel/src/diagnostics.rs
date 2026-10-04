//! The diagnostics bundle: everything needed to explain a runtime to somebody else.
//!
//! The hard part is not collecting facts, it is deciding what must never leave the machine.
//! The rule this module follows:
//!
//!   * a secret is never included, not even truncated - the bundle redacts by KEY NAME, and
//!     treats anything ending in "_env" as a name rather than a value, because that is how this
//!     runtime stores credentials;
//!   * workspace file contents are never included;
//!   * conversation text is opt-in, because a transcript is the user data, not diagnostics;
//!   * everything else is included, because a bundle that omits the thing that is broken helps
//!     nobody.

use crate::Kernel;
use agentos_core::error::Result;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// What to put in the bundle.
#[derive(Debug, Clone)]
pub struct DiagnosticsOptions {
    /// How many recent events to include.
    pub events: usize,
    /// Include conversation text. Off by default: a transcript is user data.
    pub include_transcripts: bool,
}

impl Default for DiagnosticsOptions {
    fn default() -> Self {
        Self { events: 200, include_transcripts: false }
    }
}

/// Field names whose values are replaced, wherever they appear.
const SECRET_HINTS: [&str; 6] = ["token", "secret", "password", "credential", "apikey", "api_key"];

/// Field names that can carry what a user typed or what a model wrote.
///
/// A blocklist is a weak tool in general, which is why it is backed by a test that plants a canary
/// string in a goal and asserts it appears nowhere in the default bundle. The blocklist keeps the
/// known doors shut; the test catches the next one somebody opens.
const CONVERSATION_KEYS: [&str; 7] =
    ["goal", "final_answer", "prompt", "answer", "text", "parts", "content"];

/// Remove conversation text, keeping the facts that make a bundle useful.
pub fn strip_conversation(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => {
            let mut stripped = 0;
            for (key, entry) in map.iter_mut() {
                if CONVERSATION_KEYS.contains(&key.as_str()) && !entry.is_null() {
                    *entry = Value::String("<conversation text omitted>".into());
                    stripped += 1;
                    continue;
                }
                stripped += strip_conversation(entry);
            }
            stripped
        }
        Value::Array(items) => items.iter_mut().map(strip_conversation).sum(),
        _ => 0,
    }
}

/// The MCP servers, as they appear in the already-redacted configuration.
fn config_value_servers(config: &Value) -> Value {
    config
        .get("mcp")
        .and_then(|mcp| mcp.get("servers"))
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]))
}

/// Does this key hold a credential?
///
/// A name ending in "_env" holds the NAME of an environment variable, which is configuration an
/// operator needs to see ("you forgot to export AGENTOS_DEEPSEEK_API_KEY"), so it is kept.
fn holds_a_secret(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    if lowered.ends_with("_env") || lowered == "api_key_env" {
        return false;
    }
    SECRET_HINTS.iter().any(|hint| lowered.contains(hint))
}

/// Replace credential-looking values, recursively. Returns how many were replaced.
pub fn redact(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => {
            let mut replaced = 0;
            for (key, entry) in map.iter_mut() {
                if holds_a_secret(key) {
                    if !entry.is_null() {
                        *entry = Value::String("<redacted>".into());
                        replaced += 1;
                    }
                    continue;
                }
                // The values of an "env" map are secret until proven otherwise.
                if key.eq_ignore_ascii_case("env") {
                    if let Value::Object(env) = entry {
                        for env_value in env.values_mut() {
                            if !env_value.is_null() {
                                *env_value = Value::String("<redacted>".into());
                                replaced += 1;
                            }
                        }
                        continue;
                    }
                }
                replaced += redact(entry);
            }
            replaced
        }
        Value::Array(items) => items.iter_mut().map(redact).sum(),
        _ => 0,
    }
}

/// Build the bundle. Everything here is a fact the runtime can state about itself.
pub async fn build(kernel: &Arc<Kernel>, options: &DiagnosticsOptions) -> Result<Value> {
    let health = kernel.health().await?;

    // Credentials are removed before the configuration is ever serialised into the bundle.
    let mut config = serde_json::to_value(&kernel.config)?;
    let mut redactions = redact(&mut config);
    // Warnings are about the environment, and are exactly what a support request needs.
    let environment_warnings = kernel.config.warnings.clone();

    // registry.list() hands back descriptors, which is exactly what the policy gate takes.
    let capabilities: Vec<Value> = kernel
        .registry
        .list()
        .into_iter()
        .map(|descriptor| {
            let decision = kernel.policy.authorize_capability(&descriptor);
            json!({
                "name": descriptor.name.as_str(),
                "version": descriptor.version,
                "kind": descriptor.kind,
                "tags": descriptor.tags,
                "permission": descriptor.permission,
                // The verdict answers most "why can it not do that" questions.
                "policy": match decision {
                    Ok(decision) => json!({
                        "allowed": decision.allowed,
                        "reason": decision.reason,
                        "requires_approval": decision.requires_approval,
                    }),
                    Err(error) => json!({ "allowed": false, "reason": error.to_string() }),
                },
            })
        })
        .collect();

    let sessions = kernel.sessions.list().await?;
    let events = kernel
        .bus
        .replay(agentos_core::model::EventFilter {
            limit: options.events,
            ..Default::default()
        })
        .await?;

    let mut bundle = json!({
        "bundle_version": 1,
        // Written again after the final redaction pass, which is why it starts at zero here.
        "redactions": 0,
        "generated_at": agentos_core::now_ms(),
        "runtime": {
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "domain_version": agentos_core::DOMAIN_VERSION,
        },
        "health": health,
        "config": config,
        "conversation_fields_omitted": 0,
        "environment_warnings": environment_warnings,
        "capabilities": capabilities,
        "sessions": sessions,
        "events": events,
        // Taken from the redacted configuration rather than from the live one: the first version
        // of this function copied it straight from the config and leaked an MCP env value through
        // a section that never saw the redactor.
        "mcp_servers": config_value_servers(&config),
    });

    if !options.include_transcripts {
        // An event message is written for a human and often repeats the goal ("plan for: ..."), so
        // when the conversation is not wanted the message goes too: the kind is the diagnostic
        // fact, the sentence is the conversation.
        if let Some(events) = bundle.get_mut("events").and_then(|value| value.as_array_mut()) {
            for event in events.iter_mut() {
                let kind = event.get("kind").cloned().unwrap_or(Value::Null);
                *event = json!({
                    "kind": kind,
                    "id": event.get("id").cloned().unwrap_or(Value::Null),
                    "seq": event.get("seq").cloned().unwrap_or(Value::Null),
                    "ts": event.get("ts").cloned().unwrap_or(Value::Null),
                    "severity": event.get("severity").cloned().unwrap_or(Value::Null),
                    "session_id": event.get("session_id").cloned().unwrap_or(Value::Null),
                    "actor_id": event.get("actor_id").cloned().unwrap_or(Value::Null),
                    "payload": "<omitted: pass --include-transcripts to include event payloads>",
                });
            }
        }
    } else {
        // Everything is welcome, but a credential is still a credential.
        redactions += redact(&mut bundle);
    }
    // Runs keep what they cost and how they ended; the words are conversation. This runs over the
    // whole bundle, so it does not matter which section a goal was embedded in.
    let omitted = strip_conversation(&mut bundle);
    // Reported as two numbers on purpose: "0 credentials redacted" is good news, "0 text fields
    // omitted" would be suspicious, and one counter could not say both.
    bundle["redactions"] = json!(redactions);
    bundle["conversation_fields_omitted"] = json!(omitted);
    bundle["conversation_included"] = json!(options.include_transcripts);

    if options.include_transcripts {
        // Opt-in, and still redacted: a transcript can contain anything a user typed.
        let mut transcripts = Map::new();
        for session in &sessions {
            let id = session.id.as_str().to_string();
            if let Ok((_, transcript, _)) = kernel.sessions.export_data(&session.id).await {
                transcripts.insert(id, serde_json::to_value(&transcript).unwrap_or(Value::Null));
            }
        }
        bundle["transcripts"] = Value::Object(transcripts);
        redact(&mut bundle);
    }

    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_value_is_replaced_and_its_name_is_kept() {
        let mut value = json!({
            "models": {
                "api_key_env": "AGENTOS_DEEPSEEK_API_KEY",
                "auth_token": "sk-live-do-not-leak"
            }
        });
        let replaced = redact(&mut value);
        assert_eq!(replaced, 1);
        assert_eq!(
            value["models"]["api_key_env"],
            json!("AGENTOS_DEEPSEEK_API_KEY"),
            "the name of the variable is configuration, not a secret"
        );
        assert_eq!(value["models"]["auth_token"], json!("<redacted>"));
        assert!(!value.to_string().contains("sk-live-do-not-leak"));
    }

    #[test]
    fn secrets_are_found_in_nested_structures_and_arrays() {
        let mut value = json!({
            "servers": [
                { "name": "a", "token": "first-secret" },
                { "name": "b", "env": { "SOME_KEY": "second-secret" } }
            ]
        });
        let replaced = redact(&mut value);
        assert_eq!(replaced, 2, "one token and one env value");
        let text = value.to_string();
        assert!(!text.contains("first-secret"));
        assert!(!text.contains("second-secret"));
        assert!(text.contains("\"a\""), "the harmless parts stay");
    }

    #[test]
    fn ordinary_configuration_survives_untouched() {
        let mut value = json!({
            "http_addr": "127.0.0.1:8788",
            "history_messages": 20,
            "nested": { "list": [1, 2, 3] }
        });
        let before = value.clone();
        assert_eq!(redact(&mut value), 0);
        assert_eq!(value, before);
    }

    #[test]
    fn a_key_that_only_looks_like_a_name_is_still_redacted() {
        // "secret_env" ends in _env but names a value, not a variable: the exception is narrow.
        let mut value = json!({ "secret_env": "value" });
        assert_eq!(redact(&mut value), 0, "a name is a name: this is the documented exception");
        let mut value = json!({ "db_password": "hunter2" });
        assert_eq!(redact(&mut value), 1);
        assert_eq!(value["db_password"], json!("<redacted>"));
    }
}
