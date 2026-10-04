/** View 4 - Agent state: the selected run, its model, step timeline, answer and error. */

import { useMemo, useState } from 'react';
import { ApiErrorBanner, Badge, EmptyState, JsonBlock, KeyValue, Loading, Panel } from '../components';
import { formatTime } from '../format';
import { useSessionEvents } from '../hooks';
import { useI18n } from '../i18n';
import { useNav } from '../navigation';
import { useApp } from '../store';
import { payloadNumber, payloadString } from '../api';
import type { EventRecord, RunSummary } from '../api';

const TERMINAL_RUN_STATES = new Set(['succeeded', 'failed', 'cancelled']);

interface Phase {
  /** Lifecycle-state identifier: rendered through tState() so it stays localized. */
  key: string;
  tone: 'ok' | 'error' | 'warn' | 'info' | 'muted';
}

function phaseFor(event: EventRecord): Phase {
  const explicit = payloadString(event.payload, 'phase');
  if (explicit !== null) {
    switch (explicit) {
      case 'goal':
        return { key: 'goal', tone: 'info' };
      case 'plan':
        return { key: 'planning', tone: 'info' };
      case 'act':
        return { key: 'acting', tone: 'warn' };
      case 'observe':
        return { key: 'observing', tone: 'ok' };
      case 'finalize':
        return { key: 'finalizing', tone: 'ok' };
      default:
        return { key: explicit, tone: 'muted' };
    }
  }
  switch (event.kind) {
    case 'run_created':
      return { key: 'goal', tone: 'info' };
    case 'task_created':
    case 'task_queued':
      return { key: 'planning', tone: 'info' };
    case 'agent_step':
      return { key: 'thinking', tone: 'info' };
    case 'model_call':
      return { key: 'thinking', tone: 'info' };
    case 'model_result':
      return { key: 'thinking', tone: 'ok' };
    case 'tool_call':
    case 'capability_invoked':
    case 'task_started':
      return { key: 'acting', tone: 'warn' };
    case 'task_retrying':
      return { key: 'retrying', tone: 'warn' };
    case 'tool_result':
    case 'task_completed':
      return { key: 'observing', tone: 'ok' };
    case 'task_failed':
    case 'capability_denied':
    case 'policy_denied':
    case 'run_failed':
      return { key: 'failed', tone: 'error' };
    case 'task_cancelled':
      return { key: 'cancelled', tone: 'warn' };
    case 'run_completed':
      return { key: 'finalizing', tone: 'ok' };
    default:
      return { key: 'unknown', tone: 'muted' };
  }
}

export function AgentStateView() {
  const { t, tState } = useI18n();
  const { selectedSessionId, detail, detailError, detailLoading, refreshDetail, sessions } = useApp();
  const { setView } = useNav();
  const session = useSessionEvents(selectedSessionId, 300);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);

  const runs: RunSummary[] = useMemo(
    () => (detail === null || detail.runtime === null ? [] : detail.runtime.runs),
    [detail],
  );

  const run = useMemo(() => {
    if (runs.length === 0) return undefined;
    if (selectedRunId !== null) {
      const found = runs.find((item) => item.agent_id === selectedRunId);
      if (found !== undefined) return found;
    }
    return runs[runs.length - 1];
  }, [runs, selectedRunId]);

  const timeline = useMemo(() => {
    if (run === undefined) return [];
    return (session.data ?? []).filter((event) => event.agent_id === run.agent_id);
  }, [session.data, run]);

  const selected = sessions.find((item) => item.id === selectedSessionId) ?? null;

  if (selectedSessionId === null) {
    return (
      <Panel title={t('agent.title')} subtitle={t('chat.noSession')}>
        <EmptyState
          title={t('common.pickSession')}
          hint={
            <button type="button" className="btn" onClick={() => setView('sessions')}>
              {t('common.goToSessions')}
            </button>
          }
        />
      </Panel>
    );
  }

  return (
    <div className="stack">
      <Panel
        title={selected === null ? t('agent.title') : t('agent.titleWithSession', { title: selected.title })}
        subtitle={t('agent.subtitle')}
        actions={
          <button type="button" className="btn btn-ghost btn-small" onClick={() => void refreshDetail()}>
            {detailLoading ? t('common.refreshing') : t('common.refresh')}
          </button>
        }
      >
        <ApiErrorBanner error={detailError} scope="GET /v1/sessions/{id}" onRetry={() => void refreshDetail()} />
        <ApiErrorBanner error={session.error} scope="GET /v1/sessions/{id}/events" onRetry={session.reload} />

        {runs.length === 0 ? (
          <EmptyState
            title={t('agent.noRun')}
            hint={
              <button type="button" className="btn" onClick={() => setView('chat')}>
                {t('chat.send')}
              </button>
            }
          />
        ) : (
          <>
            <div className="chips">
              {runs.map((item, index) => (
                <button
                  key={item.agent_id}
                  type="button"
                  className={
                    run !== undefined && item.agent_id === run.agent_id ? 'chip chip-active' : 'chip'
                  }
                  onClick={() => setSelectedRunId(item.agent_id)}
                  title={item.goal}
                >
                  {t('agent.runChip', { n: index + 1, state: tState(item.state) })}
                </button>
              ))}
            </div>

            {run === undefined ? null : (
              <>
                <KeyValue
                  rows={[
                    [
                      t('common.state'),
                      <Badge
                        tone={
                          run.state === 'succeeded'
                            ? 'ok'
                            : run.state === 'failed'
                              ? 'error'
                              : TERMINAL_RUN_STATES.has(run.state)
                                ? 'warn'
                                : 'info'
                        }
                      >
                        {tState(run.state)}
                      </Badge>,
                    ],
                    ['agent_id', run.agent_id],
                    [t('common.model'), run.model ?? '--'],
                    [t('agent.steps'), String(run.steps)],
                    [t('agent.goal'), run.goal],
                  ]}
                />
                <h3 className="section-title">{t('agent.stepTimeline')}</h3>
                {timeline.length === 0 ? (
                  <Loading label={t('agent.noSteps')} />
                ) : (
                  <ol className="timeline">
                    {timeline.map((event) => {
                      const phase = phaseFor(event);
                      const attempt = payloadNumber(event.payload, 'attempt');
                      return (
                        <li className="timeline-item" key={event.id}>
                          <div className="timeline-head">
                            <Badge tone={phase.tone}>{tState(phase.key)}</Badge>
                            <span className="timeline-kind">{event.kind}</span>
                            <span className="muted small">{formatTime(event.ts)}</span>
                            <span className="muted small">{t('agent.seqLabel', { n: event.seq })}</span>
                            {attempt !== null ? (
                              <span className="muted small">{t('agent.attemptLabel', { n: attempt })}</span>
                            ) : null}
                          </div>
                          <p className="timeline-message">{event.message}</p>
                          {event.payload !== null && event.payload !== undefined ? (
                            <details className="stream-payload">
                              <summary>{t('events.payload')}</summary>
                              <JsonBlock value={event.payload} />
                            </details>
                          ) : null}
                        </li>
                      );
                    })}
                  </ol>
                )}

                <h3 className="section-title">{t('agent.finalAnswer')}</h3>
                {run.final_answer === null ? (
                  <p className="muted">{t('agent.notFinalised')}</p>
                ) : (
                  <pre className="answer">{run.final_answer}</pre>
                )}

                <h3 className="section-title">{t('agent.errorField')}</h3>
                {run.error === null ? (
                  <p className="muted">{t('agent.noError')}</p>
                ) : (
                  <div className="banner banner-error">
                    <p className="banner-message">{run.error}</p>
                  </div>
                )}
              </>
            )}
          </>
        )}
      </Panel>
    </div>
  );
}
