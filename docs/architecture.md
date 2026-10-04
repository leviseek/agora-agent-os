# Architecture

## The one-sentence version

An Agent OS is an Actor Runtime that also owns an agent loop, a capability mesh, a task
scheduler, state and memory, an artifact system and a model router. Everything is an interface;
the composition root is the only place that knows a concrete implementation.

## Layers

```
        +-------------------------------------------------------------+
        |  Application & Agent Layer                                  |
        |  Tauri 2 desktop  |  React web client  |  TypeScript control|
        |                   |                    |  server + orchestr.|
        +-----------------------------+-------------------------------+
                                      | HTTP / WebSocket / gRPC
        +-----------------------------v-------------------------------+
        |  Gateway  (crates/api)                                      |
        |  auth - rate limit - request id - session routing - WS fanout|
        +-----------------------------+-------------------------------+
                                      |
        +-----------------------------v-------------------------------+
        |  Control Plane (crates/control-plane)   OFF the hot path    |
        |  Placement | Actor Directory | Worker Registry | Policy      |
        +-----------------------------+-------------------------------+
                                      | cache miss / migration / recovery only
        +-----------------------------v-------------------------------+
        |  Agent Runtime (crates/agent-runtime)                       |
        |  SessionManager -> SessionActor -> AgentLoop                |
        |     Goal -> Plan -> Act(task graph) -> Observe -> Finalize  |
        +------+----------------+------------------+-----------------+
               |                |                  |
        +------v------+  +------v-------+  +-------v--------+
        | Task        |  | Capability   |  | Model Router   |
        | Scheduler   |  | Mesh         |  | Provider       |
        | (crates/    |  | (crates/     |  | adapters       |
        | task-sched.)|  | capability-  |  | (crates/model- |
        |             |  | runtime)     |  | router)        |
        +------+------+  +------+-------+  +----------------+
               |                |
        +------v----------------v-------------------------------------+
        |  Data Plane                                                 |
        |  Actor Runtime | Wasm Sandbox | Workers | P2P | Blob store  |
        +------+------------------------------------------------------+
               |
        +------v------------------------------------------------------+
        |  State: Store (memory | file | redb) - Event Bus - Artifacts |
        +-------------------------------------------------------------+

        +-------------------------------------------------------------+
        |  Trust Plane (interfaces + local implementations only)       |
        |  DID | Capability Token | Reputation | Billing | Storage     |
        +-------------------------------------------------------------+
```

## The three separator rules

1. **Agent != Model.** An agent is a spec (prompt, capability allow-list, step budget) plus a
   loop. The model is chosen per step by the router. `AgentRun.model` records what was used.
2. **Agent != Process.** An agent is a serializable state machine (`AgentRun` +
   `SessionActorState`). It can be checkpointed mid-run, moved and resumed.
3. **Capability != Worker.** A capability describes *what* can be done; a worker is *where* it
   runs. The mesh resolves the first and the placement service picks the second, independently.

## Request path (one user goal)

```
client
  -> POST /v1/sessions/{id}/messages          (gateway: auth, rate limit, request id)
  -> SessionManager.actor_for(session)        (actor runtime cache; on a miss: directory)
  -> ActorHandle.send(UserGoal)               (typed message into a per-session mailbox)
  -> SessionActor.handle                      (serialized: one message at a time per session)
      -> AgentLoop.run
          Goal    : AgentRun created, run_created event
          Plan    : ModelRouter.complete(json_mode) -> Plan (think/capability/respond steps)
          Act     : plan -> TaskGraph -> Scheduler (parallel, retry, timeout, cancel)
                      capability nodes -> CapabilityMesh.invoke
                          policy gate -> schema check -> execute -> output schema check
          Observe : task outcomes -> AgentStep + Observation, tool_call/tool_result events
          Finalize: ModelRouter.complete -> final answer, memory write, run_completed event
  -> assistant message appended to the transcript
  -> response (or, for wait=false, the event stream carries progress)
```

## Concurrency model

| scope | guarantee | mechanism |
|---|---|---|
| inside one session | strict FIFO order | one mailbox, one task per actor |
| across sessions | full parallelism | one actor task per session |
| across task nodes | bounded parallelism | `JoinSet` + `max_concurrency` |
| across workers | placement score | least-pressure policy |
| slow sessions | cannot block others | no shared lock on the message path |

Nothing on the message path touches the control plane. The directory is consulted on a cache miss
and after a failure, and both cases are counted (`agentos_directory_cache_hits_total` /
`agentos_directory_cache_misses_total`).

## State, events, artifacts

* **Store** (`Store` trait): keyed documents plus append-only logs. Three backends:
  `memory` (tests), `file` (default: one JSON envelope per key, JSONL logs),
  `redb` (embedded KV, `--features agentos-storage/redb-backend`).
* **Event bus**: one ordered, replayable stream. Every lifecycle transition is an event, which
  makes the log the audit trail, the UI feed and the replay source for recovery - one stream,
  three consumers.
* **Artifacts**: immutable, content-addressed blobs (SHA-256) plus metadata records. Capabilities
  receive an `ArtifactStore` handle; they never see a path or a bucket.
* **Memory**: short-term, episodic, semantic, knowledge and (reserved) vector records behind a
  `MemoryStore` trait.

## Trust plane

Interfaces exist, implementations are local and deliberately non-blocking:

| concern | seam | v1 implementation |
|---|---|---|
| identity | `PolicyEngine` gate + typed actor ids | static bearer token at the edge |
| capability token | `CapabilityPolicy` | in-process policy rules |
| reputation | `CapabilityLoad` counters per capability | in-memory, exported as metrics |
| billing | - | reserved: usage is already counted per provider/capability |
| decentralised storage | `BlobStore` | filesystem / memory |

No payment, no chain writes, no external storage network - by design for v1.

## Failure model

| failure | behaviour |
|---|---|
| capability returns a retryable error | mesh retries, then the task retries, then the node fails |
| task node fails permanently | dependents are cancelled, the graph finishes as `failed` |
| model provider fails or has no key | router fails over to the next candidate, always including the offline mock |
| actor panics | caught with `catch_unwind`, actor marked `failed`, recovered from a checkpoint |
| worker lease expires | worker marked `lost`, `worker_offline` event, placement excludes it |
| request cancelled | cancellation token fires; the loop stops at the next await point |
| storage error | surfaced as `storage`, retryable, never silently swallowed |
