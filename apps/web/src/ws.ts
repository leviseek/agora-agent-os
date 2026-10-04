/**
 * agentos web client - the live event socket.
 *
 * The gateway multiplexes both directions on one socket: it streams EventRecords and
 * accepts goal/cancel/snapshot/health/ping commands. This module owns the socket
 * lifecycle: connect, auto-reconnect with backoff, and a small listener API so React
 * components subscribe without touching WebSocket directly.
 */

import type { EventRecord } from './api';

export type WsStatus = 'idle' | 'connecting' | 'open' | 'reconnecting' | 'closed';

export type WsCommand =
  | { type: 'ping' }
  | { type: 'goal'; session_id: string; goal: string; wait?: boolean }
  | { type: 'cancel'; session_id: string }
  | { type: 'snapshot'; session_id: string }
  | { type: 'health' };

export interface ServerMessage {
  type: string;
  event?: EventRecord;
  [key: string]: unknown;
}

export interface EventStreamOptions {
  /** Resolved on every connect attempt so a changed base URL takes effect on reconnect. */
  url: () => string;
  autoReconnect?: boolean;
  minDelayMs?: number;
  maxDelayMs?: number;
}

export type EventListener = (event: EventRecord) => void;
export type MessageListener = (message: ServerMessage) => void;
/**
 * A status detail is stored as data, never as a formatted sentence: the socket closes once, but
 * the user may switch language afterwards, and the line must follow the switch like any other
 * text. `text` carries server-provided strings that are not ours to translate.
 */
export type WsDetail = { key: string; params?: Record<string, string | number> } | { text: string };

export type StatusListener = (status: WsStatus, detail: WsDetail | null) => void;

export class EventStream {
  private socket: WebSocket | null = null;
  private timer: number | null = null;
  private attempt = 0;
  private closedByUser = false;
  private autoReconnect: boolean;

  private readonly options: EventStreamOptions;
  private readonly eventListeners = new Set<EventListener>();
  private readonly messageListeners = new Set<MessageListener>();
  private readonly statusListeners = new Set<StatusListener>();

  status: WsStatus = 'idle';
  detail: WsDetail | null = null;

  constructor(options: EventStreamOptions) {
    this.options = options;
    this.autoReconnect = options.autoReconnect ?? true;
  }

  // --- listeners ---------------------------------------------------------------------------

  onEvent(listener: EventListener): () => void {
    this.eventListeners.add(listener);
    return () => {
      this.eventListeners.delete(listener);
    };
  }

  onMessage(listener: MessageListener): () => void {
    this.messageListeners.add(listener);
    return () => {
      this.messageListeners.delete(listener);
    };
  }

  onStatus(listener: StatusListener): () => void {
    this.statusListeners.add(listener);
    listener(this.status, this.detail);
    return () => {
      this.statusListeners.delete(listener);
    };
  }

  private setStatus(status: WsStatus, detail: WsDetail | null): void {
    this.status = status;
    this.detail = detail;
    for (const listener of this.statusListeners) listener(status, detail);
  }

  // --- lifecycle ---------------------------------------------------------------------------

  connect(): void {
    this.closedByUser = false;
    if (this.socket !== null) {
      const state = this.socket.readyState;
      if (state === WebSocket.OPEN || state === WebSocket.CONNECTING) return;
    }
    this.open();
  }

  private open(): void {
    this.clearTimer();
    let url: string;
    try {
      url = this.options.url();
    } catch (error) {
      this.setStatus('closed', { text: error instanceof Error ? error.message : String(error) });
      return;
    }

    this.setStatus(this.attempt === 0 ? 'connecting' : 'reconnecting', { text: url });
    let socket: WebSocket;
    try {
      socket = new WebSocket(url);
    } catch (error) {
      this.setStatus('closed', { text: error instanceof Error ? error.message : String(error) });
      this.scheduleReconnect();
      return;
    }
    this.socket = socket;

    socket.onopen = () => {
      this.attempt = 0;
      this.setStatus('open', { text: url });
    };

    socket.onmessage = (message: MessageEvent<unknown>) => {
      if (typeof message.data !== 'string') return;
      let parsed: unknown;
      try {
        parsed = JSON.parse(message.data);
      } catch {
        return;
      }
      if (typeof parsed !== 'object' || parsed === null) return;
      const record = parsed as ServerMessage;
      if (typeof record.type !== 'string') return;

      if (record.type === 'event' && record.event !== undefined) {
        for (const listener of this.eventListeners) listener(record.event);
      }
      for (const listener of this.messageListeners) listener(record);
    };

    socket.onerror = () => {
      // onclose always follows; the detail is recorded there.
    };

    socket.onclose = (event: CloseEvent) => {
      if (this.socket === socket) this.socket = null;
      const detail: WsDetail =
        event.reason.length > 0 ? { text: event.reason } : { key: 'ws.closedWithCode', params: { code: event.code } };
      this.setStatus('closed', detail);
      this.scheduleReconnect();
    };
  }

  private scheduleReconnect(): void {
    if (this.closedByUser || !this.autoReconnect) return;
    const min = this.options.minDelayMs ?? 500;
    const max = this.options.maxDelayMs ?? 10_000;
    const base = Math.min(max, min * Math.pow(2, this.attempt));
    const delay = Math.round(base * (0.8 + Math.random() * 0.4));
    this.attempt += 1;
    this.setStatus('reconnecting', { key: 'ws.retrying', params: { delay, n: this.attempt } });
    this.clearTimer();
    this.timer = window.setTimeout(() => {
      this.timer = null;
      this.open();
    }, delay);
  }

  private clearTimer(): void {
    if (this.timer !== null) {
      window.clearTimeout(this.timer);
      this.timer = null;
    }
  }

  close(): void {
    this.closedByUser = true;
    this.clearTimer();
    const socket = this.socket;
    this.socket = null;
    if (socket !== null) {
      socket.onclose = null;
      socket.onmessage = null;
      socket.onerror = null;
      socket.onopen = null;
      try {
        socket.close();
      } catch {
        // already gone
      }
    }
    this.setStatus('closed', { key: 'ws.closedByClient' });
  }

  setAutoReconnect(enabled: boolean): void {
    this.autoReconnect = enabled;
    if (enabled) {
      this.connect();
    } else if (this.timer !== null) {
      this.clearTimer();
      this.setStatus('closed', { key: 'ws.autoReconnectOff' });
    }
  }

  isAutoReconnect(): boolean {
    return this.autoReconnect;
  }

  /** Returns false when the socket is not open; callers surface that to the user. */
  send(command: WsCommand): boolean {
    const socket = this.socket;
    if (socket === null || socket.readyState !== WebSocket.OPEN) return false;
    socket.send(JSON.stringify(command));
    return true;
  }

  ping(): boolean {
    return this.send({ type: 'ping' });
  }

  requestHealth(): boolean {
    return this.send({ type: 'health' });
  }

  requestSnapshot(sessionId: string): boolean {
    return this.send({ type: 'snapshot', session_id: sessionId });
  }

  sendGoal(sessionId: string, goal: string, wait = false): boolean {
    return this.send({ type: 'goal', session_id: sessionId, goal, wait });
  }

  cancel(sessionId: string): boolean {
    return this.send({ type: 'cancel', session_id: sessionId });
  }
}