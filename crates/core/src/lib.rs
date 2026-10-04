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
pub use time::{now_ms, Timestamp};

/// Version of the Agent OS domain contract. Bump on breaking domain changes.
pub const DOMAIN_VERSION: &str = "0.1.0";
