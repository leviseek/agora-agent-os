/**
 * The single App-level store.
 *
 * Everything shared between views lives here: connection settings (base URL + token,
 * persisted to localStorage), the runtime meta/health snapshot, the session list and the
 * selected session detail, the live event ring buffer, and the socket status. Views read
 * it through useApp() and call the actions; nothing is drilled through props.
 */

import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { AgentOsClient, ApiError, toApiError } from './api';
import type {
  EventRecord,
  HealthResponse,
  LoginResponse,
  PendingApproval,
  PostMessageResponse,
  RuntimeMeta,
  SessionDetail,
  SessionSummary,
} from './api';
import { EventStream } from './ws';
import type { ServerMessage, WsDetail, WsStatus } from './ws';
import { tGlobal } from './i18n';

const TOKEN_KEY = 'agentos.token';
const BASE_URL_KEY = 'agentos.baseUrl';
const AUTO_RECONNECT_KEY = 'agentos.wsAutoReconnect';

/** Live events kept in memory; older ones are still available from /v1/events. */
const EVENT_BUFFER_LIMIT = 1500;

/** Events that mean "the session's durable state changed, re-read it". */
const REFRESH_KINDS = new Set([
  'run_created',
  'run_completed',
  'run_failed',
  'session_message_handled',
  'session_closed',
]);

/** Events that mean the pending-approvals list changed. */
const APPROVAL_KINDS = new Set([
  'approval_requested',
  'approval_granted',
  'approval_denied',
  'approval_expired',
]);

function readStorage(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeStorage(key: string, value: string | null): void {
  try {
    if (value === null) window.localStorage.removeItem(key);
    else window.localStorage.setItem(key, value);
  } catch {
    // Storage can be unavailable (private mode); the app still works, it just forgets.
  }
}

export type ConnectionState = 'disconnected' | 'connecting' | 'online' | 'error';

/** One image the conversation refers to, as the transcript describes it. */
export interface AttachedImage {
  artifact_id: string;
  name: string;
  mime: string;
}

export interface AppStoreValue {
  baseUrl: string;
  token: string;
  health: HealthResponse | null;
  meta: RuntimeMeta | null;
  connection: ConnectionState;
  connectionError: ApiError | null;
  login: LoginResponse | null;

  /** The shared client; recreated when the base URL or token changes. */
  client: AgentOsClient;

  sessions: SessionSummary[];
  sessionsError: ApiError | null;
  sessionsLoading: boolean;
  /** Live answer text per run, replaced by the stored answer once the run finishes. */
  streamed: Map<string, string>;
  /** Images attached to the conversation, newest last, as the transcript refers to them. */
  attachments: AttachedImage[];
  /** Capability calls waiting for a decision. */
  approvals: PendingApproval[];
  /** Ask the runtime what is waiting. The approvals view calls this when it opens. */
  refreshApprovals: () => Promise<void>;
  /**
   * True when the connected runtime is older than this console: it has no /v1/approvals route.
   * Polling a route that does not exist produces a 404 storm in the browser console, so the poll
   * stops and the view explains why instead of retrying forever.
   */
  approvalsUnsupported: boolean;
  decideApproval: (id: string, approved: boolean, reason?: string) => Promise<void>;
  /** URL of an artifact's bytes. */
  artifactUrl: (id: string) => string;
  selectedSessionId: string | null;
  detail: SessionDetail | null;
  detailError: ApiError | null;
  detailLoading: boolean;

  events: EventRecord[];
  wsStatus: WsStatus;
  wsDetail: WsDetail | null;
  /** The most recent frame received on the socket (hello, pong, health, snapshot, error). */
  lastSocketMessage: ServerMessage | null;
  autoReconnect: boolean;

  actionError: ApiError | null;
  busy: boolean;

  setBaseUrl: (next: string) => void;
  setToken: (next: string) => void;
  setAutoReconnect: (next: boolean) => void;
  connect: (overrides?: { baseUrl?: string; token?: string }) => Promise<void>;
  disconnect: () => void;
  /** Search term applied to the session list; empty means no filter. */
  sessionQuery: string;
  setSessionQuery: (next: string) => void;
  refreshSessions: () => Promise<void>;
  renameSession: (id: string, title: string) => Promise<void>;
  createSession: (title: string, userId: string) => Promise<string | null>;
  selectSession: (id: string | null) => void;
  refreshDetail: (id?: string) => Promise<void>;
  closeSession: (id: string) => Promise<void>;
  sendGoal: (text: string, wait: boolean, images?: string[]) => Promise<PostMessageResponse | null>;
  cancelRun: () => Promise<void>;
  clearEvents: () => void;
  clearActionError: () => void;
  pingSocket: () => void;
  reconnectSocket: () => void;
}

const AppContext = createContext<AppStoreValue | null>(null);

export function useApp(): AppStoreValue {
  const value = useContext(AppContext);
  if (value === null) {
    throw new Error('useApp() must be used inside <AppProvider>');
  }
  return value;
}

export function AppProvider({ children }: { children: ReactNode }) {
  const [baseUrl, setBaseUrlState] = useState<string>(() => readStorage(BASE_URL_KEY) ?? '');
  const [token, setTokenState] = useState<string>(() => readStorage(TOKEN_KEY) ?? '');

  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [meta, setMeta] = useState<RuntimeMeta | null>(null);
  const [connection, setConnection] = useState<ConnectionState>('disconnected');
  const [connectionError, setConnectionError] = useState<ApiError | null>(null);
  const [login, setLogin] = useState<LoginResponse | null>(null);

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [sessionQuery, setSessionQueryState] = useState<string>('');
  // Live answer text per run, replaced by the stored answer when the run completes.
  const [streamed, setStreamed] = useState<Map<string, string>>(() => new Map());
  const [attachments, setAttachments] = useState<AttachedImage[]>([]);
  const [approvals, setApprovals] = useState<PendingApproval[]>([]);
  const [approvalsUnsupported, setApprovalsUnsupported] = useState(false);
  const approvalsUnsupportedRef = useRef(false);

  const artifactUrl = useCallback(
    (id: string): string => clientRef.current.artifactUrl(id),
    [],
  );
  const [sessionsError, setSessionsError] = useState<ApiError | null>(null);
  const [sessionsLoading, setSessionsLoading] = useState(false);
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(null);
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [detailError, setDetailError] = useState<ApiError | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);

  const [events, setEvents] = useState<EventRecord[]>([]);
  const [wsStatus, setWsStatus] = useState<WsStatus>('idle');
  const [wsDetail, setWsDetail] = useState<WsDetail | null>(null);
  const [lastSocketMessage, setLastSocketMessage] = useState<ServerMessage | null>(null);
  const [autoReconnect, setAutoReconnectState] = useState<boolean>(
    () => readStorage(AUTO_RECONNECT_KEY) !== 'false',
  );

  const [actionError, setActionError] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);

  const client = useMemo(() => new AgentOsClient({ baseUrl, token }), [baseUrl, token]);

  const clientRef = useRef(client);
  clientRef.current = client;
  const metaRef = useRef(meta);
  metaRef.current = meta;
  const selectedRef = useRef(selectedSessionId);
  selectedRef.current = selectedSessionId;
  const queryRef = useRef(sessionQuery);
  queryRef.current = sessionQuery;

  const [stream] = useState(
    () =>
      new EventStream({
        url: () => {
          const current = metaRef.current;
          return clientRef.current.wsUrl(current === null ? null : current.ws_path);
        },
        autoReconnect: readStorage(AUTO_RECONNECT_KEY) !== 'false',
        minDelayMs: 500,
        maxDelayMs: 8000,
      }),
  );

  const detailTimer = useRef<number | null>(null);
  const sessionsTimer = useRef<number | null>(null);

  const refreshDetail = useCallback(async (id?: string): Promise<void> => {
    const target = id ?? selectedRef.current;
    if (target === null) {
      setDetail(null);
      setDetailError(null);
      return;
    }
    setDetailLoading(true);
    try {
      const response = await clientRef.current.getSession(target);
      setDetail(response);
      setDetailError(null);
      // Attachments live in the transcript, so they are collected from there rather than from a
      // second source of truth. Only the newest few matter: the strip shows what was just sent.
      // A runtime without the route (an older build) simply has no attachments to show.
      const transcript = await clientRef.current
        .sessionTranscript(target, 60)
        .catch(() => ({ messages: [], total: 0, truncated: false }));
      const found: AttachedImage[] = [];
      for (const message of transcript.messages) {
        for (const part of message.parts) {
          if (part.type === 'image' && part.artifact_id !== undefined && part.name !== undefined) {
            found.push({
              artifact_id: part.artifact_id,
              name: part.name,
              mime: part.mime ?? 'application/octet-stream',
            });
          }
        }
      }
      setAttachments(found.slice(-6));
    } catch (cause) {
      setDetailError(toApiError(cause));
    } finally {
      setDetailLoading(false);
    }
  }, []);

  // Nothing asks about approvals until somebody looks. An earlier version polled this route every
  // few seconds from the moment the page loaded, which against a runtime that predates the route
  // painted a 404 into the browser console on every single refresh.
  const refreshApprovals = useCallback(async (): Promise<void> => {
    if (approvalsUnsupportedRef.current) return;
    try {
      const response = await clientRef.current.listApprovals();
      setApprovals(response.approvals);
    } catch (cause) {
      // A 404 means the route is not there at all - an older runtime. Remember that so the view can
      // say so once instead of asking again on every open, and do not retry the rest either.
      const status = cause instanceof ApiError ? cause.status : 0;
      if (status === 404) {
        approvalsUnsupportedRef.current = true;
        setApprovalsUnsupported(true);
        setApprovals([]);
      }
    }
  }, []);

  const decideApproval = useCallback(
    async (id: string, approved: boolean, reason?: string): Promise<void> => {
      setActionError(null);
      try {
        await clientRef.current.decideApproval(id, approved, reason);
        await refreshApprovals();
      } catch (cause) {
        setActionError(toApiError(cause));
      }
    },
    [refreshApprovals],
  );

  const loadSessions = useCallback(async (active: AgentOsClient, query = ''): Promise<void> => {
    setSessionsLoading(true);
    try {
      const response = await active.listSessions(query);
      setSessions(response.sessions);
      setSessionsError(null);
      if (selectedRef.current === null && response.sessions.length > 0) {
        const first = response.sessions[0];
        if (first !== undefined) {
          setSelectedSessionId(first.id);
          void refreshDetail(first.id);
        }
      }
    } catch (cause) {
      setSessionsError(toApiError(cause));
    } finally {
      setSessionsLoading(false);
    }
  }, [refreshDetail]);

  const refreshSessions = useCallback(
    (): Promise<void> => loadSessions(clientRef.current, queryRef.current),
    [loadSessions],
  );

  const setSessionQuery = useCallback(
    (next: string): void => {
      queryRef.current = next;
      setSessionQueryState(next);
      void loadSessions(clientRef.current, next);
    },
    [loadSessions],
  );

  const renameSession = useCallback(
    async (id: string, title: string): Promise<void> => {
      setActionError(null);
      try {
        await clientRef.current.renameSession(id, title);
        await refreshSessions();
        if (selectedRef.current === id) {
          await refreshDetail(id);
        }
      } catch (cause) {
        setActionError(toApiError(cause));
      }
    },
    [refreshDetail, refreshSessions],
  );

  const scheduleDetailRefresh = useCallback((): void => {
    if (detailTimer.current !== null) window.clearTimeout(detailTimer.current);
    detailTimer.current = window.setTimeout(() => {
      detailTimer.current = null;
      void refreshDetail();
    }, 350);
  }, [refreshDetail]);

  const scheduleSessionsRefresh = useCallback((): void => {
    if (sessionsTimer.current !== null) window.clearTimeout(sessionsTimer.current);
    sessionsTimer.current = window.setTimeout(() => {
      sessionsTimer.current = null;
      void refreshSessions();
    }, 700);
  }, [refreshSessions]);

  // One subscription for the whole app: the buffer feeds every live view.
  useEffect(() => {
    const offEvent = stream.onEvent((event) => {
      setEvents((previous) => {
        if (previous.some((item) => item.id === event.id)) return previous;
        const next =
          previous.length >= EVENT_BUFFER_LIMIT
            ? previous.slice(previous.length - EVENT_BUFFER_LIMIT + 1)
            : previous.slice();
        next.push(event);
        return next;
      });
      if (event.kind === 'agent_delta') {
        // The preview of an answer still being written. Keyed by run so two runs cannot blend,
        // and dropped the moment the run finishes - the stored answer replaces it.
        const payload = event.payload as { run_id?: string; text?: string } | null;
        const runId = payload?.run_id;
        const text = payload?.text;
        if (typeof runId === 'string' && typeof text === 'string' && text.length > 0) {
          setStreamed((previous) => {
            const next = new Map(previous);
            next.set(runId, (next.get(runId) ?? '') + text);
            return next;
          });
        }
      }
      if (event.kind === 'run_completed' || event.kind === 'run_failed' || event.kind === 'run_cancelled') {
        const payload = event.payload as { run_id?: string } | null;
        const runId = payload?.run_id;
        if (typeof runId === 'string') {
          setStreamed((previous) => {
            if (!previous.has(runId)) return previous;
            const next = new Map(previous);
            next.delete(runId);
            return next;
          });
        }
      }
      if (APPROVAL_KINDS.has(event.kind)) {
        void refreshApprovals();
      }
      if (REFRESH_KINDS.has(event.kind)) {
        if (event.session_id !== null && event.session_id === selectedRef.current) {
          scheduleDetailRefresh();
        }
        scheduleSessionsRefresh();
      }
    });

    const offStatus = stream.onStatus((status, detail) => {
      setWsStatus(status);
      setWsDetail(detail);
    });

    const offMessage = stream.onMessage((message: ServerMessage) => {
      setLastSocketMessage(message);
      if (message.type === 'goal_result' || message.type === 'accepted') {
        scheduleDetailRefresh();
        scheduleSessionsRefresh();
        return;
      }
      if (message.type === 'error') {
        const code = typeof message.code === 'string' ? message.code : 'ws_error';
        const text = typeof message.message === 'string' ? message.message : 'the socket reported an error';
        setActionError(new ApiError({ code, message: text }));
      }
    });

    return () => {
      offEvent();
      offStatus();
      offMessage();
    };
  }, [stream, scheduleDetailRefresh, scheduleSessionsRefresh]);

  // The socket only runs while the console is connected. The toggle controls *retries*:
  // with it off the socket still opens once, it just does not come back after a drop.
  useEffect(() => {
    if (connection === 'online') {
      stream.setAutoReconnect(autoReconnect);
      if (!autoReconnect) stream.connect();
    } else {
      stream.setAutoReconnect(false);
    }
  }, [stream, connection, autoReconnect]);

  useEffect(
    () => () => {
      stream.close();
      if (detailTimer.current !== null) window.clearTimeout(detailTimer.current);
      if (sessionsTimer.current !== null) window.clearTimeout(sessionsTimer.current);
    },
    [stream],
  );

  const setBaseUrl = useCallback((next: string): void => {
    setBaseUrlState(next);
    writeStorage(BASE_URL_KEY, next.trim().length > 0 ? next : null);
  }, []);

  const setToken = useCallback((next: string): void => {
    setTokenState(next);
    writeStorage(TOKEN_KEY, next.length > 0 ? next : null);
  }, []);

  const setAutoReconnect = useCallback((next: boolean): void => {
    setAutoReconnectState(next);
    writeStorage(AUTO_RECONNECT_KEY, next ? 'true' : 'false');
  }, []);

  const connect = useCallback(async (overrides?: { baseUrl?: string; token?: string }): Promise<void> => {
    const activeBaseUrl = overrides?.baseUrl ?? baseUrl;
    const activeToken = overrides?.token ?? token;
    setConnection('connecting');
    setConnectionError(null);
    const active = new AgentOsClient({
      baseUrl: activeBaseUrl,
      token: activeToken.length > 0 ? activeToken : null,
    });
    try {
      const healthResponse = await active.healthz();
      setHealth(healthResponse);
      const metaResponse = await active.meta();
      setMeta(metaResponse);
      if (metaResponse.auth_required) {
        setLogin(await active.login(activeToken));
      } else {
        setLogin({ ok: true, auth_required: false, note: 'this node has no token configured' });
      }
      setConnection('online');
      await loadSessions(active);
    } catch (cause) {
      setConnectionError(toApiError(cause));
      setConnection('error');
    }
  }, [baseUrl, token, loadSessions]);

  const disconnect = useCallback((): void => {
    stream.close();
    setTokenState('');
    writeStorage(TOKEN_KEY, null);
    setConnection('disconnected');
    setConnectionError(null);
    setLogin(null);
    setHealth(null);
    setMeta(null);
    setSessions([]);
    setSessionsError(null);
    setSelectedSessionId(null);
    setDetail(null);
    setDetailError(null);
    setEvents([]);
  }, [stream]);

  const createSession = useCallback(
    async (title: string, userId: string): Promise<string | null> => {
      setBusy(true);
      setActionError(null);
      try {
        const trimmedUser = userId.trim().length > 0 ? userId.trim() : 'anonymous';
        const trimmedTitle = title.trim().length > 0 ? title.trim() : 'untitled session';
        const record = await clientRef.current.createSession(trimmedUser, trimmedTitle);
        await refreshSessions();
        setSelectedSessionId(record.id);
        await refreshDetail(record.id);
        return record.id;
      } catch (cause) {
        setActionError(toApiError(cause));
        return null;
      } finally {
        setBusy(false);
      }
    },
    [refreshDetail, refreshSessions],
  );

  const selectSession = useCallback(
    (id: string | null): void => {
      setSelectedSessionId(id);
      if (id === null) {
        setDetail(null);
        setDetailError(null);
        return;
      }
      void refreshDetail(id);
    },
    [refreshDetail],
  );

  const closeSession = useCallback(
    async (id: string): Promise<void> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.closeSession(id);
        if (selectedRef.current === id) {
          setSelectedSessionId(null);
          setDetail(null);
        }
        await refreshSessions();
      } catch (cause) {
        setActionError(toApiError(cause));
      } finally {
        setBusy(false);
      }
    },
    [refreshSessions],
  );

  const sendGoal = useCallback(
    async (text: string, wait: boolean, images: string[] = []): Promise<PostMessageResponse | null> => {
      const id = selectedRef.current;
      setActionError(null);
      if (id === null) {
        setActionError(new ApiError({ code: 'no_session', message: tGlobal('error.noSessionOrCreate') }));
        return null;
      }
      setBusy(true);
      try {
        const response = await clientRef.current.postMessage(id, text, wait, images);
        await refreshDetail(id);
        await refreshSessions();
        return response;
      } catch (cause) {
        setActionError(toApiError(cause));
        return null;
      } finally {
        setBusy(false);
      }
    },
    [refreshDetail, refreshSessions],
  );

  const cancelRun = useCallback(async (): Promise<void> => {
    const id = selectedRef.current;
    setActionError(null);
    if (id === null) {
      setActionError(new ApiError({ code: 'no_session', message: tGlobal('error.noSession') }));
      return;
    }
    try {
      const response = await clientRef.current.cancelSession(id);
      if (!response.cancelled) {
        setActionError(
          new ApiError({
            code: 'nothing_to_cancel',
            message: tGlobal('error.noRunInFlight'),
          }),
        );
      }
      await refreshDetail(id);
    } catch (cause) {
      setActionError(toApiError(cause));
    }
  }, [refreshDetail]);

  const clearEvents = useCallback((): void => setEvents([]), []);
  const clearActionError = useCallback((): void => setActionError(null), []);
  const reconnectSocket = useCallback((): void => {
    stream.close();
    stream.setAutoReconnect(true);
    stream.connect();
  }, [stream]);

  const pingSocket = useCallback((): void => {
    if (!stream.ping()) {
      setActionError(new ApiError({ code: 'socket_closed', message: tGlobal('error.socketNotOpen') }));
    }
  }, [stream]);

  const value = useMemo<AppStoreValue>(
    () => ({
      baseUrl,
      token,
      health,
      meta,
      connection,
      connectionError,
      login,
      client,
      sessions,
      sessionsError,
      sessionsLoading,
      streamed,
      attachments,
      approvals,
      approvalsUnsupported,
      refreshApprovals,
      decideApproval,
      artifactUrl,
      sessionQuery,
      setSessionQuery,
      renameSession,
      selectedSessionId,
      detail,
      detailError,
      detailLoading,
      events,
      wsStatus,
      wsDetail,
      lastSocketMessage,
      autoReconnect,
      actionError,
      busy,
      setBaseUrl,
      setToken,
      setAutoReconnect,
      connect,
      disconnect,
      refreshSessions,
      createSession,
      selectSession,
      refreshDetail,
      closeSession,
      sendGoal,
      cancelRun,
      clearEvents,
      clearActionError,
      pingSocket,
      reconnectSocket,
    }),
    [
      baseUrl,
      token,
      health,
      meta,
      connection,
      connectionError,
      login,
      client,
      sessions,
      sessionsError,
      sessionsLoading,
      streamed,
      attachments,
      approvals,
      approvalsUnsupported,
      refreshApprovals,
      decideApproval,
      artifactUrl,
      sessionQuery,
      setSessionQuery,
      renameSession,
      selectedSessionId,
      detail,
      detailError,
      detailLoading,
      events,
      wsStatus,
      wsDetail,
      lastSocketMessage,
      autoReconnect,
      actionError,
      busy,
      setBaseUrl,
      setToken,
      setAutoReconnect,
      connect,
      disconnect,
      refreshSessions,
      createSession,
      selectSession,
      refreshDetail,
      closeSession,
      sendGoal,
      cancelRun,
      clearEvents,
      clearActionError,
      pingSocket,
      reconnectSocket,
    ],
  );

  return <AppContext.Provider value={value}>{children}</AppContext.Provider>;
}