//! agentos-control-plane - the slow, authoritative plane.
//!
//! Members: Placement Service, Actor Directory, Worker Registry and the Policy engine.
//!
//! Cardinal rule: the control plane is NOT on the per-message path. The gateway and the router
//! keep a local cache of "session -> actor -> worker"; they only call in here on a cache miss, on
//! migration or during failure recovery. That is why every lookup here reports whether it was
//! answered from cache, and why the cache-hit ratio is a first-class metric.

pub mod directory;
pub mod placement;
pub mod policy;

pub use directory::{ActorDirectory, DirectoryEntry};
pub use placement::{
    MigrationPlan, PlacementDecision, PlacementPolicy, PlacementService, PlacementStrategy,
    WorkerRegistry,
};
pub use policy::PolicyEngine;
