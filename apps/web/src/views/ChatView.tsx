/** View 3 - Chat: the session transcript (runs + live events) and the goal box. */

import { useEffect, useMemo, useRef, useState } from 'react';
import type { FormEvent } from 'react';
import { ApiErrorBanner, Badge, EmptyState, JsonBlock, Panel } from '../components';
import { formatTime } from '../format';
import { useSessionEvents } from '../hooks';
import { useI18n } from '../i18n';
import { useNav } from '../navigation';
import { useApp } from '../store';
import type { EventRecord, RunSummary } from '../api';

const TERMINAL_RUN_STATES = new Set(['succeeded', 'failed', 'cancelled']);

function isRunning(state: string): boolean {
  return !TERMINAL_RUN_STATES.has(state);
}

function runTone(state: string): 'ok' | 'error' | 'warn' | 'info' {
  if (state === 'succeeded') return 'ok';
  if (state === 'failed') return 'error';
  if (state === 'cancelled') return 'warn';
  return 'info';
}

/** One streamed line inside a run: agent_step, tool_call/tool_result, task_*, model_*. */
function EventLine({ event }: { event: EventRecord }) {
  const { t } = useI18n();
  return (
    <div className={'stream-line stream-' + event.severity}>
      <div className="stream-head">
        <span className="stream-kind">{event.kind}</span>
        <span className="muted small">{formatTime(event.ts)}</span>
        <span className="muted small">{'#' + event.seq}</span>
      </div>
      <div className="stream-message">{event.message}</div>
      {event.payload !== null && event.payload !== undefined ? (
        <details className="stream-payload">
          <summary>{t('events.payload')}</summary>
          <JsonBlock value={event.payload} />
        </details>
      ) : null}
    </div>
  );
}

export function ChatView() {
  const { t, tState } = useI18n();
  const {
    selectedSessionId,
    detail,
    detailError,
    detailLoading,
    refreshDetail,
    sendGoal,
    cancelRun,
    busy,
    connection,
    sessions,
  } = useApp();
  const { setView } = useNav();
  const session = useSessionEvents(selectedSessionId, 200);

  const [goal, setGoal] = useState('');
  const [wait, setWait] = useState(false);
  const [lastResult, setLastResult] = useState<string | null>(null);

  const runs: RunSummary[] = useMemo(
    () => (detail === null || detail.runtime === null ? [] : detail.runtime.runs),
    [detail],
  );

  // The transcript is its own scroll area ABOVE the send form, so a goal posted without waiting
  // finished out of sight: the reply was there, the user was looking at the input box. Follow the
  // newest turn instead, and keep following while a run is still going.
  const transcriptRef = useRef<HTMLDivElement | null>(null);
  const lastRun = runs.length > 0 ? runs[runs.length - 1] : undefined;
  const lastRunState = lastRun === undefined ? '' : String(lastRun.state);
  const lastRunAnswer = lastRun === undefined ? null : lastRun.final_answer;
  useEffect(() => {
    const box = transcriptRef.current;
    if (box === null) return;
    box.scrollTop = box.scrollHeight;
  }, [runs.length, lastRunState, lastRunAnswer, session.data]);

  const byAgent = useMemo(() => {
    const grouped = new Map<string, EventRecord[]>();
    for (const event of session.data ?? []) {
      const key = event.agent_id ?? '__session__';
      const list = grouped.get(key);
      if (list === undefined) grouped.set(key, [event]);
      else list.push(event);
    }
    return grouped;
  }, [session.data]);

  const sessionOnly = byAgent.get('__session__') ?? [];
  const activeRun = runs.length > 0 ? runs[runs.length - 1] : undefined;
  const streaming = activeRun !== undefined && isRunning(activeRun.state);
  const selected = sessions.find((item) => item.id === selectedSessionId) ?? null;

  const onSubmit = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault();
    const text = goal.trim();
    if (text.length === 0) return;
    setLastResult(null);
    const response = await sendGoal(text, wait);
    if (response === null) return;
    setGoal('');
    if ('accepted' in response) {
      setLastResult(t('chat.accepted'));
    } else {
      setLastResult(
        response.error === null
          ? t('chat.resultFinished', { state: tState(response.state), steps: response.steps })
          : t('chat.resultFinishedError', {
              state: tState(response.state),
              steps: response.steps,
              error: response.error,
            }),
      );
    }
  };

  if (selectedSessionId === null) {
    return (
      <Panel title={t('chat.title')} subtitle={t('chat.noSession')}>
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
        title={selected === null ? t('chat.title') : t('chat.titleWithSession', { title: selected.title })}
        subtitle={
          detail !== null && detail.runtime !== null
            ? t('chat.summary', {
                messages: detail.runtime.messages,
                goals: detail.runtime.goals_handled,
                runs: runs.length,
              })
            : selectedSessionId
        }
        actions={
          <>
            {streaming ? (
              <Badge tone="warn">{t('chat.runInFlight')}</Badge>
            ) : (
              <Badge tone="muted">{tState('idle')}</Badge>
            )}
            <button type="button" className="btn btn-ghost btn-small" onClick={() => void refreshDetail()}>
              {detailLoading ? t('common.refreshing') : t('common.refresh')}
            </button>
          </>
        }
        flush
      >
        <ApiErrorBanner error={detailError} scope="GET /v1/sessions/{id}" onRetry={() => void refreshDetail()} />
        <ApiErrorBanner error={session.error} scope="GET /v1/sessions/{id}/events" onRetry={session.reload} />

        <div className="transcript" ref={transcriptRef}>
          {runs.length === 0 && sessionOnly.length === 0 ? (
            <EmptyState title={t('chat.empty')} hint={t('chat.emptyHint')} />
          ) : null}

          {runs.map((run) => {
            const runEvents = byAgent.get(run.agent_id) ?? [];
            return (
              <article className="run" key={run.agent_id}>
                <header className="run-head">
                  <Badge tone={runTone(run.state)}>{tState(run.state)}</Badge>
                  <span className="muted small">{t('chat.runLabel', { id: run.agent_id })}</span>
                  {run.model !== null ? (
                    <span className="muted small">{t('chat.modelLabel', { model: run.model })}</span>
                  ) : null}
                  <span className="muted small">{t('chat.stepsCount', { n: run.steps })}</span>
                </header>

                <div className="bubble bubble-user">
                  <span className="bubble-role">{t('agent.goal')}</span>
                  <p>{run.goal}</p>
                </div>

                {runEvents.length > 0 ? (
                  <div className="stream">
                    {runEvents.map((event) => (
                      <EventLine event={event} key={event.id} />
                    ))}
                  </div>
                ) : null}

                {isRunning(run.state) ? (
                  <div className="bubble bubble-agent bubble-pending">
                    <span className="bubble-role">{t('chat.agent')}</span>
                    <p className="muted">{t('chat.workingHint')}</p>
                  </div>
                ) : null}

                {run.final_answer !== null ? (
                  <div className="bubble bubble-agent">
                    <span className="bubble-role">{t('chat.answer')}</span>
                    <p>{run.final_answer}</p>
                  </div>
                ) : null}

                {run.error !== null ? (
                  <div className="bubble bubble-error">
                    <span className="bubble-role">{t('agent.errorField')}</span>
                    <p>{run.error}</p>
                  </div>
                ) : null}
              </article>
            );
          })}

          {sessionOnly.length > 0 ? (
            <details className="session-events">
              <summary>{t('chat.sessionEvents', { n: sessionOnly.length })}</summary>
              <div className="stream">
                {sessionOnly.map((event) => (
                  <EventLine event={event} key={event.id} />
                ))}
              </div>
            </details>
          ) : null}
        </div>
      </Panel>

      <Panel title={t('chat.send')} subtitle="POST /v1/sessions/{id}/messages">
        <form className="chat-form" onSubmit={(event) => void onSubmit(event)}>
          <textarea
            value={goal}
            rows={3}
            placeholder={t('chat.placeholder')}
            onChange={(event) => setGoal(event.target.value)}
          />
          <div className="chat-form-actions">
            <label className="checkbox">
              <input type="checkbox" checked={wait} onChange={(event) => setWait(event.target.checked)} />
              <span>{t('chat.waitLabel')}</span>
            </label>
            <div className="row-actions">
              <button
                type="submit"
                className="btn"
                disabled={busy || goal.trim().length === 0 || connection !== 'online'}
              >
                {busy ? t('shell.working') : wait ? t('chat.sendAndWait') : t('common.send')}
              </button>
              <button
                type="button"
                className="btn btn-danger"
                disabled={connection !== 'online'}
                onClick={() => void cancelRun()}
              >
                {t('chat.cancel')}
              </button>
            </div>
          </div>
          {lastResult !== null ? <p className="muted small">{lastResult}</p> : null}
        </form>
      </Panel>
    </div>
  );
}
