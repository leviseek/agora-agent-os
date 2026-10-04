import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { DependencyList } from 'react';
import { toApiError } from './api';
import type { ApiError, EventRecord } from './api';
import { useApp } from './store';

export interface AsyncState<T> {
  data: T | null;
  error: ApiError | null;
  loading: boolean;
  reload: () => void;
}

/**
 * Run an async loader on mount and whenever its dependencies change.
 * The latest loader is used without re-running the effect, so inline closures are safe.
 */
export function useAsyncData<T>(
  loader: () => Promise<T>,
  deps: DependencyList,
  options?: { intervalMs?: number },
): AsyncState<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [loading, setLoading] = useState(true);
  const [tick, setTick] = useState(0);
  const loaderRef = useRef(loader);
  loaderRef.current = loader;

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    loaderRef.current().then(
      (value) => {
        if (cancelled) return;
        setData(value);
        setError(null);
        setLoading(false);
      },
      (cause: unknown) => {
        if (cancelled) return;
        setError(toApiError(cause));
        setLoading(false);
      },
    );
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, tick]);

  const intervalMs = options?.intervalMs;
  useEffect(() => {
    if (intervalMs === undefined || intervalMs <= 0) return;
    const id = window.setInterval(() => setTick((value) => value + 1), intervalMs);
    return () => window.clearInterval(id);
  }, [intervalMs]);

  const reload = useCallback(() => setTick((value) => value + 1), []);
  return { data, error, loading, reload };
}

/**
 * Session events = the durable log from /v1/sessions/{id}/events merged with the live
 * socket feed, de-duplicated by event id and ordered by the runtime sequence number.
 */
export function useSessionEvents(sessionId: string | null, limit = 200): AsyncState<EventRecord[]> {
  const { client, events: liveEvents } = useApp();

  const history = useAsyncData<EventRecord[]>(
    () => {
      if (sessionId === null) return Promise.resolve<EventRecord[]>([]);
      return client.sessionEvents(sessionId, limit).then((response) => response.events);
    },
    [client, sessionId, limit],
  );

  const merged = useMemo(() => {
    const byId = new Map<string, EventRecord>();
    for (const event of history.data ?? []) byId.set(event.id, event);
    for (const event of liveEvents) {
      if (event.session_id === sessionId) byId.set(event.id, event);
    }
    return Array.from(byId.values()).sort((a, b) => a.seq - b.seq);
  }, [history.data, liveEvents, sessionId]);

  return {
    data: merged,
    error: history.error,
    loading: history.loading,
    reload: history.reload,
  };
}

/** Merge live and historical events for the whole node (used by the events log view). */
export function useAllEvents(limit = 500): AsyncState<EventRecord[]> {
  const { client, events: liveEvents } = useApp();
  const history = useAsyncData<EventRecord[]>(
    () => client.listEvents({ limit }).then((response) => response.events),
    [client, limit],
  );

  const merged = useMemo(() => {
    const byId = new Map<string, EventRecord>();
    for (const event of history.data ?? []) byId.set(event.id, event);
    for (const event of liveEvents) byId.set(event.id, event);
    return Array.from(byId.values()).sort((a, b) => a.seq - b.seq);
  }, [history.data, liveEvents]);

  return { data: merged, error: history.error, loading: history.loading, reload: history.reload };
}

/** True while any run of the session is still executing, derived from the socket status. */
export function useSocketAlive(): boolean {
  const { wsStatus } = useApp();
  return wsStatus === 'open';
}
