//! agentos-agent-runtime - sessions, the agent loop, memory and artifacts.
//!
//! Layering inside this crate:
//!   SessionManager  - control-plane-facing: create/close sessions, route messages to actors.
//!   SessionActor    - one actor per session; messages inside a session are strictly ordered.
//!   AgentLoop       - Goal -> Plan -> Act (task graph) -> Observe -> Finalize.
//!   MemoryStore     - retrieval contract, local implementation.
//!
//! The agent loop never talks to a provider, a capability or a store directly: it composes the
//! Model Router, the Capability Mesh and the Task Scheduler. Replacing any of them changes
//! nothing here.

pub mod agent_loop;
pub mod compaction;
pub mod context;
pub mod export;
pub mod memory;
pub mod session;
pub mod session_manager;

pub use agent_loop::{AgentLoop, AgentLoopOutcome, PlanTaskRunner};
pub use compaction::compaction_window;
pub use context::{load_workspace_context, LoadedContext};
pub use export::to_markdown;
pub use memory::{MemoryStore, StoreMemoryStore};
pub use session::{SessionActor, SessionActorState, SessionDeps, SessionMessage};
pub use session_manager::{SessionManager, SessionSummary};
