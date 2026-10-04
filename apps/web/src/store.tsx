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
  refreshSessions: () => Promise<void>;
  createSession: (title: string, userId: string) => Promise<string | null>;
  selectSession: (id: string | null) => void;
  refreshDetail: (id?: string) => Promise<void>;
  closeSession: (id: string) => Promise<void>;
  sendGoal: (text: string, wait: boolean) => Promise<PostMessageResponse | null>;
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
    } catch (cause) {
      setDetailError(toApiError(cause));
    } finally {
      setDetailLoading(false);
    }
  }, []);

  const loadSessions = useCallback(async (active: AgentOsClient): Promise<void> => {
    setSessionsLoading(true);
    try {
      const response = await active.listSessions();
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

  const refreshSessions = useCallback((): Promise<void> => loadSessions(clientRef.current), [loadSessions]);

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
    async (text: string, wait: boolean): Promise<PostMessageResponse | null> => {
      const id = selectedRef.current;
      setActionError(null);
      if (id === null) {
        setActionError(new ApiError({ code: 'no_session', message: tGlobal('error.noSessionOrCreate') }));
        return null;
      }
      setBusy(true);
      try {
        const response = await clientRef.current.postMessage(id, text, wait);
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