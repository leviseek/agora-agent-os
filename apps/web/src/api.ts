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
  // Out of the hot store, in a package of its own: readable, restorable, not openable.
  | 'archived'
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
  /** Which identity mode this runtime is in, and therefore what ownership can mean. */
  identity?: {
    mode: 'single-principal' | 'principals';
    principals: number;
    /** False only on a node with no token and no principal table: there a declared name is the only
     *  thing that can tell two people apart. */
    authenticated?: boolean;
    /** What a declared name does here: a mode on an open node, `ignored` wherever a token exists. */
    asserted?: 'off' | 'optional' | 'required' | 'ignored';
    separation: string;
  };
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
  /** Provider this session prefers; null lets the router decide. */
  model_hint?: string | null;
  /** Thinking effort this session asks for; null means the provider default. */
  reasoning_effort?: string | null;
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
  /** The workspace this session belongs to; null only for records written before workspaces existed. */
  workspace_id?: string | null;
  /** Who owns it: a person, and optionally the node they are on. */
  owner?: { user_id: string; node_id?: string | null } | null;
  /** The caller's own role on this session, or null when they have none. */
  my_role?: string | null;
}

/** Token accounting for one run or one session. All zero means "nobody recorded it". */
export interface TokenUsage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  calls: number;
}

export interface RunSummary {
  agent_id: string;
  state: AgentRunState | string;
  goal: string;
  steps: number;
  /** What this run cost across every model call it made, including parallel task calls. */
  usage?: TokenUsage;
  /** Who actually answered the planning call; null on records written before it was recorded. */
  provider?: string | null;
  /** The provider this run was asked to prefer, and the thinking effort it was given. */
  model_hint?: string | null;
  reasoning_effort?: string | null;
  /** Set when the answer came from a fallback: "mock answered after deepseek: HTTP 400 ...". */
  degraded?: string | null;
  /** The files this goal carried, so a transcript can show them next to the turn they belong to. */
  attachments?: RunAttachment[];
  /** Who asked for this run. Null on turns from before authorship was recorded. */
  author?: { user_id: string; node_id?: string | null } | null;
  model: string | null;
  final_answer: string | null;
  error: string | null;
}

/** One file attached to a goal: an image the model saw, or a document it read. */
export interface RunAttachment {
  /** 'image' or 'document'. */
  kind: string;
  artifact_id: string;
  name: string;
  content_type?: string | null;
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
  /** Summed from the runs, so it cannot drift from them. */
  usage?: TokenUsage;
  runs: RunSummary[];
  graphs: GraphSummary[];
}

/** A capability call parked until an operator decides. */
export interface PendingApproval {
  id: string;
  capability: string;
  session_id: string;
  actor_id?: string | null;
  task_id?: string | null;
  arguments_preview: string;
  reason: string;
  created_at: number;
}

/** One turn of the conversation, as stored by the session actor. */
export interface TranscriptEntry {
  id: string;
  session_id: string;
  role: string;
  parts: {
    type?: string;
    kind?: string;
    text?: string;
    artifact_id?: string;
    name?: string;
    mime?: string;
  }[];
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
  /** What the gateway says about the caller here: their role and what it allows. */
  you?: SessionAccess;
}

/** The caller's own standing on one session, as the runtime computed it. */
export interface SessionAccess {
  user_id: string;
  node_id: string | null;
  roles: string[];
  /** owner, editor, participant, viewer - or null when they have no role at all. */
  session_role: string | null;
  can: string[];
}

/** What a session may use, and what the runtime offers. */
export interface SessionCapabilitiesResponse {
  session_id: string;
  /** Which record holds the narrowing: the session, or its workspace (D20). */
  scope?: 'session' | 'workspace';
  workspace_id?: string | null;
  runtime: string[];
  session: SessionCapabilities;
  effective: string[];
}

export interface SessionCapabilities {
  /** null means "whatever the runtime allows". */
  allow: string[] | null;
  deny: string[];
  approval_required: string[];
}

export interface AccessRequest {
  id: string;
  principal: { user_id: string; node_id?: string | null };
  role: string;
  note?: string | null;
  created_at: number;
  state: 'pending' | 'approved' | 'rejected';
  decided_by?: string | null;
  decided_at?: number | null;
  granted_role?: string | null;
}

/** One request, naming whatever it is about: a workspace, or a pre-workspace session. */
export interface AccessInboxEntry {
  /** The workspace the request is about, since access is decided there. */
  workspace_id?: string | null;
  workspace_name?: string | null;
  /** The conversation, for a request made before workspaces existed. */
  session_id?: string | null;
  session_title?: string | null;
  session_owner?: { user_id: string; node_id?: string | null } | null;
  request: AccessRequest;
}

export interface AccessInboxResponse {
  user_id: string;
  /** Pending requests on sessions the caller may decide. */
  to_decide: AccessInboxEntry[];
  /** Everything the caller asked for, whatever came of it. */
  mine: AccessInboxEntry[];
}

export interface AccessRequestsResponse {
  session_id?: string;
  workspace_id?: string;
  scope?: 'session' | 'workspace';
  /** True when the caller may answer them. */
  may_decide: boolean;
  requests: AccessRequest[];
}

/** A role handed out on a workspace: in force in every session of it. */
export interface WorkspaceGrant {
  user_id: string;
  node_id?: string | null;
  role: string;
  granted_by?: string | null;
  granted_at?: number | null;
}

/** The durable workspace record. */
export interface WorkspaceRecord {
  id: string;
  name: string;
  owner: { user_id: string; node_id?: string | null };
  created_at: number;
  updated_at: number;
  archived_at?: number | null;
  grants: WorkspaceGrant[];
  access_requests: AccessRequest[];
  metadata?: Record<string, string>;
  capabilities?: SessionCapabilities;
}

/** A workspace as a list renders it: the record plus what the caller may do in it. */
export interface WorkspaceSummary {
  workspace: WorkspaceRecord;
  workspace_role: string | null;
  can: string[];
  session_count?: number;
}

export interface WorkspacesResponse {
  workspaces: WorkspaceSummary[];
  total: number;
}

export interface WorkspaceDetail extends WorkspaceSummary {
  sessions: SessionSummary[];
}

/** What a workspace narrowed, which is what every session in it inherits. */
export interface WorkspaceCapabilitiesResponse {
  workspace_id: string;
  runtime: string[];
  workspace: SessionCapabilities;
  effective: string[];
}

/** What an archive package says about itself. */
export interface ArchiveManifest {
  format_version: number;
  session_id: string;
  title: string;
  owner?: { user_id: string; node_id?: string | null } | null;
  node_id: string;
  created_at: number;
  archived_at: number;
  runs: number;
  messages: number;
  artifacts: number;
  files: Record<string, string>;
}

export interface ArchiveEntry {
  /** The name the API addresses this package by. */
  id: string;
  path: string;
  bytes: number;
  manifest: ArchiveManifest;
}

export interface ArchivesResponse {
  root: string;
  enabled: boolean;
  archives: ArchiveEntry[];
}

/** One package with the first turns of what was said inside it. */
export interface ArchiveDetail {
  id: string;
  path: string;
  bytes: number;
  manifest: ArchiveManifest;
  preview: { role: string; parts: { type: string; text?: string; name?: string }[]; created_at: number }[];
}

/** Who the gateway thinks the caller is, for the whole console. */
export interface Whoami {
  user_id: string;
  node_id: string | null;
  roles: string[];
  admin: boolean;
  /** How this identity was established: token, asserted or operator. */
  source?: 'token' | 'asserted' | 'operator';
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
  /** Who this console says it is, on a node that has no token to identify it. */
  user?: string | null;
  /** The node that user is on, when the identity is pinned to one. */
  node?: string | null;
}

type QueryValue = string | number | boolean | null | undefined;

export class AgentOsClient {
  readonly baseUrl: string;
  readonly token: string | null;
  readonly user: string | null;
  readonly node: string | null;

  constructor(config: ClientConfig) {
    this.baseUrl = config.baseUrl;
    this.token = config.token;
    this.user = config.user ?? null;
    this.node = config.node ?? null;
  }

  withToken(token: string | null): AgentOsClient {
    return new AgentOsClient({ baseUrl: this.baseUrl, token, user: this.user, node: this.node });
  }

  withBaseUrl(baseUrl: string): AgentOsClient {
    return new AgentOsClient({ baseUrl, token: this.token, user: this.user, node: this.node });
  }

  /** The identity headers, for callers that build their own request (uploads). */
  identityHeaders(): Headers {
    const headers = new Headers();
    if (this.user !== null && this.user.length > 0) headers.set('X-Agora-User', this.user);
    if (this.node !== null && this.node.length > 0) headers.set('X-Agora-Node', this.node);
    return headers;
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
    const params: string[] = [];
    if (this.token !== null && this.token.length > 0) params.push('token=' + encodeURIComponent(this.token));
    // The socket is guarded like every other /v1 route, and a browser cannot set a header on the
    // handshake, so the declared identity travels in the query - the second form the runtime reads.
    if (this.user !== null && this.user.length > 0) params.push('user=' + encodeURIComponent(this.user));
    if (this.node !== null && this.node.length > 0) params.push('node=' + encodeURIComponent(this.node));
    const query = params.length > 0 ? '?' + params.join('&') : '';
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

  /**
   * One request path, and only one way to send a body.
   *
   * `body` is deliberately absent from the init type: passing a hand-serialised body skipped the
   * `Content-Type: application/json` header this method sets, and the runtime answered 415 to every
   * console action that used it - archiving, capability changes, access requests. The type now
   * rejects that shape, so the next person writing a call cannot make the same mistake twice.
   */
  private async request<T>(path: string, init: Omit<RequestInit, 'body'> & { json?: unknown }): Promise<T> {
    const headers = new Headers(init.headers);
    headers.set('Accept', 'application/json');
    if (this.token !== null && this.token.length > 0) {
      headers.set('Authorization', 'Bearer ' + this.token);
    }
    for (const [key, value] of this.identityHeaders()) {
      headers.set(key, value);
    }
    let body: BodyInit | undefined;
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
    for (const [key, value] of this.identityHeaders()) {
      headers.set(key, value);
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

  /** List sessions; q is a case-insensitive substring over the title and the user id. */
  listSessions(query?: string): Promise<SessionsResponse> {
    const suffix = query !== undefined && query.trim().length > 0 ? '?q=' + encodeURIComponent(query.trim()) : '';
    return this.request<SessionsResponse>('/v1/sessions' + suffix, { method: 'GET' });
  }

  /** Rename a session. The runtime renames it through the session actor, not just in the store. */
  renameSession(id: string, title: string): Promise<{ session: SessionRecord }> {
    return this.request<{ session: SessionRecord }>('/v1/sessions/' + id, {
      method: 'PATCH',
      json: { title },
    });
  }

  /**
   * Change a session's title, preferred provider or thinking effort.
   *
   * An absent field is left alone; an empty string clears it, which is how the console goes back
   * to letting the router choose.
   */
  configureSession(
    id: string,
    patch: { title?: string; model?: string; effort?: string },
  ): Promise<{ session: SessionRecord }> {
    return this.request<{ session: SessionRecord }>('/v1/sessions/' + id, {
      method: 'PATCH',
      json: patch,
    });
  }

  listSessionsUnfiltered(): Promise<SessionsResponse> {
    return this.request<SessionsResponse>('/v1/sessions', { method: 'GET' });
  }

  createSession(userId: string, title: string, workspaceId?: string | null): Promise<SessionRecord> {
    const json: Record<string, unknown> = { user_id: userId, title };
    // Named, the session goes into that workspace; omitted, the runtime uses the caller's own
    // default one. Either way the session has a workspace, which is what access is decided on.
    if (workspaceId !== undefined && workspaceId !== null && workspaceId.length > 0) {
      json.workspace_id = workspaceId;
    }
    return this.request<SessionRecord>('/v1/sessions', { method: 'POST', json });
  }

  // --- workspaces ------------------------------------------------------------------------

  listWorkspaces(): Promise<WorkspacesResponse> {
    return this.request<WorkspacesResponse>('/v1/workspaces', { method: 'GET' });
  }

  createWorkspace(name: string): Promise<{ workspace: WorkspaceRecord }> {
    return this.request('/v1/workspaces', { method: 'POST', json: { name } });
  }

  getWorkspace(id: string): Promise<WorkspaceDetail> {
    return this.request<WorkspaceDetail>('/v1/workspaces/' + encodeURIComponent(id), { method: 'GET' });
  }

  renameWorkspace(id: string, name: string): Promise<{ workspace: WorkspaceRecord }> {
    return this.request('/v1/workspaces/' + encodeURIComponent(id), { method: 'PATCH', json: { name } });
  }

  listWorkspaceSessions(id: string): Promise<{ workspace_id: string; sessions: SessionSummary[]; total: number }> {
    return this.request('/v1/workspaces/' + encodeURIComponent(id) + '/sessions', { method: 'GET' });
  }

  /** Create a conversation inside a workspace. The session's owner is the workspace's owner. */
  createWorkspaceSession(id: string, userId: string, title: string): Promise<SessionRecord> {
    return this.request<SessionRecord>('/v1/workspaces/' + encodeURIComponent(id) + '/sessions', {
      method: 'POST',
      json: { user_id: userId, title },
    });
  }

  workspaceCapabilities(id: string): Promise<WorkspaceCapabilitiesResponse> {
    return this.request<WorkspaceCapabilitiesResponse>(
      '/v1/workspaces/' + encodeURIComponent(id) + '/capabilities',
      { method: 'GET' },
    );
  }

  setWorkspaceCapabilities(
    id: string,
    body: { allow: string[] | null; deny?: string[]; approval_required?: string[] },
  ): Promise<{ workspace: WorkspaceRecord }> {
    return this.request('/v1/workspaces/' + encodeURIComponent(id) + '/capabilities', {
      method: 'PUT',
      json: body,
    });
  }

  /** Grant a role on a workspace: one decision, in force in every session of it. */
  grantWorkspaceAccess(
    id: string,
    userId: string,
    nodeId: string | null,
    role: string,
  ): Promise<{ workspace: WorkspaceRecord }> {
    const json: Record<string, unknown> = { user_id: userId, role };
    if (nodeId !== null && nodeId.length > 0) json.node_id = nodeId;
    return this.request('/v1/workspaces/' + encodeURIComponent(id) + '/access', { method: 'POST', json });
  }

  revokeWorkspaceAccess(id: string, userId: string, nodeId?: string | null): Promise<{ workspace: WorkspaceRecord }> {
    const json: Record<string, unknown> = { user_id: userId };
    if (nodeId !== undefined && nodeId !== null && nodeId.length > 0) json.node_id = nodeId;
    return this.request('/v1/workspaces/' + encodeURIComponent(id) + '/access', {
      method: 'DELETE',
      json,
    });
  }

  workspaceAccessRequests(id: string): Promise<AccessRequestsResponse> {
    return this.request<AccessRequestsResponse>(
      '/v1/workspaces/' + encodeURIComponent(id) + '/access-requests',
      { method: 'GET' },
    );
  }

  requestWorkspaceAccess(
    id: string,
    role: string,
    note?: string,
  ): Promise<{ request: AccessRequest; workspace_id?: string }> {
    return this.request('/v1/workspaces/' + encodeURIComponent(id) + '/access-requests', {
      method: 'POST',
      json: note === undefined ? { role } : { role, note },
    });
  }

  decideWorkspaceAccess(
    id: string,
    requestId: string,
    approve: boolean,
    role?: string,
  ): Promise<{ request: AccessRequest; workspace: WorkspaceRecord }> {
    return this.request(
      '/v1/workspaces/' +
        encodeURIComponent(id) +
        '/access-requests/' +
        encodeURIComponent(requestId) +
        '/decide',
      { method: 'POST', json: role === undefined ? { approve } : { approve, role } },
    );
  }

  getSession(id: string): Promise<SessionDetail> {
    return this.request<SessionDetail>('/v1/sessions/' + encodeURIComponent(id), { method: 'GET' });
  }

  closeSession(id: string): Promise<CloseResponse> {
    return this.request<CloseResponse>('/v1/sessions/' + encodeURIComponent(id), { method: 'DELETE' });
  }

  /** Opening a closed session again. Closing stops the actor; this takes it back. */
  openSession(id: string): Promise<{ opened: boolean; session: SessionRecord }> {
    return this.request<{ opened: boolean; session: SessionRecord }>(
      '/v1/sessions/' + encodeURIComponent(id) + '/open',
      { method: 'POST', json: {} },
    );
  }

  whoami(): Promise<Whoami> {
    return this.request<Whoami>('/v1/auth/whoami', { method: 'GET' });
  }

  /** Write a conversation into an archive package. Closes it on the way if it is still open. */
  archiveSession(id: string): Promise<{ archived: boolean; archive_id: string; path: string; bytes: number; manifest: ArchiveManifest }> {
    return this.request('/v1/sessions/' + encodeURIComponent(id) + '/archive', {
      method: 'POST',
      json: {},
    });
  }

  /** What this session may use: the runtime's set, the session's narrowing, and the intersection. */
  sessionCapabilities(id: string): Promise<SessionCapabilitiesResponse> {
    return this.request<SessionCapabilitiesResponse>(
      '/v1/sessions/' + encodeURIComponent(id) + '/capabilities',
      { method: 'GET' },
    );
  }

  setSessionCapabilities(
    id: string,
    body: { allow: string[] | null; deny?: string[]; approval_required?: string[] },
  ): Promise<{ session: SessionRecord }> {
    return this.request('/v1/sessions/' + encodeURIComponent(id) + '/capabilities', {
      method: 'PUT',
      json: body,
    });
  }

  /** Everything waiting on this person, across every session. */
  accessInbox(): Promise<AccessInboxResponse> {
    return this.request<AccessInboxResponse>('/v1/access-requests', { method: 'GET' });
  }

  accessRequests(id: string): Promise<AccessRequestsResponse> {
    return this.request<AccessRequestsResponse>(
      '/v1/sessions/' + encodeURIComponent(id) + '/access-requests',
      { method: 'GET' },
    );
  }

  requestAccess(id: string, role: string, note?: string): Promise<{ request: AccessRequest }> {
    return this.request('/v1/sessions/' + encodeURIComponent(id) + '/access-requests', {
      method: 'POST',
      json: note === undefined ? { role } : { role, note },
    });
  }

  decideAccess(
    id: string,
    requestId: string,
    approve: boolean,
    role?: string,
  ): Promise<{ request: AccessRequest; session?: SessionRecord; workspace?: WorkspaceRecord; scope?: string }> {
    return this.request(
      '/v1/sessions/' + encodeURIComponent(id) + '/access-requests/' + encodeURIComponent(requestId) + '/decide',
      { method: 'POST', json: role === undefined ? { approve } : { approve, role } },
    );
  }
  listArchives(): Promise<ArchivesResponse> {
    return this.request<ArchivesResponse>('/v1/archives', { method: 'GET' });
  }

  getArchive(id: string): Promise<ArchiveDetail> {
    return this.request<ArchiveDetail>('/v1/archives/' + encodeURIComponent(id), { method: 'GET' });
  }

  restoreArchive(id: string, title?: string): Promise<{ restored: boolean; session: SessionRecord }> {
    return this.request('/v1/archives/' + encodeURIComponent(id) + '/restore', {
      method: 'POST',
      json: title === undefined ? {} : { title },
    });
  }

  deleteArchive(id: string): Promise<{ deleted: boolean; archive_id: string }> {
    return this.request('/v1/archives/' + encodeURIComponent(id), { method: 'DELETE' });
  }

  sessionStatus(id: string): Promise<SessionRuntime> {
    return this.request<SessionRuntime>('/v1/sessions/' + encodeURIComponent(id) + '/status', {
      method: 'GET',
    });
  }

  postMessage(
    id: string,
    text: string,
    wait: boolean,
    images: string[] = [],
    model?: string,
    effort?: string,
    attachments: string[] = [],
  ): Promise<PostMessageResponse> {
    const json: Record<string, unknown> = { text, wait };
    if (images.length > 0) json.images = images;
    // Artifact ids from upload() - how a browser attaches an image it cannot put in the workspace.
    if (attachments.length > 0) json.attachments = attachments;
    // A one-off choice for this goal only; the session keeps whatever it had.
    if (model !== undefined && model.length > 0) json.model = model;
    if (effort !== undefined && effort.length > 0) json.effort = effort;
    return this.request<PostMessageResponse>('/v1/sessions/' + encodeURIComponent(id) + '/messages', {
      method: 'POST',
      json,
    });
  }

  /** Capability calls waiting for an operator decision. */
  listApprovals(): Promise<{ approvals: PendingApproval[]; total: number }> {
    return this.request<{ approvals: PendingApproval[]; total: number }>('/v1/approvals', {
      method: 'GET',
    });
  }

  /** Decide one. The id is single use. */
  decideApproval(
    id: string,
    approved: boolean,
    reason?: string,
  ): Promise<{ approved: boolean }> {
    const body: Record<string, unknown> = { approved };
    if (reason !== undefined && reason.length > 0) body.reason = reason;
    return this.request<{ approved: boolean }>('/v1/approvals/' + encodeURIComponent(id), {
      method: 'POST',
      json: body,
    });
  }

  /** The URL of an artifact's bytes, for rendering an image the transcript refers to. */
  artifactUrl(id: string): string {
    return this.baseUrl.replace(/\/$/, '') + '/v1/artifacts/' + encodeURIComponent(id);
  }

  /**
   * Upload an image and get back an attachment to name in a goal.
   *
   * The body is the file itself rather than JSON, so a 5 MiB screenshot does not become 7 MiB of
   * base64 on the way. The name travels in a header the runtime reads; the content type is a hint,
   * because the runtime decides the type by the bytes.
   */
  async upload(
    sessionId: string,
    file: File,
  ): Promise<{ artifact_id: string; name: string; content_type: string; bytes: number }> {
    const headers = new Headers({ Accept: 'application/json' });
    if (this.token !== null && this.token.length > 0) {
      headers.set('Authorization', 'Bearer ' + this.token);
    }
    for (const [key, value] of this.identityHeaders()) {
      headers.set(key, value);
    }
    headers.set('Content-Type', file.type.length > 0 ? file.type : 'application/octet-stream');
    // Non-ASCII file names have to survive the trip; the runtime strips any path part.
    headers.set('X-AgentOS-Filename', encodeURIComponent(file.name));
    const response = await fetch(
      this.url('/v1/sessions/' + encodeURIComponent(sessionId) + '/attachments'),
      { method: 'POST', headers, body: file },
    );
    if (!response.ok) throw await readError(response);
    return (await response.json()) as {
      artifact_id: string;
      name: string;
      content_type: string;
      bytes: number;
    };
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