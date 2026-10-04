//! agentos-task-scheduler - task graphs, dependency-aware parallel execution and retries.
//!
//! A task graph is data: nodes, dependencies, payloads, retry policy. The scheduler is the only
//! component that decides what runs when, and it never blocks the runtime: every node executes
//! on the shared runtime with its own timeout and cancellation.

pub mod graph;
pub mod scheduler;

pub use graph::TaskGraphBuilder;
pub use scheduler::{GraphOutcome, Scheduler, SchedulerConfig, TaskContext, TaskRunner};
