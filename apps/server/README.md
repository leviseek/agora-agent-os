# @agentos/server - TypeScript control server

A thin, typed facade in front of the Rust runtime plus the application-level **orchestration
layer**. It runs on Node 22+ with native TypeScript execution (no build step, no bundler).

## Why it exists

The runtime executes one agent loop per session. Splitting a goal into independent sub-goals,
running them in parallel sessions and aggregating the answers is an *application* concern. It
lives here, in TypeScript, where it can change without touching the runtime.

## Run

```bash
# 1. runtime (Rust)
cargo run -p agentos-server

# 2. control server (Node)
pnpm --filter @agentos/server start
```

Environment:

| Variable | Default | Meaning |
|---|---|---|
| `PORT` | `8790` | port this server listens on |
| `RUNTIME_URL` | `http://127.0.0.1:8788` | the Rust runtime HTTP endpoint |
| `AGENTOS_AUTH_TOKEN` | unset | bearer token forwarded to the runtime |

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `/healthz` | control server and runtime health |
| GET | `/api/meta` | runtime metadata |
| GET | `/api/sessions` | session list |
| GET | `/api/capabilities` | capability list |
| POST | `/api/orchestrate` | `{goal, strategy?}` -> multi-session run, `{goal, plan:true}` -> decomposition only |
| ALL | `/api/runtime/*` | verbatim proxy to `/v1/*` on the runtime |
| WS | `/ws` | fan-out of the runtime event stream (browser clients share one upstream socket) |

## Orchestration

```bash
curl -X POST http://127.0.0.1:8790/api/orchestrate \
  -H 'content-type: application/json' \
  -d '{"goal":"what is 12*12; and what is 9*9","strategy":"fan-out"}'
```

Returns one result per sub-goal, each executed in its own session, plus timing and the number of
runtime events produced. Because sessions are independent actors, the sub-goals genuinely run in
parallel.