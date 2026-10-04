//! agentos-capability-runtime - the capability mesh.
//!
//! A capability is a typed, versioned, permissioned unit of ability. It is NOT a worker and NOT
//! an agent: the same capability may run in-process, inside the Wasm sandbox, or on another
//! node behind an RPC hop, and callers must not be able to tell the difference.
//!
//! Every invocation goes through the policy gate and the schema check before it reaches the
//! implementation, because user input is untrusted by default.

pub mod builtins;
pub mod capability;
pub mod mesh;
pub mod policy;
pub mod registry;
pub mod schema;
pub mod transport;
pub mod workspace;

pub use capability::{CallerContext, Capability, CapabilityContext, InvocationResult};
pub use mesh::{CapabilityMesh, MeshConfig};
pub use policy::{AllowAllPolicy, CapabilityPolicy, PolicyDecision, PolicyRequest};
pub use registry::{CapabilityRegistry, DiscoveryQuery, RegisteredCapability};
pub use transport::{CapabilityTransport, LocalTransport};
pub use workspace::Workspace;
