/** View 8 - Events: the live socket log with kind/severity filters, pause and clear. */

import { Fragment, useMemo, useState } from 'react';
import { ApiErrorBanner, Badge, EmptyState, JsonBlock, Panel } from '../components';
import type { Tone } from '../components';
import { formatTime, severityRank } from '../format';
import { useAllEvents } from '../hooks';
import { useI18n } from '../i18n';
import { useApp } from '../store';
import type { EventRecord } from '../api';

/** The runtime event vocabulary (agentos-core EventKind), used to build the filter chips. */
const EVENT_KINDS: string[] = [
  'session_created',
  'session_closed',
  'session_message_queued',
  'session_message_handled',
  'actor_spawned',
  'actor_stopped',
  'actor_restarted',
  'actor_migrated',
  'actor_cloned',
  'snapshot_created',
  'snapshot_restored',
  'event_replayed',
  'run_created',
  'run_completed',
  'run_failed',
  'agent_step',
  'model_call',
  'model_result',
  'tool_call',
  'tool_result',
  'task_created',
  'task_queued',
  'task_started',
  'task_retrying',
  'task_completed',
  'task_failed',
  'task_cancelled',
  'capability_registered',
  'capability_invoked',
  'capability_denied',
  'worker_registered',
  'worker_heartbeat',
  'worker_offline',
  'artifact_created',
  'memory_written',
  'policy_denied',
  'error',
];

/** The list stays responsive by rendering only the newest slice of the filtered set. */
const RENDER_LIMIT = 400;

type SeverityFilter = 'all' | 'debug' | 'info' | 'warn' | 'error';

function severityTone(severity: string): Tone {
  switch (severity) {
    case 'error':
      return 'error';
    case 'warn':
      return 'warn';
    case 'debug':
      return 'muted';
    default:
      return 'info';
  }
}

export function EventsView() {
  const { t, tSeverity } = useI18n();
  const {
    clearEvents,
    selectedSessionId,
    autoReconnect,
    setAutoReconnect,
    pingSocket,
    wsStatus,
    viewDrafts,
    updateViewDraft,
  } = useApp();
  const all = useAllEvents(400);

  const [paused, setPaused] = useState(false);
  const [frozen, setFrozen] = useState<EventRecord[]>([]);
  const [kinds, setKinds] = useState<string[]>([]);
  const [severity, setSeverity] = useState<SeverityFilter>('all');
  // The filter lives in the store, so switching tabs does not clear what somebody was looking for.
  const { search } = viewDrafts.events;
  const setSearch = (next: string): void => updateViewDraft('events', { search: next });
  const [sessionOnly, setSessionOnly] = useState(false);
  const [expandedId, setExpandedId] = useState<string | null>(null);

  const source = paused ? frozen : (all.data ?? []);

  const filtered = useMemo(() => {
    const needle = search.trim().toLowerCase();
    const minRank = severity === 'all' ? 0 : severityRank(severity);
    return source.filter((event) => {
      if (kinds.length > 0 && !kinds.includes(event.kind)) return false;
      if (severity !== 'all' && severityRank(event.severity) < minRank) return false;
      if (sessionOnly && (selectedSessionId === null || event.session_id !== selectedSessionId)) return false;
      if (needle.length > 0) {
        const haystack = (event.kind + ' ' + event.message + ' ' + JSON.stringify(event.payload)).toLowerCase();
        if (!haystack.includes(needle)) return false;
      }
      return true;
    });
  }, [source, kinds, severity, search, sessionOnly, selectedSessionId]);

  const visible = useMemo(() => filtered.slice(-RENDER_LIMIT).reverse(), [filtered]);

  const toggleKind = (kind: string): void => {
    setKinds((previous) => (previous.includes(kind) ? previous.filter((item) => item !== kind) : [...previous, kind]));
  };

  const onTogglePause = (): void => {
    if (paused) {
      setPaused(false);
      setFrozen([]);
      return;
    }
    setFrozen(all.data ?? []);
    setPaused(true);
  };

  const onClear = (): void => {
    clearEvents();
    setFrozen([]);
  };

  return (
    <div className="stack">
      <Panel
        title={t('events.title')}
        subtitle={
          paused
            ? t('events.subtitlePaused', { source: source.length, limit: RENDER_LIMIT })
            : t('events.subtitleLive', { source: source.length, limit: RENDER_LIMIT })
        }
        actions={
          <>
            <Badge tone={wsStatus === 'open' ? 'ok' : wsStatus === 'reconnecting' ? 'warn' : 'error'}>
              {'ws ' + t('ws.status.' + wsStatus)}
            </Badge>
            <Badge tone="muted">{t('events.count', { n: filtered.length })}</Badge>
            <button type="button" className="btn btn-small" onClick={onTogglePause}>
              {paused ? t('events.resume') : t('events.pause')}
            </button>
            <button type="button" className="btn btn-ghost btn-small" onClick={onClear}>
              {t('events.clear')}
            </button>
            <button type="button" className="btn btn-ghost btn-small" onClick={pingSocket}>
              {t('events.ping')}
            </button>
            <button type="button" className="btn btn-ghost btn-small" onClick={all.reload}>
              {all.loading ? t('common.reloading') : t('events.reloadHistory')}
            </button>
          </>
        }
        flush
      >
        <ApiErrorBanner error={all.error} scope="GET /v1/events" onRetry={all.reload} />

        <div className="filter-bar filter-bar-wrap">
          <label className="checkbox">
            <input
              type="checkbox"
              checked={autoReconnect}
              onChange={(event) => setAutoReconnect(event.target.checked)}
            />
            <span>{t('events.autoReconnect')}</span>
          </label>
          <label className="checkbox">
            <input type="checkbox" checked={sessionOnly} onChange={(event) => setSessionOnly(event.target.checked)} />
            <span>{t('events.sessionOnly')}</span>
          </label>
          <label className="field field-inline-label">
            <span>{t('events.filterSeverity')}</span>
            <select value={severity} onChange={(event) => setSeverity(event.target.value as SeverityFilter)}>
              <option value="all">{t('common.all')}</option>
              <option value="debug">{tSeverity('debug') + '+'}</option>
              <option value="info">{tSeverity('info') + '+'}</option>
              <option value="warn">{tSeverity('warn') + '+'}</option>
              <option value="error">{tSeverity('error')}</option>
            </select>
          </label>
          <label className="field field-grow">
            <span>{t('common.search')}</span>
            <input
              type="text"
              value={search}
              placeholder={t('events.searchPlaceholder')}
              onChange={(event) => setSearch(event.target.value)}
            />
          </label>
        </div>

        <div className="chips chips-scroll">
          <button
            type="button"
            className={kinds.length === 0 ? 'chip chip-active' : 'chip'}
            onClick={() => setKinds([])}
          >
            {t('events.allKinds')}
          </button>
          {EVENT_KINDS.map((kind) => (
            <button
              key={kind}
              type="button"
              className={kinds.includes(kind) ? 'chip chip-active' : 'chip'}
              onClick={() => toggleKind(kind)}
            >
              {kind}
            </button>
          ))}
        </div>

        {visible.length === 0 ? (
          <EmptyState title={t('events.noMatchTitle')} hint={t('events.noMatchHint')} />
        ) : (
          <table className="table table-events">
            <thead>
              <tr>
                <th>{t('events.seq')}</th>
                <th>{t('events.time')}</th>
                <th>{t('events.severity')}</th>
                <th>{t('events.kind')}</th>
                <th>{t('events.message')}</th>
                <th>{t('common.session')}</th>
              </tr>
            </thead>
            <tbody>
              {visible.map((event) => (
                <Fragment key={event.id}>
                  <tr
                    className={expandedId === event.id ? 'row-selected' : undefined}
                    onClick={() => setExpandedId(expandedId === event.id ? null : event.id)}
                  >
                    <td>{event.seq}</td>
                    <td>{formatTime(event.ts)}</td>
                    <td>
                      <Badge tone={severityTone(event.severity)}>{tSeverity(event.severity)}</Badge>
                    </td>
                    <td>
                      <span className="kind">{event.kind}</span>
                    </td>
                    <td>{event.message}</td>
                    <td className="cell-clip">{event.session_id ?? '--'}</td>
                  </tr>
                  {expandedId === event.id ? (
                    <tr className="row-detail">
                      <td colSpan={6}>
                        <JsonBlock value={event.payload} empty={t('events.noPayload')} />
                      </td>
                    </tr>
                  ) : null}
                </Fragment>
              ))}
            </tbody>
          </table>
        )}
      </Panel>
    </div>
  );
}
