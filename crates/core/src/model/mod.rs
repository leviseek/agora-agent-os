//! Shared domain model: the nouns of the Agent OS.
//!
//! These types are pure data (serde-friendly, no behaviour that needs a backend). Every plane
//! persists, streams or routes them, so they must stay free of storage/model/network concerns.
//! Behaviour lives in the owning crate; this module only owns shape and invariants that are
//! local to a single record.

pub mod actor;
pub mod agent;
pub mod artifact;
pub mod capability;
pub mod event;
pub mod memory;
pub mod message;
pub mod session;
pub mod task;
pub mod worker;

pub use actor::{
    ActorRecord, ActorSnapshotMeta, Checkpoint, CheckpointMeta, CloneRequest, MigrationReport,
};
pub use agent::{
    ActionCall, AgentRun, AgentSpec, AgentStep, AttachmentRef, Observation, Plan, PlanStep,
    PlanStepKind, ReasoningEffort, StepKind, TokenUsage,
};
pub use artifact::{ArtifactKind, ArtifactRecord};
pub use capability::{
    CapabilityDescriptor, CapabilityKind, CapabilityLoad, CapabilityPermission, CapabilityProvider, VersionReq,
};
pub use event::{EventFilter, EventKind, EventRecord, EventSeverity, NewEvent};
pub use memory::{MemoryKind, MemoryQuery, MemoryRecord};
pub use message::{ContentPart, MessageRole, SessionMessage};
pub use session::SessionRecord;
pub use task::{TaskGraphRecord, TaskKind, TaskNode, TaskPayload, TaskRecord};
pub use worker::{WorkerCapacity, WorkerLoad, WorkerRecord};
