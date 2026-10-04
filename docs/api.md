# API reference

Three surfaces, one runtime:

* **HTTP/WS** (`crates/api`) for browsers, the desktop shell and scripts.
* **gRPC** (`crates/network`, `proto/agentos/v1`) for node-to-node calls.
* **CLI** (`crates/cli`) for humans and for the acceptance walkthrough.

All errors share one shape:

```json
{ "error": { "code": "policy_denied", "message": "filesystem-read denied: ...", "retryable": false, "details": {} } }
```

`code` is the machine-readable `ErrorKind`: `invalid_input`, `not_found`, `conflict`,
`unauthorized`, `policy_denied`, `rate_limited`, `timeout`, `cancelled`, `capability`, `model`,
`storage`, `network`, `sandbox`, `migration`, `internal`, `unavailable`.

## HTTP

Open routes (no token, still rate limited): `GET /healthz`, `GET /readyz`, `GET /v1/meta`,
`POST /v1/auth/login`.

Everything else under `/v1` requires `Authorization: Bearer <token>` when the node has
`AGENTOS_AUTH_TOKEN` set. The WebSocket accepts `?token=` for browsers.

| Method | Path | Body / query | Returns |
|---|---|---|---|
| GET | `/healthz` | - | `{status, service, domain_version}` |
| GET | `/readyz` | - | `{status, health}` (503 when storage is unhealthy) |
| GET | `/v1/meta` | - | node, versions, backends, limits, workspace root |
| POST | `/v1/auth/login` | `{token}` | `{ok, auth_required}` |
| GET | `/v1/sessions` | - | `{sessions:[SessionSummary]}` |
| POST | `/v1/sessions` | `{user_id?, title?}` | `SessionRecord` |
| GET | `/v1/sessions/{id}` | - | `{session, runtime}` |
| DELETE | `/v1/sessions/{id}` | - | `{closed:true}` |
| GET | `/v1/sessions/{id}/status` | - | runtime view: state, runs, graphs |
| POST | `/v1/sessions/{id}/messages` | `{text, wait?}` | run result or `{accepted:true}` |
| POST | `/v1/sessions/{id}/cancel` | - | `{cancelled}` |
| GET | `/v1/sessions/{id}/transcript` | `?limit` | `{messages:[TranscriptEntry], total, truncated}` - the conversation; how a client that posted with `wait:false` reads the reply |
| GET | `/v1/sessions/{id}/events` | `?limit` | `{events:[EventRecord]}` |
| GET | `/v1/sessions/{id}/graph` | - | `{graphs:[TaskGraphRecord]}` |
| GET | `/v1/sessions/{id}/snapshot` | - | `Checkpoint` |
| POST | `/v1/sessions/{id}/restore` | checkpoint (or `{checkpoint}`) | `{restored, session_id, actor_id}` |
| POST | `/v1/sessions/{id}/migrate` | `{target_worker?}` | `MigrationReport` |
| GET | `/v1/capabilities` | `?q=&tags=&healthy_only=` | `{capabilities:[Descriptor]}` |
| POST | `/v1/capabilities/{name}/invoke` | `{input, version?, session_id?}` | output + timing + attempts |
| GET | `/v1/tasks` | `?session_id=&limit=` | `{tasks:[TaskRecord]}` |
| GET | `/v1/workers` | - | `{workers:[WorkerRecord]}` |
| GET | `/v1/actors` | - | live actors, directory entries, cache counters |
| GET | `/v1/events` | `?limit=&session_id=&kinds=a,b` | `{events:[EventRecord]}` |
| GET | `/v1/nodes` | - | `{self, nodes:[NodeSummary], discovery:{backend,dir,ttl_ms,...}}` - this node plus every node discovery can see |
| GET | `/v1/models` | - | provider health and usage, plus configured provider summary |
| GET | `/v1/metrics` | - | Prometheus text |

## WebSocket `/v1/ws`

Server frames: `{"type":"hello",...}`, `{"type":"event","event":EventRecord}`,
`{"type":"pong"}`, `{"type":"accepted"}`, `{"type":"goal_result","result":{...}}`,
`{"type":"cancelled","cancelled":bool}`, `{"type":"snapshot","checkpoint":{...}}`,
`{"type":"health","health":{...}}`, `{"type":"error","code","message"}`.

Client frames: `{"type":"ping"}`, `{"type":"goal","session_id","goal","wait"?}`,
`{"type":"cancel","session_id"}`, `{"type":"snapshot","session_id"}`, `{"type":"health"}`.

## gRPC (`proto/agentos/v1`)

| service | methods |
|---|---|
| `CapabilityService` | `Invoke`, `List`, `Health` |
| `ControlService` | `RegisterWorker`, `Heartbeat`, `DeregisterWorker`, `ListWorkers`, `RegisterActor`, `LookupActor`, `UnregisterActor`, `Place` |
| `AgentService` | `CreateSession`, `ListSessions`, `CloseSession`, `GetSession`, `PostGoal`, `Cancel`, `Snapshot`, `Restore`, `Migrate`, `ListEvents`, `Health` |

The Rust clients (`GrpcCapabilityClient`, `GrpcControlClient`, `GrpcAgentClient`) are implemented and
exercised by `crates/network` tests; `GrpcCapabilityTransport` plugs the capability service into
the mesh as a `CapabilityTransport`, so a `RemoteCapability` is callable exactly like a local one.

## CLI

```
agentos start                                   # runtime: HTTP/WS + gRPC
agentos doctor                                  # configuration and environment check
agentos demo [--goal "..."]                     # full acceptance scenario with a report
agentos session create|list|show|message|cancel|close|events|snapshot|restore|migrate
agentos task create <session> --capability echo --capability calculator [--input '{"text":"hi"}']
agentos task list [--session-id <id>]
agentos capability list [--query calc] | describe <name> | invoke <name> --input '{...}'
agentos worker list | placement
agentos actor list | checkpoint <actor-id>
agentos agent inspect <session-id>
```

Global flags: `--config <file>`, `--json`, `--remote <grpc-endpoint>`.

With `--remote` the session verbs (create, list, show, message, cancel, close, events, snapshot,
migrate) go through the AgentService gRPC client instead of bootstrapping a kernel, so a CLI on
one machine drives a runtime on another:

```bash
cargo run -p agentos-server                                    # node A
cargo run -p agentos-cli -- --remote http://127.0.0.1:8789 --json session create --title "over grpc"
cargo run -p agentos-cli -- --remote http://127.0.0.1:8789 session message <session-id> "what is 13*13?"
```