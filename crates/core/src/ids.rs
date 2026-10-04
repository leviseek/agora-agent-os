//! Typed identifiers.
//!
//! Every entity in the runtime owns its own id type. They are shape-compatible (prefix plus
//! uuid-v7 body) but NOT interchangeable, which the compiler enforces for us. Time-ordered
//! uuid v7 gives cheap, globally unique, sortable keys without a central sequence.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Parse failure for a typed id.
#[derive(Debug, thiserror::Error)]
#[error("invalid {expected} id: {value:?}")]
pub struct IdError {
    pub expected: &'static str,
    pub value: String,
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Mint a fresh, time-ordered id.
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::now_v7().simple()))
            }

            /// Adopt an already-formatted id (read back from a store, or received over the wire).
            pub fn from_raw(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn prefix(&self) -> &'static str {
                $prefix
            }

            /// Strict parse: the id must carry the expected prefix and a uuid-v7 body.
            pub fn parse(raw: &str) -> Result<Self, IdError> {
                let expected = format!("{}_", $prefix);
                let body = raw.strip_prefix(&expected).ok_or_else(|| IdError {
                    expected: $prefix,
                    value: raw.to_string(),
                })?;
                Uuid::parse_str(body).map_err(|_| IdError {
                    expected: $prefix,
                    value: raw.to_string(),
                })?;
                Ok(Self(raw.to_string()))
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.0
            }
        }
    };
}

define_id!(
    /// A logical, addressable concurrency unit. One user session equals one Session Actor.
    SessionId,
    "ses"
);
define_id!(
    /// The unit of isolation and placement. An actor owns state and a serialized mailbox.
    ActorId,
    "act"
);
define_id!(
    /// An agent definition instance. Agent is not Model, Agent is not Process.
    AgentId,
    "agt"
);
define_id!(
    /// A node inside a task graph.
    TaskId,
    "tsk"
);
define_id!(
    /// A typed, versioned, permissioned unit of ability. Capability is not Worker.
    CapabilityId,
    "cap"
);
define_id!(
    /// A process/runtime that can host actors and capabilities.
    WorkerId,
    "wkr"
);
define_id!(
    /// An immutable output blob produced by an agent or capability.
    ArtifactId,
    "art"
);
define_id!(
    /// A durable, retrievable memory record.
    MemoryId,
    "mem"
);
define_id!(
    /// An append-only observation emitted on the event bus.
    EventId,
    "evt"
);
define_id!(
    /// A snapshot of actor state used for migration and restore.
    CheckpointId,
    "ckp"
);
define_id!(
    /// A model route resolved by the Model Router.
    ModelId,
    "mdl"
);
define_id!(
    /// A runtime node participating in the mesh.
    NodeId,
    "nod"
);
define_id!(
    /// Inbound request id, injected at the edge and propagated everywhere.
    RequestId,
    "req"
);
define_id!(
    /// Distributed trace id shared by every hop of one logical operation.
    TraceId,
    "trc"
);
define_id!(
    /// Correlation id stitching agent loop steps together.
    CorrelationId,
    "cor"
);
define_id!(
    /// A single turn in a session transcript.
    MessageId,
    "msg"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_parseable() {
        let s = SessionId::new();
        assert!(s.as_str().starts_with("ses_"));
        assert_eq!(SessionId::parse(s.as_str()).unwrap(), s);
    }

    #[test]
    fn wrong_prefix_is_rejected() {
        let a = ActorId::new();
        assert!(SessionId::parse(a.as_str()).is_err());
    }

    #[test]
    fn wrong_body_is_rejected() {
        assert!(TaskId::parse("tsk_not-a-uuid").is_err());
    }

    #[test]
    fn ids_are_time_ordered() {
        let a = EventId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = EventId::new();
        assert!(a.as_str() < b.as_str(), "uuid v7 must sort by creation time");
    }
}
