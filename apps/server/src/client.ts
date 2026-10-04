/**
 * Typed client for the Agent OS runtime HTTP API.
 *
 * The runtime is the source of truth for sessions, tasks, capabilities, workers and events.
 * This layer only shapes requests and normalizes errors, so the orchestration code above it
 * stays readable.
 */

export interface RuntimeErrorBody {
  code: string;
  message: string;
  retryable: boolean;
  details?: unknown;
}

export class AgentOsError extends Error {
  readonly code: string;
  readonly retryable: boolean;
  readonly status: number;
  readonly details?: unknown;

  constructor(status: number, error: RuntimeErrorBody) {
    super(error.message);
    this.name = "AgentOsError";
    this.status = status;
    this.code = error.code;
    this.retryable = error.retryable;
    this.details = error.details;
  }
}

export interface SessionSummary {
  id: string;
  user_id: string;
  title: string;
  state: string;
  actor_id: string;
  message_count: number;
  created_at: number;
  updated_at: number;
}

export interface RunResult {
  session_id: string;
  agent_id: string;
  state: string;
  answer: string | null;
  error: string | null;
  steps: number;
}

export interface CapabilityDescriptor {
  id: string;
  name: string;
  version: string;
  description: string;
  kind: string;
  tags: string[];
  permission: Record<string, unknown>;
  health: string;
}

export interface EventRecord {
  id: string;
  seq: number;
  kind: string;
  severity: string;
  ts: number;
  message: string;
  payload: unknown;
  session_id: string | null;
  actor_id: string | null;
  agent_id: string | null;
  task_id: string | null;
}

export interface RuntimeMeta {
  node: string;
  domain_version: string;
  auth_required: boolean;
  store_backend: string;
  capabilities: number;
  workers_online: number;
  sessions: number;
  grpc_addr: string;
  ws_path: string;
}

export class RuntimeClient {
  // Explicit fields rather than constructor parameter properties: Node's built-in TypeScript
  // support strips types without transforming syntax, and parameter properties are not stripped.
  readonly baseUrl: string;
  readonly token?: string;

  constructor(baseUrl: string, token?: string) {
    this.baseUrl = baseUrl;
    this.token = token;
  }

  private headers(): Record<string, string> {
    const headers: Record<string, string> = { "content-type": "application/json" };
    if (this.token) headers.authorization = "Bearer " + this.token;
    return headers;
  }

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const response = await fetch(this.baseUrl + path, {
      method,
      headers: this.headers(),
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await response.text();
    const parsed = text.length > 0 ? JSON.parse(text) : null;
    if (!response.ok) {
      const error = (parsed && parsed.error) || { code: "unknown", message: text, retryable: false };
      throw new AgentOsError(response.status, error as RuntimeErrorBody);
    }
    return parsed as T;
  }

  health(): Promise<{ status: string; domain_version: string }> {
    return this.request("GET", "/healthz");
  }

  meta(): Promise<RuntimeMeta> {
    return this.request("GET", "/v1/meta");
  }

  async login(token: string): Promise<boolean> {
    const result = await this.request<{ ok: boolean }>("POST", "/v1/auth/login", { token });
    return result.ok;
  }

  async listSessions(): Promise<SessionSummary[]> {
    const result = await this.request<{ sessions: SessionSummary[] }>("GET", "/v1/sessions");
    return result.sessions;
  }

  createSession(userId: string, title: string): Promise<SessionSummary> {
    return this.request("POST", "/v1/sessions", { user_id: userId, title });
  }

  getSession(id: string): Promise<{ session: SessionSummary; runtime: unknown }> {
    return this.request("GET", "/v1/sessions/" + id);
  }

  postGoal(id: string, text: string, wait: boolean): Promise<RunResult> {
    return this.request("POST", "/v1/sessions/" + id + "/messages", { text, wait });
  }

  cancel(id: string): Promise<{ cancelled: boolean }> {
    return this.request("POST", "/v1/sessions/" + id + "/cancel", {});
  }

  closeSession(id: string): Promise<{ closed: boolean }> {
    return this.request("DELETE", "/v1/sessions/" + id);
  }

  async listCapabilities(): Promise<CapabilityDescriptor[]> {
    const result = await this.request<{ capabilities: CapabilityDescriptor[] }>("GET", "/v1/capabilities");
    return result.capabilities;
  }

  invokeCapability(
    name: string,
    input: unknown,
    sessionId?: string,
  ): Promise<{ output: unknown; duration_ms: number; attempts: number }> {
    return this.request("POST", "/v1/capabilities/" + encodeURIComponent(name) + "/invoke", {
      input,
      session_id: sessionId,
    });
  }

  async listEvents(limit = 200, sessionId?: string): Promise<EventRecord[]> {
    const query = new URLSearchParams({ limit: String(limit) });
    if (sessionId) query.set("session_id", sessionId);
    const result = await this.request<{ events: EventRecord[] }>("GET", "/v1/events?" + query.toString());
    return result.events;
  }

  metrics(): Promise<string> {
    return fetch(this.baseUrl + "/v1/metrics", { headers: this.headers() }).then((r) => r.text());
  }

  /** WebSocket URL for the live event stream. */
  wsUrl(): string {
    const url = new URL(this.baseUrl);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    url.pathname = "/v1/ws";
    url.search = this.token ? "?token=" + encodeURIComponent(this.token) : "";
    return url.toString();
  }
}
