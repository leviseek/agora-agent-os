//! Session export: the conversation and the runs, as data or as a document.
//!
//! Export is a read-only projection of state that already exists, so it lives beside the session
//! rather than inside the actor: nothing here can change a session, and the rendering is a pure
//! function that a test can pin down exactly.

use agentos_core::model::{AgentRun, SessionRecord, SessionMessage};

/// Render a session as Markdown: front matter, then the conversation, then what each run cost.
pub fn to_markdown(session: &SessionRecord, transcript: &[SessionMessage], runs: &[AgentRun]) -> String {
    let mut out = String::new();
    out.push_str("# ");
    out.push_str(session.title.trim());
    out.push_str("\n\n");
    out.push_str(&format!("- session: `{}`\n", session.id));
    out.push_str(&format!("- user: `{}`\n", session.user_id));
    out.push_str(&format!("- state: `{}`\n", session.state.as_str()));
    out.push_str(&format!("- created: {}\n", session.created_at));
    let total: u64 = runs.iter().map(|run| run.usage.total_tokens).sum();
    out.push_str(&format!("- turns: {} · runs: {} · tokens: {}\n", transcript.len(), runs.len(), total));

    out.push_str("\n## Conversation\n");
    for message in transcript {
        let text: String = message
            .parts
            .iter()
            .filter_map(|part| match part {
                agentos_core::model::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("\n**{}**\n\n{}\n", message.role.as_str(), text.trim()));
    }

    if !runs.is_empty() {
        out.push_str("\n## Runs\n");
        for run in runs {
            out.push_str(&format!(
                "\n- `{}` {} · {} step(s) · {} tokens over {} call(s){}",
                run.id,
                run.state.as_str(),
                run.steps.len(),
                run.usage.total_tokens,
                run.usage.calls,
                run.provider
                    .as_ref()
                    .map(|provider| format!(" · provider: {provider}"))
                    .unwrap_or_default(),
            ));
            out.push_str(&format!("\n  - goal: {}", run.goal.trim()));
            if let Some(error) = &run.error {
                out.push_str(&format!("\n  - error: {error}"));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::SessionId;

    #[test]
    fn markdown_carries_the_conversation_and_the_cost() {
        let session = SessionRecord::new("u1", "Export me");
        let id = session.id.clone();
        let transcript = vec![
            SessionMessage::user(id.clone(), "what is 6*7?"),
            SessionMessage::assistant(id.clone(), "42"),
        ];
        let mut run = AgentRun::new(id, &Default::default(), "what is 6*7?");
        run.provider = Some("mock".into());
        run.usage.record(10, 5, 15);

        let markdown = to_markdown(&session, &transcript, &[run]);
        assert!(markdown.starts_with("# Export me"));
        assert!(markdown.contains("**user**"));
        assert!(markdown.contains("what is 6*7?"));
        assert!(markdown.contains("**assistant**"));
        assert!(markdown.contains("tokens: 15"), "the cost is part of the export");
        assert!(markdown.contains("provider: mock"));
    }

    #[test]
    fn an_empty_session_still_exports() {
        let session = SessionRecord::new("u1", "Nothing yet");
        let markdown = to_markdown(&session, &[], &[]);
        assert!(markdown.contains("## Conversation"));
        assert!(!markdown.contains("## Runs"), "no runs, no section");
    }
}