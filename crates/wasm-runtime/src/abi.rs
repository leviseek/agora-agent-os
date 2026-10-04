//! The guest ABI.
//!
//! Exports a guest module must provide:
//!   memory                      - exported linear memory
//!   alloc(len: i32) -> i32      - reserve len bytes, return the offset
//!   invoke(ptr: i32, len: i32) -> i64
//!                               - handle a UTF-8 JSON request at ptr/len and return
//!                                 (result_ptr << 32) | result_len, pointing at a UTF-8 JSON result
//!
//! Imports the host provides (all under the "agentos" module):
//!   host_log(level: i32, ptr: i32, len: i32) -> i32     - structured logging, always allowed
//!   host_emit(ptr: i32, len: i32) -> i32                - publish a runtime event, gated
//!   host_now() -> i64                                   - epoch millis, always allowed
//!
//! A negative return value means "denied or failed"; guests are expected to check.

pub const ABI_VERSION: &str = "agentos.wasm.v1";

/// What a module is allowed to do. Deny by default.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WasmHostPolicy {
    pub allow_log: bool,
    pub allow_emit: bool,
    pub allow_clock: bool,
}

impl WasmHostPolicy {
    /// The minimum surface a well behaved capability needs.
    pub fn minimal() -> Self {
        Self { allow_log: true, allow_emit: false, allow_clock: true }
    }

    pub fn summary(&self) -> String {
        let mut granted = Vec::new();
        if self.allow_log { granted.push("log"); }
        if self.allow_emit { granted.push("emit"); }
        if self.allow_clock { granted.push("clock"); }
        if granted.is_empty() { "none".to_string() } else { granted.join("+") }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WasmResult {
    pub value: serde_json::Value,
    pub fuel_used: Option<u64>,
    pub duration_ms: u64,
    pub host_calls: u64,
}

/// Host call status codes returned to the guest.
pub mod status {
    pub const OK: i32 = 0;
    pub const DENIED: i32 = -1;
    pub const INVALID: i32 = -2;
}
