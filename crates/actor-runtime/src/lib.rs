//! agentos-actor-runtime - the concurrency and lifecycle substrate of the Agent OS.
//!
//! Design commitments:
//!
//! * An actor is a logical addressable unit with a typed mailbox. Messages inside one actor are
//!   processed strictly in order; different actors run concurrently. That single property is what
//!   gives us per-session ordering with inter-session parallelism.
//! * Actor state is DATA, never a process image. Every actor can produce and consume a JSON
//!   snapshot, which is the precondition for clone and migration.
//! * Migration is a pipeline, not a copy: Checkpoint -> Snapshot -> Transfer -> Restore ->
//!   Replay -> Resume. Transfer is an interface with a local implementation in v1.

pub mod actor;
pub mod checkpoint;
pub mod migration;
pub mod runtime;

pub use actor::{
    Actor, ActorCommand, ActorContext, ActorFactory, ActorHandle, ActorInit, ErasedActor, TypedActor,
};
pub use checkpoint::{CheckpointStore, StoreCheckpointStore};
pub use migration::{ActorTransfer, LocalTransfer, MigrationCoordinator, TransferReceipt, TransferTarget};
pub use runtime::{ActorRuntime, ActorRuntimeConfig};
