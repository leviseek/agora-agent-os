# Capabilities

A capability is a typed, versioned, permissioned unit of ability. It is **not** an agent (it has
no goal) and **not** a worker (it does not own resources). The same capability can run

* in-process (built-in, Rust),
* inside the Wasmtime sandbox (portable, cloneable, migratable),
* on another node behind gRPC (remote).

Callers cannot tell the difference: all three go through `CapabilityMesh::invoke`, which
resolves the version, asks the policy gate, validates the input against the JSON Schema, applies
the timeout, retries retryable failures and emits `tool_call` / `tool_result` events.

## Built-ins (crates/capability-runtime/src/builtins.rs)

| name | version | permission | notes |
|---|---|---|---|
| `echo` | 1.0.0 | none | control capability for connectivity and policy tests |
| `calculator` | 1.0.0 | none | recursive-descent arithmetic parser; no eval crate, no code execution |
| `clock` | 1.0.0 | none | runtime clock |
| `filesystem-list` | 1.0.0 | fs_read | workspace-jailed |
| `filesystem-read` | 1.0.0 | fs_read | workspace-jailed, size-capped |
| `filesystem-write` | 1.0.0 | fs_read + fs_write | denied unless the capability is on the policy allow list |

Every descriptor carries `name`, `version`, `input_schema`, `output_schema`, `permission`,
`provider`, `timeout_ms` and `idempotent`. Output is validated against the output schema
too, so a misbehaving implementation cannot poison the agent loop.

## Wasm capabilities

`capabilities/wasm/` contains two hand-written modules:

| file | purpose |
|---|---|
| `echo.wat` | smallest useful guest: JSON in, same JSON out, calls `host_log` once |
| `spin.wat` | loops forever on purpose; proves the epoch watchdog stops runaway guests |

### ABI (agentos.wasm.v1)

Exports:

| export | signature | meaning |
|---|---|---|
| `memory` | - | exported linear memory |
| `alloc` | `(i32) -> i32` | reserve `len` bytes, return the offset |
| `invoke` | `(i32, i32) -> i64` | handle the UTF-8 JSON request, return `(ptr << 32) | len` of the JSON result |

Imports (module `agentos`), all gated by `WasmHostPolicy`:

| import | signature | gate |
|---|---|---|
| `host_log` | `(i32 level, i32 ptr, i32 len) -> i32` | `allow_log` |
| `host_emit` | `(i32 ptr, i32 len) -> i32` | `allow_emit` (denied by default) |
| `host_now` | `() -> i64` | `allow_clock` |

A denied import returns `-1`; the guest is expected to check. The host never traps on a
permission violation, so a guest can degrade instead of dying.

### Registering a wasm capability

```rust
let spec = WasmCapabilitySpec {
    name: "wasm-echo".into(),
    version: "1.0.0".into(),
    input_schema: json!({ "type": "object", "required": ["text"] }),
    output_schema: json!({ "type": "object", "required": ["text"] }),
    host_policy: WasmHostPolicy { allow_log: true, allow_emit: false, allow_clock: true },
    timeout_ms: 2_000,
    ..WasmCapabilitySpec::new("wasm-echo", "1.0.0")
};
kernel.register_wasm_capability(spec, include_bytes!("echo.wasm"))?;
```

Compilation happens at registration time, instantiation per call. Limits in force: memory cap,
instance cap, wall-clock timeout via epoch interruption, and (reserved) fuel metering.

## Adding your own capability

1. Implement `agentos_capability_runtime::Capability` (one `descriptor()`, one `invoke()`).
2. Register it: `kernel.registry.register(Arc::new(MyCapability))`.
3. If it needs host access, declare it in `CapabilityPermission` and add the capability name to
   `policy.allowed_capabilities` when the permission is mutating (fs_write, network, exec) -
   the default policy denies those.
