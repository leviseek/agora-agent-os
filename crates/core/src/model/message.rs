use crate::ids::{MessageId, SessionId};
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        }
    }
}

/// Multi-part content so that artifacts and tool results can travel with a message without
/// inventing a second message type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    Artifact { artifact_id: String, name: String },
    Json { value: serde_json::Value },
    /// An image the user attached. The bytes live in the artifact store, not here: a transcript
    /// full of base64 would be unreadable to a human and expensive to store, and the artifact
    /// store already exists to hold binary payloads by content hash.
    Image { artifact_id: String, name: String, mime: String },
}

/// A single turn in a session transcript. User input is untrusted: it is stored as data and
/// never interpreted as instructions by the runtime itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    pub id: MessageId,
    pub session_id: SessionId,
    pub role: MessageRole,
    pub parts: Vec<ContentPart>,
    pub created_at: Timestamp,
    pub correlation_id: Option<String>,
    pub agent_id: Option<String>,
    /// Who said it. A conversation can have several participants once a session is shared, and a
    /// transcript that cannot tell them apart is a transcript that attributes one person's words to
    /// another. Older records have no author; their turns read as the session's own.
    #[serde(default)]
    pub author: Option<crate::model::PrincipalRef>,
}

impl SessionMessage {
    pub fn text(session_id: SessionId, role: MessageRole, text: impl Into<String>) -> Self {
        Self {
            id: MessageId::new(),
            session_id,
            role,
            parts: vec![ContentPart::Text { text: text.into() }],
            created_at: crate::now_ms(),
            correlation_id: None,
            agent_id: None,
            author: None,
        }
    }

    pub fn user(session_id: SessionId, text: impl Into<String>) -> Self {
        Self::text(session_id, MessageRole::User, text)
    }

    pub fn assistant(session_id: SessionId, text: impl Into<String>) -> Self {
        Self::text(session_id, MessageRole::Assistant, text)
    }

    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                ContentPart::Text { text } => out.push_str(text),
                ContentPart::Artifact { name, .. } => out.push_str(&format!("[artifact {name}]")),
                ContentPart::Json { value } => out.push_str(&value.to_string()),
                // A placeholder, not the bytes: this is what an older turn looks like once its
                // image is no longer sent to the model.
                ContentPart::Image { name, .. } => out.push_str(&format!("[image {name}]")),
            }
        }
        out
    }
}
