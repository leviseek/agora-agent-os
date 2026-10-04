# Agora Agent OS Console (apps/web)

The web client for the Agent OS runtime: sessions, chat, agent state, task graph, capability
invocation, fleet topology, the live event log and node settings - all over the HTTP/WebSocket
gateway in `crates/api`.

Stack: Vite + React 19 + TypeScript (strict) + @xyflow/react. No CSS framework, no state library,
no router: plain CSS, React context for the app store, and a tiny view switcher.

## Run it

```bash
# from the repository root (pnpm workspace)
pnpm install
pnpm --filter @agentos/web dev     # http://localhost:5173

# or standalone, without the workspace:
cd apps/web && pnpm install --ignore-workspace && pnpm dev
```

> Use **http://localhost:5173**. Vite binds to `localhost`; on machines where `localhost`
> resolves to IPv6 only, `http://127.0.0.1:5173` is refused while `http://[::1]:5173` works.

The dev server proxies the runtime so the browser can stay same-origin:

| Path | Target |
|---|---|
| `/healthz` | `http://127.0.0.1:8788` |
| `/readyz` | `http://127.0.0.1:8788` |
| `/v1` (incl. `/v1/ws`, `ws: true`) | `http://127.0.0.1:8788` |

The target is `AGENTOS_HTTP_TARGET` from the environment / `.env`, defaulting to
`http://127.0.0.1:8788` (the runtime's `api.http_addr`). Start the runtime with
`cargo run -p agentos-server` (or `agentos start`) before connecting.

Other scripts:

```bash
pnpm build      # tsc && vite build -> dist/
pnpm typecheck  # tsc --noEmit
pnpm preview    # serve the production build
```

## How it talks to the runtime

Everything goes through two modules:

* **`src/api.ts`** - `AgentOsClient`: one typed method per endpoint (healthz, meta, login,
  sessions CRUD/status/messages/cancel/events/graph/snapshot/restore/migrate, capabilities +
  invoke, tasks, workers, actors, events, models, metrics) and a TypeScript interface for every
  payload. Every failure - transport, HTTP status or JSON decode - is normalised into an
  `ApiError` carrying `code`, HTTP `status`, `retryable`, `details` and `message`, which is what
  the UI renders. Nothing is swallowed into `console.log`.
* **`src/ws.ts`** - `EventStream`: the `/v1/ws` socket with auto-reconnect (exponential backoff
  with jitter, 500 ms - 8 s), a listener API (`onEvent`, `onMessage`, `onStatus`) and command
  senders (`ping`, `sendGoal`, `cancel`, `requestSnapshot`, `requestHealth`).

`src/store.tsx` holds the single app-level store (connection settings, meta/health, session list
and selected session detail, the live event ring buffer, socket status) and is consumed through
`useApp()`; `src/hooks.ts` adds `useAsyncData` (fetch + interval refresh) and
`useSessionEvents` (durable `/v1/sessions/{id}/events` merged with the live socket feed).

### Authentication

* The token is kept in `localStorage` under `agentos.token` (base URL: `agentos.baseUrl`).
* `Connect` calls `GET /healthz`, then `GET /v1/meta`; when `auth_required` is true it calls
  `POST /v1/auth/login` and reports a rejected token inline.
* Requests carry `Authorization: Bearer <token>`.
* **The socket carries the token as `?token=<token>`**, because the browser WebSocket API cannot
  set request headers and the runtime guards `/v1/ws` like every other `/v1` route. That is the
  second form `crates/api/src/middleware.rs` accepts. Socket URLs are masked (`<token>`) wherever
  the UI displays them, but note that a token in a query string can end up in proxy/access logs.

### Views

| View | Endpoints / sources |
|---|---|
| Connection | `/healthz`, `/v1/meta`, `/v1/auth/login` |
| Sessions | `GET/POST /v1/sessions`, `DELETE /v1/sessions/{id}` |
| Chat | `GET /v1/sessions/{id}` (runtime runs) + `POST .../messages`, `POST .../cancel`, live socket events |
| Agent state | run projection from `/v1/sessions/{id}/status`, step timeline from `agent_step`/`task_*`/`tool_*` events |
| Task graph | `GET /v1/sessions/{id}/graph` (polled every 5 s) + live `task_*` events, laid out by dependency depth |
| Capabilities | `GET /v1/capabilities` (q/tags filters) + `POST /v1/capabilities/{name}/invoke` with a JSON input box |
| Topology | `GET /v1/workers` + `GET /v1/actors` + `GET /v1/sessions`, plus directory cache hits/misses |
| Events | live socket feed with kind/severity/text filters, pause, clear, ping, history reload |
| Settings | `/v1/meta`, `/v1/models`, `/v1/metrics` (raw Prometheus text), socket reconnect toggle |

The task-graph and topology canvases are React Flow graphs; nodes are coloured by lifecycle state
(`pending`, `ready`, `running`, `retrying`, `succeeded`, `failed`, `cancelled`) and edges come from
the task `deps` (or worker -> actor -> session ownership).

## Layout

```
apps/web
  index.html
  vite.config.ts          # dev server + proxy to the runtime
  tsconfig.json           # strict, noEmit type check used by the build
  src/
    main.tsx              # React root
    App.tsx               # shell: sidebar, topbar, main panel
    navigation.tsx         # the tiny view switcher (context, no router)
    store.tsx             # app-level state, actions, socket wiring
    api.ts                # typed HTTP client + every payload interface
    ws.ts                 # reconnecting event socket
    hooks.ts              # useAsyncData / useSessionEvents / useAllEvents
    components.tsx        # panel, banner, badge, kv, json, empty state
    format.ts             # time/duration/bytes/percent helpers, token masking
    styles.css            # all styling
    views/                # one component per view (9)
```
## Screenshots

Both captures come from a real browser (headless Edge over CDP) after clicking the switches in the
sidebar - no reload, no rebuild.

| Light + English | Dark + 中文 |
|---|---|
| ![light](../../docs/screenshots/console-light-en.png) | ![dark](../../docs/screenshots/console-dark-zh.png) |

## Language and theme

Both switches live in the sidebar footer and again in **Settings -> Appearance**. Both apply
immediately, both are stored per browser, and neither needs a reload.

| switch | values | default | stored under |
|---|---|---|---|
| Language | English / 中文 | browser language (`navigator.language`, `zh*` -> Chinese) | `agentos.locale` |
| Theme | Light / Dark / System | System (`prefers-color-scheme`) | `agentos.theme` |

* `src/i18n.tsx` - the two dictionaries and the `useI18n()` hook. Keys are typed from the English
  table, so a missing or misspelled key fails the build. `tState()` and `tSeverity()` translate
  runtime lifecycle values and fall back to the raw identifier, which keeps a new backend state
  readable instead of blank.
* `src/theme.tsx` - resolves `system` through `matchMedia` and writes `data-theme` on `<html>`.
* `src/styles.css` - the only file with colour literals: `:root` (dark) and
  `[data-theme='light']`. Every other rule uses a semantic token, including the React Flow
  variables, so the graph views re-theme with everything else.
* `src/format.ts` - dates and numbers follow the active locale through `setFormatLocale()`.

Adding a string: add the key to both tables. Adding a language: extend `LOCALES`,
`LOCALE_LABELS` and one new table; no component changes are needed.