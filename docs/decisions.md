# Engineering decisions

Every decision below was taken to keep the first version **simple, runnable and replaceable**.
Each records the alternative that was rejected and why.

## D1. A dedicated `crates/kernel` as the composition root

**Decision.** The wiring of concrete implementations lives in one crate (`agentos-kernel`) that
depends on every plane. `crates/api` is a pure gateway, `crates/cli` and `crates/server` are entry
points.
**Rejected.** Putting the wiring into `crates/api` (would make the gateway depend on every plane
*and* be depended on by them) or into `main.rs` (would make it untestable).
**Consequence.** No plane can reach a concrete backend: the only place that knows `FileStore`
exists is the kernel.

## D2. Actor state is JSON, not a process image

**Decision.** `ErasedActor::snapshot` returns `serde_json::Value`; migration is
checkpoint/restore/replay.
**Rejected.** Copying process memory or serializing a stack. Not portable, not versionable, not
cloneable.
**Consequence.** Checkpoints are readable, diffable and hashable. The cost is serialization
overhead, which v1 accepts.

## D3. One mailbox per actor, one actor per session

**Decision.** Ordering inside a session is guaranteed by a single `mpsc` queue per actor; different
sessions are different actors on different tasks.
**Rejected.** A global work queue with per-session sequence numbers: more machinery for the same
guarantee, and it puts a shared lock on the message path.
**Consequence.** A long-running goal occupies its session (by design); cancellation therefore goes
through a `CancellationToken` registry *outside* the mailbox.

## D4. Control plane off the hot path

**Decision.** `SessionManager` consults the in-process actor runtime first and the Actor Directory
only on a miss or after a failure. Both outcomes are counted as metrics.
**Rejected.** Looking up the directory on every message: it would make every message a
control-plane round trip and couple throughput to the directory.
**Consequence.** The directory may be slightly stale; the runtime treats a stale entry as a
recovery trigger rather than an error.

## D5. Protobuf carries JSON payloads

**Decision.** `proto/agentos/v1/*.proto` transports domain objects as opaque JSON strings; only
routing-relevant fields are structured.
**Rejected.** Mirroring the full domain model in Protobuf: every domain change would be a wire
change, and the runtime would carry two definitions of the same type.
**Consequence.** gRPC stays stable while the domain evolves; the cost is that field-level schema
validation does not happen at the wire boundary (it happens in the mesh, against the capability
schemas).

## D6. Wasm capabilities use a JSON-in/JSON-out ABI

**Decision.** Guests export `alloc` and `invoke`, exchange UTF-8 JSON, and may call a handful of
permission-gated host functions.
**Rejected.** WASI preview 1 with a filesystem preopen: it would hand guests a real filesystem
surface before the runtime has a sandbox policy for it, and it pulls in a large dependency.
**Consequence.** The example `echo.wat` is ~40 lines and the sandbox surface is exactly what the
policy grants. WASI can be added later behind the same `Capability` trait.

## D7. Timeouts use epoch interruption, not threads

**Decision.** `Config::epoch_interruption(true)` plus a watchdog task that ticks the engine; each
store gets a deadline computed from its timeout.
**Rejected.** Running each guest on a thread that gets killed: cannot interrupt wasm safely, and
threads are not free.
**Consequence.** Guests are interrupted at the next epoch check (bounded by `epoch_tick_ms`, 50ms
by default), and the runtime never leaks a spinning guest.

## D8. A mock model provider ships in the runtime

**Decision.** `MockProvider` is always registered. The router prefers configured providers and
falls back to the mock.
**Rejected.** Requiring an API key to run anything: it makes tests network-dependent and the
desktop demo unusable offline.
**Consequence.** The whole acceptance suite runs with no network. The mock implements the same
contract including tool calls, so it exercises the real code path.

## D9. Retry decisions live in the error taxonomy

**Decision.** `ErrorKind::retryable()` decides retries; individual errors may override.
**Rejected.** Per-call-site retry booleans: guaranteed drift.
**Consequence.** A capability can mark one failure as `retryable(false)` and the mesh, the
scheduler and the router all honour it without further code.

## D10. Task graphs are data, the scheduler is the only decision maker

**Decision.** `TaskGraphBuilder` validates the DAG (dangling deps, cycles) before anything runs;
`Scheduler` owns concurrency, retry and cancellation.
**Rejected.** Executing the plan directly inside the agent loop: it would duplicate scheduling logic
and make the loop untestable in isolation.
**Consequence.** The same scheduler serves the agent loop, the CLI and any future workflow engine.

## D11. Memory retrieval is a trait from day one

**Decision.** `MemoryStore` with a deterministic local implementation (tag/importance/recency).
**Rejected.** Building a vector index into the runtime before a provider exists.
**Consequence.** `MemoryRecord.embedding` is reserved; a vector store drops in behind the trait
without touching the agent loop.

## D12. The desktop shell reuses the web client

**Decision.** Tauri 2 loads the same `apps/web` build; the Rust side only locates the runtime,
probes health and optionally starts it.
**Rejected.** A separate desktop UI: two UIs to keep in sync, and the desktop would drift from the
browser.
**Consequence.** One UI, two shells; the desktop can point at a remote runtime.

## D13. File store as the default backend

**Decision.** `StoreBackend::File` - one JSON envelope per key, JSONL event logs, atomic renames.
**Rejected.** SQLite: it needs a C toolchain, which is not guaranteed on a fresh Windows machine.
`redb` is available behind a feature for embedded-KV users.
**Consequence.** Default installs need nothing but Rust; the trait keeps the SQLite option open.

## D14. No model output is trusted

**Decision.** The plan JSON is parsed defensively (unknown kinds fall back to `think`, a parse
failure degrades to a direct answer), and capability input/output are validated against JSON
Schema before and after execution.
**Rejected.** Assuming the model returns the requested shape.
**Consequence.** A malformed plan produces a worse answer, never a crashed run.
## D15. Localization: one typed dictionary, English as the source of truth

**Decision.** `apps/web/src/i18n.tsx` holds two flat tables (`en`, `zh`) with namespaced keys
(`nav.*`, `common.*`, `state.*`, `chat.*`, ...). The English table defines the `MessageKey` union,
so a typo is a compile error. Lookup falls back to English and then to the key itself, and runtime
identifiers (lifecycle states, severities) fall back to the raw value - a new backend state can
never render as a blank or as a missing-translation artefact. The active locale is persisted per
browser and seeded from `navigator.language`; `format.ts` reads it through a module-level setter so
dates and numbers follow the language without threading a locale through every call site.

**Rejected.** An i18n library (react-i18next and friends): the console has roughly 250 strings in
two languages, no plurals engine needs, no message extraction pipeline, and no server rendering.
A dependency would have cost more than the whole dictionary. Also rejected: keying the dictionary
by English source text, which silently turns copy edits into missing translations.

**Consequence.** Adding a language is one table plus one entry in `LOCALES`; adding a string is one
line in two tables. Untranslated keys are visible in code review because the `zh` table is typed as
`Partial<Record<MessageKey, string>>` and the fallback is deliberate, not accidental.

## D16. Theming: CSS custom properties, one attribute, no theme objects in JS

**Decision.** Every colour is a CSS custom property defined twice in `styles.css`: once in `:root`
(dark) and once under `[data-theme='light']`. `theme.tsx` only decides which attribute value is on
`<html>` - `light`, `dark`, or `system` resolved through `prefers-color-scheme` with a live
listener - and persists the choice. The 37 colours that had been written inline in the stylesheet
were replaced by semantic tokens (`--bg-chip`, `--border-warn-strong`, `--text-accent`, ...), so no
rule outside the two palette blocks contains a literal colour.

**Rejected.** A CSS-in-JS or utility-framework theme object: it would move colour decisions into
components, which is exactly the coupling the palette blocks exist to prevent. Also rejected:
preprocessor variables, which cannot be switched at runtime without a rebuild.

**Consequence.** A component changes appearance without knowing that themes exist, the two palettes
cannot drift (every token has a counterpart in both), and a third theme is one more block. React
Flow's `--xy-*` variables are mapped onto the same tokens so graph views follow the switch too.

---

## D17. Discovery defaults to a shared directory, not to the network

**Decision.** Nodes find each other through a per-user directory of small JSON advertisements
(`%LOCALAPPDATA%\agora-agent-os\nodes`, `$XDG_RUNTIME_DIR/agora-agent-os/nodes`, ...), written
atomically, refreshed on a heartbeat, expired by TTL, and read by `LocalFileDiscovery`. It is on by
default. The libp2p/mDNS backend implements the same `NodeDiscovery` trait and is chosen by the
composition root.

**Why.** The case that matters first is two checkouts of this repository running on one machine as
two processes owned by the same user. A directory is a rendezvous for exactly that case: no
multicast, no firewall exception, no bootstrap list, no configuration at all - and it is
inspectable with `dir`, which is what an operator needs when a node does not show up. mDNS is the
right answer for "a node on another machine", and it costs a libp2p dependency and a network
surface that a single-machine setup should not have to pay for.

**Rejected.** mDNS-only: it is the higher-friction default (multicast is often blocked, and it
answers a question the user did not ask). A central registry service: it reintroduces exactly the
control-plane-on-the-hot-path coupling the architecture avoids. UDP broadcast: same reach as the
directory, none of the inspectability.

**Correction.** The first implementation resolved a node's identity to its *name* when
`AGENTOS_NODE_ID` was unset. Since every node defaults to the same name, two checkouts wrote one
advertisement, each skipped it as its own, and zero-configuration discovery - the entire point -
did not work. Identity is now generated on first start and persisted in `<data_dir>/node.id`:
unique per instance, stable across restarts, and independent of the label. Names are for humans;
the console appends a port when two nodes share one.

**Consequence.** Discovery is a hint, never a fact: a stale advertisement expires, a corrupt file is
skipped, and a hostile node id cannot escape the directory (it is hashed). Nothing on the request
path depends on it - `/v1/nodes` reads a cached view that a background heartbeat maintains, and
join/leave are ordinary events on the bus. Two running instances must still not share a data
directory; discovery shares knowledge, not state.
