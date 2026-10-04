//! agentos-network - RPC and peer-to-peer transports.
//!
//! Two independent concerns live here, on purpose kept apart:
//!
//! * RPC (gRPC + Protobuf): the remote half of the Capability and Control planes. A remote
//!   capability is called through the same Capability trait, so the mesh cannot tell the
//!   difference between local and remote.
//! * P2P (libp2p, feature gated): node discovery, basic connectivity and a control-message
//!   abstraction. Deliberately NOT on the hot path: no business message travels over it in v1.

pub mod discovery;

#[cfg(feature = "grpc")]
pub mod grpc;

#[cfg(feature = "p2p")]
pub mod p2p;

#[cfg(feature = "grpc")]
pub mod transport;

/// Generated protobuf types and service stubs (package agentos.v1).
#[cfg(feature = "grpc")]
pub mod rpc {
    tonic::include_proto!("agentos.v1");
}

#[cfg(feature = "grpc")]
pub use grpc::{
    AgentHandler, CapabilityHandler, ControlHandler, GrpcAgentClient, GrpcControlClient, GrpcServer,
    RemoteInvocation, RemoteInvocationResult,
};

#[cfg(feature = "grpc")]
pub use transport::GrpcCapabilityTransport;

pub use discovery::{DiscoveryEvent, NodeDiscovery, NodeInfo, StubDiscovery};
