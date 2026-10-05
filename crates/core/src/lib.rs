//! agentos-core - the shared kernel of the Agent Operating System.
//!
//! This crate deliberately depends on almost nothing. It defines the vocabulary every other
//! plane speaks:
//!
//! * typed identifiers (module ids) with a distinct lifetime per domain entity,
//! * explicit state machines (module state), so no module smuggles lifecycle into booleans,
//! * one error taxonomy (module error) so retry / surface decisions happen in exactly one place,
//! * configuration plus secret resolution (module config),
//! * observability primitives: tracing bootstrap, correlation context, metrics (module telemetry).
//!
//! Boundaries: nothing here knows about storage, models, networks or actors.

pub mod config;
pub mod error;
pub mod ids;
pub mod model;
pub mod state;
pub mod telemetry;
pub mod time;

pub use error::{ErrorKind, Result, RuntimeError};
pub use ids::{
    ActorId, AgentId, ArtifactId, CapabilityId, CheckpointId, CorrelationId, EventId, MemoryId,
    MessageId, ModelId, NodeId, RequestId, SessionId, TaskId, TraceId, WorkerId,
};
pub use config::vision_of_model;
pub use time::{now_ms, Timestamp};

/// Version of the Agent OS domain contract. Bump on breaking domain changes.
pub const DOMAIN_VERSION: &str = "0.1.0";

/// Behaviours this build of the runtime has, so a client can tell whether the thing it is talking
/// to is new enough for it. Add a name here when a behaviour lands; never remove one, because an
/// older runtime is exactly what a caller needs to recognise.
pub const FEATURES: &[&str] = &[
    "session.durable-recovery",
    "session.rebuild-without-directory",
    "session.closed-is-readable",
    "events.delta-streaming",
    "events.unique-sequence",
    "session.model-choice",
    "session.reasoning-effort",
    "models.placeholder-is-fallback",
    "conversation.placeholder-filtered",
    "approvals.list",
    "diagnostics.bundle",
    "model.vision-declared",
    "vision.capability-gated",
    "attachments.upload",
    "attachments.text-documents",
    "observations.without-tool-protocol",
];

/// The sentence the built-in placeholder provider answers with.
///
/// It lives here because two crates need to agree on it: the placeholder writes it, and the
/// conversation filter recognises it. A real model that reads this sentence in its history starts
/// reproducing it - so faithfully that the copy is indistinguishable from the placeholder's own
/// answer, which is why the filter cannot rely on who answered a turn.
pub const PLACEHOLDER_ANSWER_MARKER: &str = "No capability was required, so this is the final answer";
