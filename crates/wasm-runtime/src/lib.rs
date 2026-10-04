//! agentos-wasm-runtime - Wasmtime sandbox for portable capabilities.
//!
//! Why: a capability that ships as a wasm module can be cloned, versioned and moved between
//! nodes without trusting it with the host. The sandbox gives us:
//!   * memory and instance limits,
//!   * wall-clock timeouts via epoch interruption (no runaway guest can wedge a session),
//!   * an explicit host-function surface, each function gated by a permission flag,
//!   * a JSON in / JSON out ABI so guests need no knowledge of the runtime.
//!
//! The sandbox is deliberately NOT on the message hot path: modules are compiled once and
//! instantiated per call, and the compile step happens at registration time.

pub mod abi;
pub mod engine;
pub mod capability;

pub use abi::{WasmHostPolicy, WasmResult, ABI_VERSION};
pub use capability::WasmCapability;
pub use engine::{WasmEngine, SandboxConfig, WasmModule};
