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
  AccessInboxResponse,
  AccessRequestsResponse,
  ArchiveDetail,
  ArchiveEntry,
  SessionCapabilitiesResponse,
  EventRecord,
  HealthResponse,
  LoginResponse,
  PendingApproval,
  PostMessageResponse,
  RuntimeMeta,
  SessionDetail,
  SessionSummary,
  WorkspaceCapabilitiesResponse,
  WorkspaceDirectoryListing,
  WorkspaceIndex,
  WorkspaceRecord,
  WorkspaceSummary,
} from './api';
import { EventStream } from './ws';
import type { ServerMessage, WsDetail, WsStatus } from './ws';
import { tGlobal } from './i18n';

const TOKEN_KEY = 'agentos.token';
const BASE_URL_KEY = 'agentos.baseUrl';
const AUTO_RECONNECT_KEY = 'agentos.wsAutoReconnect';
// Who this console says it is. Not a secret: it is the name a person works under, and on an open
// node it is the only thing that separates their sessions from somebody else's.
const IDENTITY_USER_KEY = 'agentos.identity.user';
const IDENTITY_NODE_KEY = 'agentos.identity.node';
// The workspace the console is working in. Persisted because switching tabs or reloading must not
// silently move somebody to another working unit.
const WORKSPACE_KEY = 'agentos.workspace';

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

/** One image the runtime has accepted and stored, ready to be named in a goal. */
export interface UploadedAttachment {
  artifact_id: string;
  name: string;
  content_type: string;
  bytes: number;
}

/** What the runtime accepts. The decision is by content, but the picker should not offer more. */
export const IMAGE_TYPES = 'image/png,image/jpeg,image/gif,image/webp';

/**
 * Text files the runtime reads into the prompt.
 *
 * Extension-based, unlike everything else here: a text file has no magic bytes to sniff, so the
 * picker can only go by the name. The runtime still decides by content, and refuses a file that
 * turns out to be binary.
 */
export const DOCUMENT_TYPES =
  '.csv,.tsv,.md,.markdown,.json,.txt,.log,.yaml,.yml,.toml,.xlsx,text/csv,text/markdown,application/json,text/plain';

/** Everything the paperclip offers. */
export const ATTACHMENT_TYPES = IMAGE_TYPES + ',' + DOCUMENT_TYPES;

/**
 * The runtime's own cap on an image, in bytes.
 *
 * Duplicated here on purpose: the upload would fail with a size error anyway, but spending a
 * multi-megabyte round trip to be told so - and leaving the composer waiting on it - is worse than
 * saying no immediately. The runtime remains the authority.
 */
export const MAX_IMAGE_BYTES = 5 * 1024 * 1024;

/**
 * The runtime's cap on a text file, in bytes. Mirrors
 * `agentos_agent_runtime::documents::MAX_DOCUMENT_BYTES` for the same reason as the image cap.
 */
export const MAX_DOCUMENT_BYTES = 256 * 1024;

/**
 * One file on its way to the runtime.
 *
 * `uploading` exists so the composer never lies: a drop that has not landed yet must not look
 * ready, and a file the runtime refused has to say why instead of disappearing.
 */
export interface PendingAttachment {
  key: string;
  name: string;
  status: 'uploading' | 'ready' | 'error';
  previewUrl: string | null;
  artifactId?: string;
  error?: string;
  /** Size in bytes, so a text file can say how big it is without a thumbnail. */
  bytes?: number;
  /** Images get a thumbnail; documents get a glyph and a size. */
  isImage?: boolean;
}

/**
 * What the user has typed but not sent.
 *
 * It lives here, not in the chat view, because switching tabs unmounts the view: a draft held in
 * component state was silently destroyed by a click on another tab, and the person who had just
 * pasted a screenshot had to start again.
 */
export interface ComposerDraft {
  goal: string;
  imagePaths: string;
  pending: PendingAttachment[];
}

export const EMPTY_DRAFT: ComposerDraft = { goal: '', imagePaths: '', pending: [] };

/**
 * Unsent form state of a view, keyed by the view's own field names.
 *
 * Every view used to keep what the user had typed in component state, and App renders one view at
 * a time - so a click on another tab unmounted the form and threw the typing away. The drafts live
 * in the store instead: they survive a tab switch, and `persist` names the fields worth keeping
 * across a reload (a user id, a filter - never a secret, because this is localStorage).
 */
export interface ViewDrafts {
  sessions: { title: string; userId: string; renamingId: string | null; renameText: string };
  events: { search: string };
  capabilities: { q: string; tags: string; selected: string | null; input: string };
  approvals: { reasons: Record<string, string> };
  connection: { baseUrl: string; token: string };
}

export const EMPTY_VIEW_DRAFTS: ViewDrafts = {
  sessions: { title: '', userId: 'operator', renamingId: null, renameText: '' },
  events: { search: '' },
  capabilities: { q: '', tags: '', selected: null, input: '{}' },
  approvals: { reasons: {} },
  connection: { baseUrl: '', token: '' },
};

/**
 * The fields kept across a page reload, per view.
 *
 * Only low-risk values: a token or an API key must never be written to localStorage by this app.
 */
export const PERSISTED_DRAFT_FIELDS: { [K in keyof ViewDrafts]?: (keyof ViewDrafts[K])[] } = {
  sessions: ['userId'],
  events: ['search'],
  capabilities: ['q', 'tags', 'input'],
};

/**
 * Fold one upload batch's outcome into a draft's queue, by identity.
 *
 * Two drops can be in flight at once, and the one that finishes last must not overwrite what the
 * other already learned: a refusal would turn back into "ready", and a stored image would be sent
 * by id that the runtime has.
 */
export function settleAttachmentBatch(
  current: PendingAttachment[],
  batch: PendingAttachment[],
  uploaded: UploadedAttachment[],
  failed: { name: string; reason: string }[],
): PendingAttachment[] {
  return current.map((item) => {
    const index = batch.findIndex((candidate) => candidate.key === item.key);
    if (index < 0) return item;
    const stored = uploaded[index];
    if (stored !== undefined) {
      return { ...item, status: 'ready', artifactId: stored.artifact_id, name: stored.name };
    }
    const refusal = failed[index] ?? { name: item.name, reason: 'upload failed' };
    return { ...item, status: 'error', error: refusal.reason };
  });
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
  /** Every workspace, with the caller's own role on each. */
  workspaces: WorkspaceSummary[];
  /** Workspaces this caller has no role in: a name to ask about, never the member list. */
  discoverableWorkspaces: WorkspaceIndex[];
  workspacesError: ApiError | null;
  workspacesLoading: boolean;
  refreshWorkspaces: () => Promise<void>;
  createWorkspace: (directory: string, name: string) => Promise<string | null>;
  /** List the folders a workspace could be created in, one level at a time. */
  browseWorkspaceDirectories: (path?: string) => Promise<WorkspaceDirectoryListing | null>;
  renameWorkspace: (id: string, name: string) => Promise<boolean>;
  /**
   * The workspace this console is working in, or null for "all of them".
   *
   * A filter rather than a permission: the runtime decides what may be seen and done, and this only
   * decides which of those the session list puts in front of you.
   */
  selectedWorkspaceId: string | null;
  selectWorkspace: (id: string | null) => void;
  grantWorkspaceAccess: (id: string, userId: string, nodeId: string | null, role: string) => Promise<boolean>;
  revokeWorkspaceAccess: (id: string, userId: string, nodeId?: string | null) => Promise<boolean>;
  loadWorkspaceCapabilities: (id: string) => Promise<WorkspaceCapabilitiesResponse | null>;
  saveWorkspaceCapabilities: (id: string, allow: string[] | null) => Promise<boolean>;
  requestWorkspaceAccess: (id: string, role: string, note?: string) => Promise<boolean>;
  decideWorkspaceAccess: (id: string, requestId: string, approve: boolean, role?: string) => Promise<boolean>;
  /** The workspace records themselves, for a view that needs the full record (members). */
  workspaceById: (id: string | null) => WorkspaceRecord | null;
  /** Live answer text per run, replaced by the stored answer once the run finishes. */
  /**
   * The live preview of an answer still being written, per session.
   *
   * Keyed by session first because the socket carries every session's events: a flat map keyed by
   * run let one session's text render in another session's transcript - open B while A is streaming
   * and A's words appeared under B.
   */
  streamed: Map<string, Map<string, string>>;
  /** Capability calls waiting for a decision. */
  approvals: PendingApproval[];
  /** Ask the runtime what is waiting. The approvals view calls this when it opens. */
  refreshApprovals: () => Promise<void>;
  /** Provider names the runtime can route to, for the session's model picker. */
  modelOptions: string[];
  /** Change a session's title, provider or thinking effort. */
  configureSession: (
    id: string,
    patch: { title?: string; model?: string; effort?: string },
  ) => Promise<void>;
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
  /** The name this console declares on a node that has no token to identify it. */
  identityUser: string;
  identityNode: string;
  setIdentityUser: (next: string) => void;
  setIdentityNode: (next: string) => void;
  setAutoReconnect: (next: boolean) => void;
  connect: (overrides?: { baseUrl?: string; token?: string; user?: string; node?: string }) => Promise<void>;
  disconnect: () => void;
  /** Search term applied to the session list; empty means no filter. */
  sessionQuery: string;
  setSessionQuery: (next: string) => void;
  refreshSessions: () => Promise<void>;
  renameSession: (id: string, title: string) => Promise<void>;
  createSession: (title: string, userId: string, workspaceId?: string | null) => Promise<string | null>;
  selectSession: (id: string | null) => void;
  refreshDetail: (id?: string) => Promise<void>;
  closeSession: (id: string) => Promise<void>;
  /** Open a closed session again. */
  openSession: (id: string) => Promise<void>;
  /** Write a closed conversation into an archive package. */
  archiveSession: (id: string) => Promise<void>;
  /** What a session narrowed about capabilities, and what the runtime offers. */
  loadCapabilities: (id: string) => Promise<SessionCapabilitiesResponse | null>;
  saveCapabilities: (id: string, allow: string[] | null) => Promise<boolean>;
  /** Access requests: ask, list, decide. */
  loadAccessRequests: (id: string) => Promise<AccessRequestsResponse | null>;
  /** What is waiting on this person, across every session. The nav badge reads it. */
  accessInbox: AccessInboxResponse | null;
  refreshAccessInbox: () => Promise<void>;
  requestAccess: (id: string, role: string, note?: string) => Promise<boolean>;
  decideAccess: (id: string, requestId: string, approve: boolean, role?: string) => Promise<boolean>;
  /** The archive root's packages, newest first. Fetched when the archive view asks for them. */
  archives: ArchiveEntry[];
  archivesRoot: string;
  archivesEnabled: boolean;
  archivesLoading: boolean;
  archivesError: ApiError | null;
  refreshArchives: () => Promise<void>;
  getArchive: (id: string) => Promise<ArchiveDetail>;
  restoreArchive: (id: string, title?: string) => Promise<string | null>;
  deleteArchive: (id: string) => Promise<void>;
  sendGoal: (
    text: string,
    wait: boolean,
    images?: string[],
    attachments?: string[],
  ) => Promise<PostMessageResponse | null>;
  /**
   * Upload images and get back the attachments to name in the next goal.
   *
   * A browser cannot write into the runtime's workspace, so naming a path is not an option: the
   * bytes go to the runtime, which verifies them by content and hands back an artifact id.
   */
  uploadAttachments: (
    sessionId: string,
    files: File[],
  ) => Promise<{ uploaded: UploadedAttachment[]; failed: { name: string; reason: string }[] }>;
  /** The unsent goal, image paths and staged attachments, per session. Survives a tab switch. */
  composerDrafts: Record<string, ComposerDraft>;
  updateComposerDraft: (sessionId: string, patch: Partial<ComposerDraft>) => void;
  /** Unsent form state per view. A view reads its own entry; the store owns the lifetime. */
  viewDrafts: ViewDrafts;
  /** Patch one view's draft; string fields listed in PERSISTED_DRAFT_FIELDS also survive a reload. */
  updateViewDraft: <K extends keyof ViewDrafts>(view: K, patch: Partial<ViewDrafts[K]>) => void;
  /** Apply one upload batch's outcome to a session's queue, by attachment key. */
  settleAttachments: (
    sessionId: string,
    batch: PendingAttachment[],
    uploaded: UploadedAttachment[],
    failed: { name: string; reason: string }[],
  ) => void;
  clearComposerDraft: (sessionId: string) => void;
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
  const [identityUser, setIdentityUserState] = useState<string>(() => readStorage(IDENTITY_USER_KEY) ?? '');
  const [identityNode, setIdentityNodeState] = useState<string>(() => readStorage(IDENTITY_NODE_KEY) ?? '');

  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [meta, setMeta] = useState<RuntimeMeta | null>(null);
  const [connection, setConnection] = useState<ConnectionState>('disconnected');
  const [connectionError, setConnectionError] = useState<ApiError | null>(null);
  const [login, setLogin] = useState<LoginResponse | null>(null);

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [workspaces, setWorkspaces] = useState<WorkspaceSummary[]>([]);
  const [discoverableWorkspaces, setDiscoverableWorkspaces] = useState<WorkspaceIndex[]>([]);
  const [workspacesError, setWorkspacesError] = useState<ApiError | null>(null);
  const [workspacesLoading, setWorkspacesLoading] = useState(false);
  const [selectedWorkspaceId, setSelectedWorkspaceId] = useState<string | null>(
    () => readStorage(WORKSPACE_KEY),
  );
  const [sessionQuery, setSessionQueryState] = useState<string>('');
  // Live answer text per run, replaced by the stored answer when the run completes.
  const [streamed, setStreamed] = useState<Map<string, Map<string, string>>>(() => new Map());
  // Unsent composer state, per session. Kept above the views so a tab switch cannot throw away
  // what somebody was about to send.
  const [composerDrafts, setComposerDrafts] = useState<Record<string, ComposerDraft>>({});
  // View form drafts, same reason as the composer: a tab switch unmounts the view.
  const [viewDrafts, setViewDrafts] = useState<ViewDrafts>(() => {
    const initial: ViewDrafts = {
      sessions: { ...EMPTY_VIEW_DRAFTS.sessions },
      events: { ...EMPTY_VIEW_DRAFTS.events },
      capabilities: { ...EMPTY_VIEW_DRAFTS.capabilities },
      approvals: { ...EMPTY_VIEW_DRAFTS.approvals },
      connection: { ...EMPTY_VIEW_DRAFTS.connection },
    };
    for (const [view, fields] of Object.entries(PERSISTED_DRAFT_FIELDS) as [
      keyof ViewDrafts,
      string[],
    ][]) {
      for (const field of fields) {
        const stored = readStorage(`draft.${view}.${field}`);
        if (stored === null) continue;
        (initial[view] as Record<string, unknown>)[field] = stored;
      }
    }
    return initial;
  });
  const [approvals, setApprovals] = useState<PendingApproval[]>([]);
  const [approvalsUnsupported, setApprovalsUnsupported] = useState(false);
  const [modelOptions, setModelOptions] = useState<string[]>([]);
  const approvalsUnsupportedRef = useRef(false);

  const artifactUrl = useCallback(
    (id: string): string => clientRef.current.artifactUrl(id),
    [],
  );
  const [sessionsError, setSessionsError] = useState<ApiError | null>(null);
  const [sessionsLoading, setSessionsLoading] = useState(false);
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(null);
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [accessInbox, setAccessInbox] = useState<AccessInboxResponse | null>(null);
  const [archives, setArchives] = useState<ArchiveEntry[]>([]);
  const [archivesRoot, setArchivesRoot] = useState('');
  const [archivesEnabled, setArchivesEnabled] = useState(true);
  const [archivesLoading, setArchivesLoading] = useState(false);
  const [archivesError, setArchivesError] = useState<ApiError | null>(null);
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

  const client = useMemo(
    () => new AgentOsClient({ baseUrl, token, user: identityUser, node: identityNode }),
    [baseUrl, token, identityUser, identityNode],
  );

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
      // Attachments are rendered inside the turn that carried them, from the run record itself, so
      // there is nothing to collect here. Asking for a session-wide list used to show images only
      // (a document is not an image part) and never showed what belonged to which message.
    } catch (cause) {
      setDetailError(toApiError(cause));
    } finally {
      setDetailLoading(false);
    }
  }, []);

  // Nothing asks about approvals until somebody looks. An earlier version polled this route every
  // few seconds from the moment the page loaded, which against a runtime that predates the route
  // painted a 404 into the browser console on every single refresh.
  const refreshAccessInbox = useCallback(async (): Promise<void> => {
    try {
      setAccessInbox(await clientRef.current.accessInbox());
    } catch {
      // A runtime without the route (an older build) has no inbox rather than an error page: the
      // access view explains itself, and the badge simply shows nothing.
      setAccessInbox(null);
    }
  }, []);

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

  /**
   * Read the workspace list with a specific client, so `connect` can use the client it just built
   * rather than waiting a render for `clientRef` to catch up.
   */
  const loadWorkspaces = useCallback(async (active: AgentOsClient): Promise<void> => {
    setWorkspacesLoading(true);
    try {
      const response = await active.listWorkspaces();
      setWorkspaces(response.workspaces);
      setDiscoverableWorkspaces(response.discoverable ?? []);
      setWorkspacesError(null);
    } catch (cause) {
      setWorkspacesError(toApiError(cause));
    } finally {
      setWorkspacesLoading(false);
    }
  }, []);

  const configureSession = useCallback(
    async (id: string, patch: { title?: string; model?: string; effort?: string }): Promise<void> => {
      setActionError(null);
      try {
        await clientRef.current.configureSession(id, patch);
        await refreshDetail(id);
        await refreshSessions();
      } catch (cause) {
        setActionError(toApiError(cause));
      }
    },
    [refreshDetail, refreshSessions],
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
        // The preview of an answer still being written. Keyed by session and then by run: the socket
        // delivers every session's events, so a preview that is not bound to its session is a
        // preview that shows up in whatever transcript happens to be open.
        const payload = event.payload as { run_id?: string; text?: string } | null;
        const runId = payload?.run_id;
        const text = payload?.text;
        const sessionId = event.session_id;
        if (
          typeof sessionId === 'string' &&
          typeof runId === 'string' &&
          typeof text === 'string' &&
          text.length > 0
        ) {
          setStreamed((previous) => {
            const next = new Map(previous);
            const forSession = new Map(next.get(sessionId) ?? []);
            forSession.set(runId, (forSession.get(runId) ?? '') + text);
            next.set(sessionId, forSession);
            return next;
          });
        }
      }
      if (event.kind === 'run_completed' || event.kind === 'run_failed' || event.kind === 'run_cancelled') {
        const payload = event.payload as { run_id?: string } | null;
        const runId = payload?.run_id;
        const sessionId = event.session_id;
        if (typeof runId === 'string' && typeof sessionId === 'string') {
          // The stored answer replaces the preview the moment the run settles.
          setStreamed((previous) => {
            const forSession = previous.get(sessionId);
            if (forSession === undefined || !forSession.has(runId)) return previous;
            const next = new Map(previous);
            const trimmed = new Map(forSession);
            trimmed.delete(runId);
            if (trimmed.size === 0) next.delete(sessionId);
            else next.set(sessionId, trimmed);
            return next;
          });
        }
      }
      if (APPROVAL_KINDS.has(event.kind)) {
        void refreshApprovals();
      }
      // A request, or an answer to one, changes what is waiting on somebody: the inbox and the nav
      // badge are the same fact seen twice.
      if (
        event.kind === 'session_access_requested' ||
        event.kind === 'session_access_decided'
      ) {
        void refreshAccessInbox();
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
      // What is waiting on this person is part of being connected: the nav badge has to be right
      // before anybody opens the page that fills it.
      void refreshAccessInbox();
    } else {
      stream.setAutoReconnect(false);
    }
  }, [stream, connection, autoReconnect, refreshAccessInbox]);

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

  const setIdentityUser = useCallback((next: string): void => {
    setIdentityUserState(next);
    writeStorage(IDENTITY_USER_KEY, next.trim().length > 0 ? next.trim() : null);
  }, []);

  const setIdentityNode = useCallback((next: string): void => {
    setIdentityNodeState(next);
    writeStorage(IDENTITY_NODE_KEY, next.trim().length > 0 ? next.trim() : null);
  }, []);

  const setAutoReconnect = useCallback((next: boolean): void => {
    setAutoReconnectState(next);
    writeStorage(AUTO_RECONNECT_KEY, next ? 'true' : 'false');
  }, []);

  const connect = useCallback(async (overrides?: { baseUrl?: string; token?: string; user?: string; node?: string }): Promise<void> => {
    const activeBaseUrl = overrides?.baseUrl ?? baseUrl;
    const activeToken = overrides?.token ?? token;
    // An override exists so Connect can apply freshly typed values in the same click that reads them:
    // the state setter above has not re-rendered yet, and the closure would still hold the old name.
    const activeUser = overrides?.user ?? identityUser;
    const activeNode = overrides?.node ?? identityNode;
    setConnection('connecting');
    setConnectionError(null);
    const active = new AgentOsClient({
      baseUrl: activeBaseUrl,
      token: activeToken.length > 0 ? activeToken : null,
      user: activeUser,
      node: activeNode,
    });
    try {
      const healthResponse = await active.healthz();
      setHealth(healthResponse);
      const metaResponse = await active.meta();
      setMeta(metaResponse);
      // The provider list feeds the session's model picker. A runtime too old to answer is not an
      // error: the picker simply has nothing to offer and the router keeps deciding.
      try {
        const models = await active.listModels();
        setModelOptions(models.providers.map((provider) => provider.name));
      } catch {
        setModelOptions([]);
      }
      if (metaResponse.auth_required) {
        setLogin(await active.login(activeToken));
      } else {
        setLogin({ ok: true, auth_required: false, note: 'this node has no token configured' });
      }
      setConnection('online');
      await loadSessions(active);
      // Workspaces come with the connection: they are what the console groups sessions by, and a
      // list that arrives after the first paint would flash the wrong grouping.
      await loadWorkspaces(active);
    } catch (cause) {
      setConnectionError(toApiError(cause));
      setConnection('error');
    }
  }, [baseUrl, token, identityUser, identityNode, loadSessions, loadWorkspaces]);

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
    async (title: string, userId: string, workspaceId?: string | null): Promise<string | null> => {
      setBusy(true);
      setActionError(null);
      try {
        const trimmedUser = userId.trim().length > 0 ? userId.trim() : 'anonymous';
        const trimmedTitle = title.trim().length > 0 ? title.trim() : 'untitled session';
        // Which workspace was chosen, or null for "the caller's own default". The runtime creates
        // that default on first use, so a client that has never heard of workspaces still gets one.
        const target = workspaceId === undefined ? selectedWorkspaceId : workspaceId;
        const record = await clientRef.current.createSession(trimmedUser, trimmedTitle, target);
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
    [refreshDetail, refreshSessions, selectedWorkspaceId],
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

  // Closing and opening are a pair now: closing stops the actor, and the record, the runs and the
  // transcript stay. Selecting a different session afterwards is not needed any more, because the
  // session is still there to look at.
  const setSessionOpen = useCallback(
    async (id: string, open: boolean): Promise<void> => {
      setBusy(true);
      setActionError(null);
      try {
        if (open) {
          await clientRef.current.openSession(id);
        } else {
          await clientRef.current.closeSession(id);
        }
        await refreshSessions();
        if (selectedRef.current === id) {
          await refreshDetail(id);
        }
      } catch (cause) {
        setActionError(toApiError(cause));
      } finally {
        setBusy(false);
      }
    },
    [refreshDetail, refreshSessions],
  );

  const closeSession = useCallback(
    async (id: string): Promise<void> => setSessionOpen(id, false),
    [setSessionOpen],
  );

  const openSession = useCallback(
    async (id: string): Promise<void> => setSessionOpen(id, true),
    [setSessionOpen],
  );

  const refreshArchives = useCallback(async (): Promise<void> => {
    setArchivesLoading(true);
    setArchivesError(null);
    try {
      const response = await clientRef.current.listArchives();
      setArchives(response.archives);
      setArchivesRoot(response.root);
      setArchivesEnabled(response.enabled);
    } catch (cause) {
      // A runtime without the route (an older build) answers 404: an empty archive, said plainly,
      // rather than an error banner on a page that is simply not supported there.
      setArchivesError(toApiError(cause));
      setArchives([]);
    } finally {
      setArchivesLoading(false);
    }
  }, []);

  const archiveSession = useCallback(
    async (id: string): Promise<void> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.archiveSession(id);
        await refreshSessions();
        await refreshArchives();
        if (selectedRef.current === id) {
          await refreshDetail(id);
        }
      } catch (cause) {
        setActionError(toApiError(cause));
      } finally {
        setBusy(false);
      }
    },
    [refreshArchives, refreshDetail, refreshSessions],
  );

  const restoreArchive = useCallback(
    async (id: string, title?: string): Promise<string | null> => {
      setBusy(true);
      setActionError(null);
      try {
        const result = await clientRef.current.restoreArchive(id, title);
        await refreshSessions();
        await refreshArchives();
        setSelectedSessionId(result.session.id);
        await refreshDetail(result.session.id);
        return result.session.id;
      } catch (cause) {
        setActionError(toApiError(cause));
        return null;
      } finally {
        setBusy(false);
      }
    },
    [refreshArchives, refreshDetail, refreshSessions],
  );

  const deleteArchive = useCallback(
    async (id: string): Promise<void> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.deleteArchive(id);
        await refreshArchives();
      } catch (cause) {
        setActionError(toApiError(cause));
      } finally {
        setBusy(false);
      }
    },
    [refreshArchives],
  );

  const getArchive = useCallback(
    async (id: string): Promise<ArchiveDetail> => clientRef.current.getArchive(id),
    [],
  );

  // ------------------------------------------------------------------ workspaces

  const refreshWorkspaces = useCallback(
    (): Promise<void> => loadWorkspaces(clientRef.current),
    [loadWorkspaces],
  );

  const selectWorkspace = useCallback((id: string | null): void => {
    setSelectedWorkspaceId(id);
    writeStorage(WORKSPACE_KEY, id);
  }, []);

  const createWorkspace = useCallback(
    async (directory: string, name: string): Promise<string | null> => {
      setBusy(true);
      setActionError(null);
      try {
        const response = await clientRef.current.createWorkspace(directory.trim(), name);
        await refreshWorkspaces();
        // Creating one is also choosing it: a workspace you just made is where you are working.
        selectWorkspace(response.workspace.id);
        return response.workspace.id;
      } catch (cause) {
        setActionError(toApiError(cause));
        return null;
      } finally {
        setBusy(false);
      }
    },
    [refreshWorkspaces, selectWorkspace],
  );

  const browseWorkspaceDirectories = useCallback(
    async (path?: string): Promise<WorkspaceDirectoryListing | null> => {
      try {
        return await clientRef.current.browseWorkspaceDirectories(path);
      } catch (cause) {
        setActionError(toApiError(cause));
        return null;
      }
    },
    [],
  );

  const renameWorkspace = useCallback(
    async (id: string, name: string): Promise<boolean> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.renameWorkspace(id, name.trim());
        await refreshWorkspaces();
        return true;
      } catch (cause) {
        setActionError(toApiError(cause));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [refreshWorkspaces],
  );

  const grantWorkspaceAccess = useCallback(
    async (id: string, userId: string, nodeId: string | null, role: string): Promise<boolean> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.grantWorkspaceAccess(id, userId.trim(), nodeId, role);
        // A role on a workspace is a role in every session of it, so the session list moves too.
        await refreshWorkspaces();
        await refreshSessions();
        return true;
      } catch (cause) {
        setActionError(toApiError(cause));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [refreshSessions, refreshWorkspaces],
  );

  const revokeWorkspaceAccess = useCallback(
    async (id: string, userId: string, nodeId?: string | null): Promise<boolean> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.revokeWorkspaceAccess(id, userId.trim(), nodeId);
        await refreshWorkspaces();
        await refreshSessions();
        return true;
      } catch (cause) {
        setActionError(toApiError(cause));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [refreshSessions, refreshWorkspaces],
  );

  const loadWorkspaceCapabilities = useCallback(
    async (id: string): Promise<WorkspaceCapabilitiesResponse | null> => {
      try {
        return await clientRef.current.workspaceCapabilities(id);
      } catch (cause) {
        setActionError(toApiError(cause));
        return null;
      }
    },
    [],
  );

  const saveWorkspaceCapabilities = useCallback(
    async (id: string, allow: string[] | null): Promise<boolean> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.setWorkspaceCapabilities(id, { allow });
        return true;
      } catch (cause) {
        setActionError(toApiError(cause));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [],
  );

  const requestWorkspaceAccess = useCallback(
    async (id: string, role: string, note?: string): Promise<boolean> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.requestWorkspaceAccess(id, role, note);
        return true;
      } catch (cause) {
        setActionError(toApiError(cause));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [],
  );

  const decideWorkspaceAccess = useCallback(
    async (id: string, requestId: string, approve: boolean, role?: string): Promise<boolean> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.decideWorkspaceAccess(id, requestId, approve, role);
        // An approval changes who may do what, everywhere in the workspace.
        await refreshWorkspaces();
        await refreshSessions();
        return true;
      } catch (cause) {
        setActionError(toApiError(cause));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [refreshSessions, refreshWorkspaces],
  );

  const workspaceById = useCallback(
    (id: string | null): WorkspaceRecord | null => {
      if (id === null) return null;
      return workspaces.find((entry) => entry.workspace.id === id)?.workspace ?? null;
    },
    [workspaces],
  );

  const loadCapabilities = useCallback(
    async (id: string): Promise<SessionCapabilitiesResponse | null> => {
      try {
        return await clientRef.current.sessionCapabilities(id);
      } catch (cause) {
        setActionError(toApiError(cause));
        return null;
      }
    },
    [],
  );

  const saveCapabilities = useCallback(
    async (id: string, allow: string[] | null): Promise<boolean> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.setSessionCapabilities(id, { allow });
        return true;
      } catch (cause) {
        setActionError(toApiError(cause));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [],
  );

  const loadAccessRequests = useCallback(
    async (id: string): Promise<AccessRequestsResponse | null> => {
      try {
        return await clientRef.current.accessRequests(id);
      } catch (cause) {
        setActionError(toApiError(cause));
        return null;
      }
    },
    [],
  );

  const requestAccess = useCallback(
    async (id: string, role: string, note?: string): Promise<boolean> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.requestAccess(id, role, note);
        return true;
      } catch (cause) {
        setActionError(toApiError(cause));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [],
  );

  const decideAccess = useCallback(
    async (id: string, requestId: string, approve: boolean, role?: string): Promise<boolean> => {
      setBusy(true);
      setActionError(null);
      try {
        await clientRef.current.decideAccess(id, requestId, approve, role);
        // An approval changes who may do what, so the list and the detail both move.
        await refreshSessions();
        if (selectedRef.current === id) {
          await refreshDetail(id);
        }
        return true;
      } catch (cause) {
        setActionError(toApiError(cause));
        return false;
      } finally {
        setBusy(false);
      }
    },
    [refreshDetail, refreshSessions],
  );

  const sendGoal = useCallback(
    async (
      text: string,
      wait: boolean,
      images: string[] = [],
      attachments: string[] = [],
    ): Promise<PostMessageResponse | null> => {
      const id = selectedRef.current;
      setActionError(null);
      if (id === null) {
        setActionError(new ApiError({ code: 'no_session', message: tGlobal('error.noSessionOrCreate') }));
        return null;
      }
      setBusy(true);
      try {
        const response = await clientRef.current.postMessage(
          id,
          text,
          wait,
          images,
          undefined,
          undefined,
          // The staged uploads, by artifact id. Dropping this argument silently sends the goal
          // without its files: the model then answers about an attachment it never received.
          attachments,
        );
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

  /**
   * Upload files one at a time and report per file what happened.
   *
   * One failure must not lose the others: a user who dropped four screenshots and has one
   * unsupported file should still be able to send the three that worked.
   */
  const uploadAttachments = useCallback(
    async (
      sessionId: string,
      files: File[],
    ): Promise<{ uploaded: UploadedAttachment[]; failed: { name: string; reason: string }[] }> => {
      const uploaded: UploadedAttachment[] = [];
      const failed: { name: string; reason: string }[] = [];
      for (const file of files) {
        try {
          const record = await clientRef.current.upload(sessionId, file);
          uploaded.push({
            artifact_id: record.artifact_id,
            name: record.name,
            content_type: record.content_type,
            bytes: record.bytes,
          });
        } catch (cause) {
          const error = toApiError(cause);
          failed.push({ name: file.name, reason: error.message });
        }
      }
      return { uploaded, failed };
    },
    [],
  );

  /**
   * Patch one view's draft. Persisted fields are also written to localStorage, so a reload keeps
   * them too - the tab switch and the reload were the same complaint from different angles.
   */
  const updateViewDraft = useCallback(
    <K extends keyof ViewDrafts>(view: K, patch: Partial<ViewDrafts[K]>): void => {
      setViewDrafts((current) => ({ ...current, [view]: { ...current[view], ...patch } }));
      const persisted = (PERSISTED_DRAFT_FIELDS[view] ?? []) as string[];
      for (const [field, value] of Object.entries(patch)) {
        if (!persisted.includes(field)) continue;
        if (typeof value === 'string') writeStorage(`draft.${view}.${field}`, value);
      }
    },
    [],
  );

  const updateComposerDraft = useCallback(
    (sessionId: string, patch: Partial<ComposerDraft>): void => {
      if (sessionId.length === 0) return;
      setComposerDrafts((current) => ({
        ...current,
        [sessionId]: { ...(current[sessionId] ?? EMPTY_DRAFT), ...patch },
      }));
    },
    [],
  );

  const settleAttachments = useCallback(
    (
      sessionId: string,
      batch: PendingAttachment[],
      uploaded: UploadedAttachment[],
      failed: { name: string; reason: string }[],
    ): void => {
      setComposerDrafts((current) => {
        const draft = current[sessionId];
        if (draft === undefined) return current;
        return { ...current, [sessionId]: { ...draft, pending: settleAttachmentBatch(draft.pending, batch, uploaded, failed) } };
      });
    },
    [],
  );

  const clearComposerDraft = useCallback((sessionId: string): void => {
    setComposerDrafts((current) => {
      const draft = current[sessionId];
      // Preview URLs are object URLs; dropping the entry without revoking them leaks the blobs for
      // the lifetime of the page.
      if (draft !== undefined) {
        for (const item of draft.pending) {
          if (item.previewUrl !== null) URL.revokeObjectURL(item.previewUrl);
        }
      }
      const next = { ...current };
      delete next[sessionId];
      return next;
    });
  }, []);

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
      identityUser,
      identityNode,
      health,
      meta,
      connection,
      connectionError,
      login,
      client,
      sessions,
      sessionsError,
      sessionsLoading,
      workspaces,
      discoverableWorkspaces,
      workspacesError,
      workspacesLoading,
      refreshWorkspaces,
      createWorkspace,
      browseWorkspaceDirectories,
      renameWorkspace,
      selectedWorkspaceId,
      selectWorkspace,
      grantWorkspaceAccess,
      revokeWorkspaceAccess,
      loadWorkspaceCapabilities,
      saveWorkspaceCapabilities,
      requestWorkspaceAccess,
      decideWorkspaceAccess,
      workspaceById,
      streamed,
      approvals,
      approvalsUnsupported,
      refreshApprovals,
      decideApproval,
      modelOptions,
      configureSession,
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
      setIdentityUser,
      setIdentityNode,
      setAutoReconnect,
      connect,
      disconnect,
      refreshSessions,
      createSession,
      selectSession,
      refreshDetail,
      closeSession,
      openSession,
      archiveSession,
      archives,
      archivesRoot,
      archivesEnabled,
      archivesLoading,
      archivesError,
      refreshArchives,
      getArchive,
      restoreArchive,
      deleteArchive,
      accessInbox,
      refreshAccessInbox,
      loadCapabilities,
      saveCapabilities,
      loadAccessRequests,
      requestAccess,
      decideAccess,
      sendGoal,
      uploadAttachments,
      composerDrafts,
      updateComposerDraft,
      viewDrafts,
      updateViewDraft,
      settleAttachments,
      clearComposerDraft,
      cancelRun,
      clearEvents,
      clearActionError,
      pingSocket,
      reconnectSocket,
    }),
    [
      baseUrl,
      token,
      identityUser,
      identityNode,
      health,
      meta,
      connection,
      connectionError,
      login,
      client,
      sessions,
      sessionsError,
      sessionsLoading,
      workspaces,
      discoverableWorkspaces,
      workspacesError,
      workspacesLoading,
      refreshWorkspaces,
      createWorkspace,
      browseWorkspaceDirectories,
      renameWorkspace,
      selectedWorkspaceId,
      selectWorkspace,
      grantWorkspaceAccess,
      revokeWorkspaceAccess,
      loadWorkspaceCapabilities,
      saveWorkspaceCapabilities,
      requestWorkspaceAccess,
      decideWorkspaceAccess,
      workspaceById,
      streamed,
      approvals,
      approvalsUnsupported,
      refreshApprovals,
      decideApproval,
      modelOptions,
      configureSession,
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
      setIdentityUser,
      setIdentityNode,
      setAutoReconnect,
      connect,
      disconnect,
      refreshSessions,
      createSession,
      selectSession,
      refreshDetail,
      closeSession,
      openSession,
      archiveSession,
      archives,
      archivesRoot,
      archivesEnabled,
      archivesLoading,
      archivesError,
      refreshArchives,
      getArchive,
      restoreArchive,
      deleteArchive,
      accessInbox,
      refreshAccessInbox,
      loadCapabilities,
      saveCapabilities,
      loadAccessRequests,
      requestAccess,
      decideAccess,
      sendGoal,
      uploadAttachments,
      composerDrafts,
      updateComposerDraft,
      viewDrafts,
      updateViewDraft,
      settleAttachments,
      clearComposerDraft,
      cancelRun,
      clearEvents,
      clearActionError,
      pingSocket,
      reconnectSocket,
      openSession,
      accessInbox,
      refreshAccessInbox,
    ],
  );

  return <AppContext.Provider value={value}>{children}</AppContext.Provider>;
}