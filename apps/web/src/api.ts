/**
 * agentos web client - typed access to the runtime HTTP gateway.
 *
 * One module owns every request: URL building, bearer auth, error decoding and the
 * TypeScript shape of every payload the gateway returns. Views never call fetch().
 *
 * Errors: every failure - transport, HTTP status or decode - is normalised into an
 * ApiError carrying the runtime error code, the HTTP status and the message, so the UI
 * can always render "code: message" instead of a silent console entry.
 */

import { tGlobal } from './i18n';

// ---------------------------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------------------------

export class ApiError extends Error {
  readonly code: string;
  readonly status: number;
  readonly retryable: boolean;
  readonly details: unknown;

  constructor(init: {
    code: string;
    message: string;
    status?: number;
    retryable?: boolean;
    details?: unknown;
  }) {
    super(init.message);
    this.name = 'ApiError';
    this.code = init.code;
    this.status = init.status ?? 0;
    this.retryable = init.retryable ?? false;
    this.details = init.details;
  }

  /** "unauthorized (HTTP 401)" or "network_error" for transport failures. */
  get label(): string {
    return this.status > 0 ? this.code + ' (HTTP ' + this.status + ')' : this.code;
  }
}

/** Normalise anything thrown by the client into an ApiError. */
export function toApiError(value: unknown): ApiError {
  if (value instanceof ApiError) return value;
  if (value instanceof Error) {
    return new ApiError({
      code: value.name === 'TypeError' ? 'network_error' : 'client_error',
      message: value.message,
      status: 0,
      retryable: true,
    });
  }
  return new ApiError({ code: 'unknown_error', message: String(value), status: 0 });
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function asArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

async function readError(response: Response): Promise<ApiError> {
  let code = 'http_' + response.status;
  let message = response.statusText || 'request failed';
  let retryable = response.status >= 500;
  let details: unknown = null;
  try {
    const text = await response.text();
    if (text.length > 0) {
      const parsed: unknown = JSON.parse(text);
      if (isRecord(parsed) && isRecord(parsed['error'])) {
        const envelope = parsed['error'];
        if (typeof envelope['code'] === 'string') code = envelope['code'];
        if (typeof envelope['message'] === 'string') message = envelope['message'];
        if (typeof envelope['retryable'] === 'boolean') retryable = envelope['retryable'];
        details = envelope['details'] ?? null;
      } else {
        message = text.slice(0, 400);
      }
    }
  } catch {
    // Non-JSON body: the status line is the best description we have.
  }
  return new ApiError({ code, message, status: response.status, retryable, details });
}

// ---------------------------------------------------------------------------------------------
// shared value types
// ---------------------------------------------------------------------------------------------

export type Timestamp = number;

export type SessionState =
  | 'creating'
  | 'active'
  | 'idle'
  | 'suspended'
  | 'closing'
  | 'closed'
  | 'failed';

export type ActorState =
  | 'spawning'
  | 'active'
  | 'idle'
  | 'draining'
  | 'migrating'
  | 'stopped'
  | 'failed';

export type TaskState =
  | 'pending'
  | 'ready'
  | 'running'
  | 'retrying'
  | 'succeeded'
  | 'failed'
  | 'cancelled';

export type TaskKind = 'capability' | 'model' | 'agent' | 'join';

export type WorkerState = 'joining' | 'ready' | 'draining' | 'offline' | 'lost';

export type CapabilityHealth = 'unknown' | 'healthy' | 'degraded' | 'unavailable';

export type EventSeverity = 'debug' | 'info' | 'warn' | 'error';

export type StepPhase = 'goal' | 'plan' | 'think' | 'act' | 'observe' | 'finalize';

export type AgentRunState =
  | 'goal'
  | 'planning'
  | 'thinking'
  | 'acting'
  | 'observing'
  | 'finalizing'
  | 'succeeded'
  | 'failed'
  | 'cancelled';

/** Task payloads are tagged by "kind" (see agentos-core TaskPayload). */
export interface TaskPayload {
  kind: TaskKind;
  capability?: string;
  version?: string | null;
  input?: unknown;
  prompt?: string;
  model_hint?: string | null;
  goal?: string;
  spec_name?: string;
  template?: string;
}

export interface TaskRecord {
  id: string;
  graph_id: string;
  session_id: string;
  agent_id: string | null;
  title: string;
  kind: TaskKind;
  payload: TaskPayload;
  deps: string[];
  state: TaskState;
  attempts: number;
  max_attempts: number;
  timeout_ms: number;
  result: unknown;
  error: string | null;
  created_at: Timestamp;
  started_at: Timestamp | null;
  finished_at: Timestamp | null;
  labels: Record<string, string>;
}

export interface TaskGraphRecord {
  id: string;
  session_id: string;
  agent_id: string | null;
  title: string;
  state: TaskState;
  nodes: TaskRecord[];
  created_at: Timestamp;
  updated_at: Timestamp;
}

export interface EventRecord {
  id: string;
  seq: number;
  kind: string;
  severity: EventSeverity;
  ts: Timestamp;
  message: string;
  payload: unknown;
  node_id: string | null;
  correlation_id: string | null;
  session_id: string | null;
  actor_id: string | null;
  agent_id: string | null;
  task_id: string | null;
  capability_id: string | null;
  worker_id: string | null;
  artifact_id: string | null;
}

// ---------------------------------------------------------------------------------------------
// payloads
// ---------------------------------------------------------------------------------------------

export interface HealthResponse {
  status: string;
  service: string;
  domain_version: string;
}

export interface RuntimeLimits {
  max_steps_per_run: number;
  max_concurrent_tasks: number;
  capability_timeout_ms: number;
  session_queue_capacity: number;
}

export interface RuntimeMeta {
  node: string;
  region?: string;
  domain_version: string;
  uptime_ms: number;
  auth_required: boolean;
  store_backend: string;
  blob_backend?: string;
  ws_path: string;
  grpc_addr: string;
  p2p_enabled: boolean;
  transfer: unknown;
  capabilities: number;
  workers_online: number;
  sessions: number;
  limits: RuntimeLimits;
  workspace_root: string;
}

/** One node as reported by GET /v1/nodes. */
export interface NodeSummary {
  node_id: string;
  name: string;
  address: string;
  grpc?: string | null;
  version?: string;
  capabilities?: string[];
  auth_required?: boolean;
  transport?: string;
  last_seen?: number;
  age_ms?: number;
  /** True for the node this console is talking to. */
  self: boolean;
}

export interface NodeListResponse {
  self: NodeSummary;
  nodes: NodeSummary[];
  discovery: {
    backend: string;
    enabled: boolean;
    advertise: boolean;
    dir: string;
    ttl_ms: number;
  };
}

export interface LoginResponse {
  ok: boolean;
  auth_required: boolean;
  note?: string;
}

export interface SessionRecord {
  id: string;
  user_id: string;
  title: string;
  state: SessionState;
  actor_id: string;
  worker_id: string | null;
  message_count: number;
  created_at: Timestamp;
  updated_at: Timestamp;
  closed_at: Timestamp | null;
  metadata?: Record<string, string>;
}

/** GET /v1/sessions returns the SessionSummary projection. */
export interface SessionSummary {
  id: string;
  user_id: string;
  title: string;
  state: SessionState;
  actor_id: string;
  created_at: Timestamp;
  updated_at: Timestamp;
  message_count: number;
}

export interface RunSummary {
  agent_id: string;
  state: AgentRunState | string;
  goal: string;
  steps: number;
  /** Who actually answered the planning call; null on records written before it was recorded. */
  provider?: string | null;
  model: string | null;
  final_answer: string | null;
  error: string | null;
}

export interface GraphSummary {
  graph_id: string;
  title: string;
  nodes: number;
  succeeded: number;
  failed: number;
}

export interface SessionRuntime {
  session_id: string;
  state: SessionState | string;
  title: string;
  messages: number;
  goals_handled: number;
  runs: RunSummary[];
  graphs: GraphSummary[];
}

/** One turn of the conversation, as stored by the session actor. */
export interface TranscriptEntry {
  id: string;
  session_id: string;
  role: string;
  parts: { kind?: string; text?: string }[];
  created_at: number;
  correlation_id?: string | null;
  agent_id?: string | null;
}

export interface TranscriptResponse {
  messages: TranscriptEntry[];
  total: number;
  truncated: boolean;
}

export interface SessionDetail {
  session: SessionRecord;
  runtime: SessionRuntime | null;
}

export interface SessionsResponse {
  sessions: SessionSummary[];
}

export interface EventsResponse {
  events: EventRecord[];
}

export interface GraphsResponse {
  graphs: TaskGraphRecord[];
}

export interface TasksResponse {
  tasks: TaskRecord[];
}

export interface GoalAccepted {
  accepted: true;
  session_id: string;
}

export interface GoalResult {
  session_id: string;
  agent_id: string;
  state: AgentRunState | string;
  answer: string | null;
  error: string | null;
  steps: number;
}

export type PostMessageResponse = GoalAccepted | GoalResult;

export function isGoalResult(value: PostMessageResponse): value is GoalResult {
  return !('accepted' in value);
}

export interface CancelResponse {
  cancelled: boolean;
}

export interface CloseResponse {
  closed: boolean;
  session_id?: string;
}

export interface CheckpointMeta {
  id: string;
  actor_id: string;
  session_id: string;
  generation: number;
  applied_seq: number;
  event_offset: number;
  bytes: number;
  state_hash: string;
  domain_version: string;
  created_at: Timestamp;
}

export interface Checkpoint {
  meta: CheckpointMeta;
  state: unknown;
}

export interface RestoreResponse {
  restored: boolean;
  session_id: string;
  actor_id: string;
}

export interface MigrationReport {
  actor_id: string;
  session_id: string;
  from_worker: string | null;
  to_worker: string | null;
  state: string;
  checkpoint_id: string | null;
  replayed_events: number;
  duration_ms: number;
  error: string | null;
}

export type CapabilityKind = 'builtin' | 'wasm' | 'remote';

export interface CapabilityPermission {
  fs_read: boolean;
  fs_write: boolean;
  network: boolean;
  process_exec: boolean;
  secret_names: string[];
}

export interface CapabilityLoad {
  inflight: number;
  total_calls: number;
  failures: number;
  avg_latency_ms: number;
}

export interface CapabilityDescriptor {
  id: string;
  name: string;
  version: string;
  description: string;
  kind: CapabilityKind;
  tags: string[];
  input_schema: unknown;
  output_schema: unknown;
  permission: CapabilityPermission;
  provider: unknown;
  timeout_ms: number;
  idempotent: boolean;
  health: CapabilityHealth;
  load: CapabilityLoad | null;
}

export interface CapabilitiesResponse {
  capabilities: CapabilityDescriptor[];
}

export interface InvokeResponse {
  capability: string;
  version: string;
  output: unknown;
  duration_ms: number;
  attempts: number;
  provider: unknown;
  timeout_ms?: number | null;
}

export interface WorkerCapacity {
  max_actors: number;
  max_tasks: number;
  memory_bytes: number;
}

export interface WorkerLoad {
  actors: number;
  running_tasks: number;
  cpu_percent: number;
  memory_bytes: number;
}

export interface WorkerRecord {
  id: string;
  node_id: string;
  name: string;
  state: WorkerState;
  addr: string;
  capacity: WorkerCapacity;
  load: WorkerLoad;
  labels: Record<string, string>;
  capabilities: string[];
  version: string;
  registered_at: Timestamp;
  last_heartbeat: Timestamp;
}

export interface WorkersResponse {
  workers: WorkerRecord[];
}

export interface ActorRecord {
  id: string;
  session_id: string;
  kind: string;
  state: ActorState;
  worker_id: string | null;
  generation: number;
  mailbox_depth: number;
  last_applied_seq: number;
  created_at: Timestamp;
  updated_at: Timestamp;
}

export interface DirectoryEntry {
  session_id: string;
  actor_id: string;
  kind: string;
  worker_id: string | null;
  node_id: string | null;
  generation: number;
  state: ActorState;
  endpoints: string[];
  updated_at: Timestamp;
}

export interface DirectoryCacheStats {
  hits: number;
  misses: number;
  entries: number;
}

export interface ActorsResponse {
  actors: ActorRecord[];
  directory: DirectoryEntry[];
  cache: DirectoryCacheStats;
}

export interface ModelProviderInfo {
  name: string;
  kind: unknown;
  model: string;
  health: string;
  calls: number;
  failures: number;
  avg_latency_ms: number;
  total_tokens: number;
}

export interface ConfiguredProvider {
  name?: string;
  /** "mock" is the runtime's own deterministic stand-in, never a real model. */
  kind?: string;
  model?: string;
  enabled?: boolean;
  key_env?: string;
  configured?: boolean;
}

export interface ModelsResponse {
  providers: ModelProviderInfo[];
  configured: ConfiguredProvider[];
  default: string | null;
}

// ---------------------------------------------------------------------------------------------
// client
// ---------------------------------------------------------------------------------------------

export interface ClientConfig {
  /** Empty string means same-origin (the dev server proxies /v1 and /healthz). */
  baseUrl: string;
  token: string | null;
}

type QueryValue = string | number | boolean | null | undefined;

export class AgentOsClient {
  readonly baseUrl: string;
  readonly token: string | null;

  constructor(config: ClientConfig) {
    this.baseUrl = config.baseUrl;
    this.token = config.token;
  }

  withToken(token: string | null): AgentOsClient {
    return new AgentOsClient({ baseUrl: this.baseUrl, token });
  }

  withBaseUrl(baseUrl: string): AgentOsClient {
    return new AgentOsClient({ baseUrl, token: this.token });
  }

  /** Absolute http(s) URL for a gateway path. */
  url(path: string): string {
    return this.baseUrl.replace(/\/+$/, '') + path;
  }

  query(params: Record<string, QueryValue>): string {
    const parts: string[] = [];
    for (const [key, value] of Object.entries(params)) {
      if (value === null || value === undefined || value === '') continue;
      parts.push(encodeURIComponent(key) + '=' + encodeURIComponent(String(value)));
    }
    return parts.length > 0 ? '?' + parts.join('&') : '';
  }

  /**
   * ws:// or wss:// URL for the event socket, honouring the runtime ws_path when known.
   *
   * The browser WebSocket API cannot set an Authorization header, and the gateway guards
   * /v1/ws like every other /v1 route, so the token travels as the ?token= query parameter
   * - the second form the runtime's auth middleware accepts.
   */
  wsUrl(wsPath?: string | null): string {
    const path = wsPath && wsPath.trim().length > 0 ? wsPath.trim() : '/v1/ws';
    const normalised = path.startsWith('/') ? path : '/' + path;
    const origin = typeof window === 'undefined' ? 'http://localhost' : window.location.href;
    const base = this.baseUrl.trim();
    const query =
      this.token !== null && this.token.length > 0 ? '?token=' + encodeURIComponent(this.token) : '';
    if (base.length > 0) {
      const parsed = new URL(base, origin);
      parsed.protocol = parsed.protocol === 'https:' ? 'wss:' : 'ws:';
      parsed.pathname = normalised;
      parsed.search = query;
      parsed.hash = '';
      return parsed.toString();
    }
    const isSecure = typeof window !== 'undefined' && window.location.protocol === 'https:';
    const host = typeof window !== 'undefined' ? window.location.host : 'localhost';
    return (isSecure ? 'wss://' : 'ws://') + host + normalised + query;
  }

  private async request<T>(path: string, init: RequestInit & { json?: unknown }): Promise<T> {
    const headers = new Headers(init.headers);
    headers.set('Accept', 'application/json');
    if (this.token !== null && this.token.length > 0) {
      headers.set('Authorization', 'Bearer ' + this.token);
    }
    let body: BodyInit | null | undefined = init.body;
    if (init.json !== undefined) {
      headers.set('Content-Type', 'application/json');
      body = JSON.stringify(init.json);
    }

    let response: Response;
    try {
      response = await fetch(this.url(path), { ...init, headers, body });
    } catch (cause) {
      throw new ApiError({
        code: 'network_error',
        message: tGlobal('error.network', {
          url: this.baseUrl.length > 0 ? this.baseUrl : window.location.origin,
          reason: cause instanceof Error ? cause.message : String(cause),
        }),
        status: 0,
        retryable: true,
        details: { path },
      });
    }

    if (!response.ok) throw await readError(response);
    const text = await response.text();
    if (text.length === 0) return undefined as T;
    try {
      return JSON.parse(text) as T;
    } catch {
      throw new ApiError({
        code: 'decode_error',
        message: tGlobal('error.decode'),
        status: response.status,
        details: { path, body: text.slice(0, 400) },
      });
    }
  }

  /** Metrics is text/plain, so it gets its own reader. */
  private async requestText(path: string): Promise<string> {
    const headers = new Headers({ Accept: 'text/plain' });
    if (this.token !== null && this.token.length > 0) {
      headers.set('Authorization', 'Bearer ' + this.token);
    }
    let response: Response;
    try {
      response = await fetch(this.url(path), { headers });
    } catch (cause) {
      throw new ApiError({
        code: 'network_error',
        message: cause instanceof Error ? cause.message : String(cause),
        status: 0,
        retryable: true,
        details: { path },
      });
    }
    if (!response.ok) throw await readError(response);
    return response.text();
  }

  // --- system ------------------------------------------------------------------------------

  healthz(): Promise<HealthResponse> {
    return this.request<HealthResponse>('/healthz', { method: 'GET' });
  }

  /** Nodes this runtime can see, including itself. */
  listNodes(): Promise<NodeListResponse> {
    return this.request<NodeListResponse>('/v1/nodes', { method: 'GET' });
  }

  meta(): Promise<RuntimeMeta> {
    return this.request<RuntimeMeta>('/v1/meta', { method: 'GET' });
  }

  login(token: string): Promise<LoginResponse> {
    return this.request<LoginResponse>('/v1/auth/login', { method: 'POST', json: { token } });
  }

  // --- sessions ----------------------------------------------------------------------------

  listSessions(): Promise<SessionsResponse> {
    return this.request<SessionsResponse>('/v1/sessions', { method: 'GET' });
  }

  createSession(userId: string, title: string): Promise<SessionRecord> {
    return this.request<SessionRecord>('/v1/sessions', {
      method: 'POST',
      json: { user_id: userId, title },
    });
  }

  getSession(id: string): Promise<SessionDetail> {
    return this.request<SessionDetail>('/v1/sessions/' + encodeURIComponent(id), { method: 'GET' });
  }

  closeSession(id: string): Promise<CloseResponse> {
    return this.request<CloseResponse>('/v1/sessions/' + encodeURIComponent(id), { method: 'DELETE' });
  }

  sessionStatus(id: string): Promise<SessionRuntime> {
    return this.request<SessionRuntime>('/v1/sessions/' + encodeURIComponent(id) + '/status', {
      method: 'GET',
    });
  }

  postMessage(id: string, text: string, wait: boolean): Promise<PostMessageResponse> {
    return this.request<PostMessageResponse>('/v1/sessions/' + encodeURIComponent(id) + '/messages', {
      method: 'POST',
      json: { text, wait },
    });
  }

  cancelSession(id: string): Promise<CancelResponse> {
    return this.request<CancelResponse>('/v1/sessions/' + encodeURIComponent(id) + '/cancel', {
      method: 'POST',
      json: {},
    });
  }

  /** The conversation so far: how a caller that did not wait for a run picks up its answer. */
  sessionTranscript(id: string, limit = 200): Promise<TranscriptResponse> {
    return this.request<TranscriptResponse>('/v1/sessions/' + id + '/transcript?limit=' + limit, {
      method: 'GET',
    });
  }

  sessionEvents(id: string, limit = 200): Promise<EventsResponse> {
    return this.request<EventsResponse>(
      '/v1/sessions/' + encodeURIComponent(id) + '/events' + this.query({ limit }),
      { method: 'GET' },
    );
  }

  sessionGraph(id: string): Promise<GraphsResponse> {
    return this.request<GraphsResponse>('/v1/sessions/' + encodeURIComponent(id) + '/graph', {
      method: 'GET',
    });
  }

  sessionSnapshot(id: string): Promise<Checkpoint> {
    return this.request<Checkpoint>('/v1/sessions/' + encodeURIComponent(id) + '/snapshot', {
      method: 'GET',
    });
  }

  restoreSession(id: string, checkpoint: unknown): Promise<RestoreResponse> {
    return this.request<RestoreResponse>('/v1/sessions/' + encodeURIComponent(id) + '/restore', {
      method: 'POST',
      json: checkpoint,
    });
  }

  migrateSession(id: string, targetWorker?: string | null): Promise<MigrationReport> {
    return this.request<MigrationReport>('/v1/sessions/' + encodeURIComponent(id) + '/migrate', {
      method: 'POST',
      json: { target_worker: targetWorker ?? null },
    });
  }

  // --- capabilities, tasks, fleet ----------------------------------------------------------

  listCapabilities(query?: { q?: string; tags?: string; healthy_only?: boolean }): Promise<CapabilitiesResponse> {
    return this.request<CapabilitiesResponse>(
      '/v1/capabilities' +
        this.query({
          q: query?.q,
          tags: query?.tags,
          healthy_only: query?.healthy_only === true ? 'true' : undefined,
        }),
      { method: 'GET' },
    );
  }

  invokeCapability(name: string, input: unknown): Promise<InvokeResponse> {
    return this.request<InvokeResponse>(
      '/v1/capabilities/' + encodeURIComponent(name) + '/invoke',
      { method: 'POST', json: { input } },
    );
  }

  listTasks(sessionId?: string | null, limit = 200): Promise<TasksResponse> {
    return this.request<TasksResponse>(
      '/v1/tasks' + this.query({ session_id: sessionId ?? undefined, limit }),
      { method: 'GET' },
    );
  }

  listWorkers(): Promise<WorkersResponse> {
    return this.request<WorkersResponse>('/v1/workers', { method: 'GET' });
  }

  listActors(): Promise<ActorsResponse> {
    return this.request<ActorsResponse>('/v1/actors', { method: 'GET' });
  }

  listEvents(options?: { limit?: number; sessionId?: string | null; kinds?: string[] }): Promise<EventsResponse> {
    const kinds = options?.kinds !== undefined && options.kinds.length > 0 ? options.kinds.join(',') : undefined;
    return this.request<EventsResponse>(
      '/v1/events' + this.query({ limit: options?.limit ?? 200, session_id: options?.sessionId ?? undefined, kinds }),
      { method: 'GET' },
    );
  }

  listModels(): Promise<ModelsResponse> {
    return this.request<ModelsResponse>('/v1/models', { method: 'GET' });
  }

  metrics(): Promise<string> {
    return this.requestText('/v1/metrics');
  }
}

/** Narrowing helper for JSON payload fields rendered by the UI. */
export function jsonEntries(value: unknown): [string, unknown][] {
  return isRecord(value) ? Object.entries(value) : [];
}

/** Stringify anything for display without ever throwing. */
export function jsonText(value: unknown, indent = 2): string {
  if (value === undefined) return '';
  try {
    return JSON.stringify(value, null, indent) ?? String(value);
  } catch {
    return String(value);
  }
}

/** Read a string field from an event payload, if present. */
export function payloadString(payload: unknown, key: string): string | null {
  if (!isRecord(payload)) return null;
  const value = payload[key];
  return typeof value === 'string' ? value : null;
}

/** Read a numeric field from an event payload, if present. */
export function payloadNumber(payload: unknown, key: string): number | null {
  if (!isRecord(payload)) return null;
  const value = payload[key];
  return typeof value === 'number' ? value : null;
}

/** Every string field of a payload, used by the event log summary column. */
export function payloadKeys(payload: unknown): string[] {
  return isRecord(payload) ? Object.keys(payload) : [];
}

export { asArray, isRecord };