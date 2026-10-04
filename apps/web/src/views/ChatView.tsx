/** View 3 - Chat: the session transcript (runs + live events) and the goal box. */

import { useEffect, useMemo, useRef, useState } from 'react';
import type { FormEvent } from 'react';
import { ApiErrorBanner, Badge, EmptyState, JsonBlock, Panel } from '../components';
import { formatTime } from '../format';
import { useSessionEvents } from '../hooks';
import { useI18n } from '../i18n';

/** Provider names that are the runtime's own deterministic stand-in, not a real model. */
const PLACEHOLDER_PROVIDERS = new Set(['mock']);
function isPlaceholderProvider(provider: string): boolean {
  return PLACEHOLDER_PROVIDERS.has(provider) || provider.startsWith('agentos-mock');
}
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
    streamed,
    attachments,
    artifactUrl,
    modelOptions,
    configureSession,
  } = useApp();
  const { setView } = useNav();
  const session = useSessionEvents(selectedSessionId, 200);

  const [goal, setGoal] = useState('');
  // Workspace-relative image paths, comma separated. Attachments are read by the runtime through
  // the workspace jail, so this box can only name files the runtime is allowed to read.
  const [imagePaths, setImagePaths] = useState('');
  // The session's stored choice, edited in place. Changing it PATCHes the session, so the next
  // goal (and every goal after) uses it; a one-off override is available through the API.
  const sessionModel = detail === null ? '' : detail.session.model_hint ?? '';
  const sessionEffort = detail === null ? '' : detail.session.reasoning_effort ?? '';
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
  // Streaming text belongs to whichever run is currently writing. It is driven by the deltas
  // themselves, NOT by the run list: the detail view refreshes on a debounce, and a local model
  // finishes before the first refresh lands - which used to mean the live text never rendered at
  // all, even though the deltas had been arriving the whole time.
  const liveEntry = streamed.size > 0 ? [...streamed.entries()][streamed.size - 1] : null;
  const liveRunId = liveEntry === null ? null : liveEntry[0];
  const liveText = liveEntry === null ? '' : liveEntry[1];
  const liveRunVisible = liveRunId !== null && runs.some((run) => run.agent_id === liveRunId);
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
    const images = imagePaths
      .split(',')
      .map((path) => path.trim())
      .filter((path) => path.length > 0);
    const response = await sendGoal(text, wait, images);
    if (images.length > 0) {
      setImagePaths('');
    }
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
            {detail !== null && detail.runtime !== null && (detail.runtime.usage?.calls ?? 0) > 0 ? (
              <Badge tone="muted">
                {t('chat.sessionTokens', { tokens: detail.runtime.usage?.total_tokens ?? 0 })}
              </Badge>
            ) : null}
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

        {attachments.length > 0 ? (
          <div className="attachments">
            <span className="muted small">{t('chat.attachments')}</span>
            {attachments.map((image) => (
              <a
                key={image.artifact_id}
                className="attachment"
                href={artifactUrl(image.artifact_id)}
                target="_blank"
                rel="noreferrer"
                title={image.mime}
              >
                <img src={artifactUrl(image.artifact_id)} alt={image.name} />
                <span>{image.name}</span>
              </a>
            ))}
          </div>
        ) : null}

        <div className="transcript" ref={transcriptRef}>
          {runs.length === 0 && sessionOnly.length === 0 ? (
            <EmptyState title={t('chat.empty')} hint={t('chat.emptyHint')} />
          ) : null}

          {runs.map((run) => {
            const runEvents = byAgent.get(run.agent_id) ?? [];
            // provider is the recorded answerer; model is the legacy field that held the requested
            // one, so fall back to it only when the run predates the recorded answerer.
            const answeredBy = run.provider ?? run.model;
            const effort = run.reasoning_effort ?? null;
            return (
              <article className="run" key={run.agent_id}>
                <header className="run-head">
                  <Badge tone={runTone(run.state)}>{tState(run.state)}</Badge>
                  <span className="muted small">{t('chat.runLabel', { id: run.agent_id })}</span>
                  {answeredBy !== null ? (
                    isPlaceholderProvider(answeredBy) ? (
                      // The built-in provider is not a model anyone has heard of, and it needs no
                      // key: say so instead of dressing it up as one.
                      <Badge tone="muted">
                        {t('chat.placeholderProvider', { provider: answeredBy })}
                      </Badge>
                    ) : (
                      <Badge tone="info">
                        {t('chat.providerLabel', { provider: answeredBy })}
                      </Badge>
                    )
                  ) : null}
                  {effort !== null ? (
                    // What this run was asked for, next to who answered: without it, a run that
                    // cost more than expected has no visible cause.
                    <Badge tone="muted">{t('chat.effortLabel', { effort })}</Badge>
                  ) : null}
                  <span className="muted small">{t('chat.stepsCount', { n: run.steps })}</span>
                  {run.usage !== undefined && run.usage.calls > 0 ? (
                    <span className="muted small">
                      {t('chat.tokenUsage', { tokens: run.usage.total_tokens, calls: run.usage.calls })}
                    </span>
                  ) : null}
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
                    <span className="bubble-role">
                      {t('chat.agent')}
                      {liveRunId === run.agent_id && liveText.length > 0 ? (
                        <span className="streaming-dot" aria-hidden="true" />
                      ) : null}
                    </span>
                    {liveRunId === run.agent_id && liveText.length > 0 ? (
                      // The preview of an answer still being written. It disappears when the run
                      // finishes, because the stored answer takes its place.
                      <p className="streaming-text">{liveText}</p>
                    ) : (
                      <p className="muted">{t('chat.workingHint')}</p>
                    )}
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

          {liveText.length > 0 && !liveRunVisible ? (
            // The deltas are already here but the run itself has not reached the detail view yet,
            // which is the normal case for a fast model. Show the text anyway.
            <article className="run">
              <div className="bubble bubble-agent bubble-pending">
                <span className="bubble-role">
                  {t('chat.agent')}
                  <span className="streaming-dot" aria-hidden="true" />
                </span>
                <p className="streaming-text">{liveText}</p>
              </div>
            </article>
          ) : null}

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
          <div className="model-row">
            <label>
              <span>{t('chat.model')}</span>
              <select
                value={sessionModel}
                disabled={detail === null || connection !== 'online'}
                onChange={(event) =>
                  void configureSession(selectedSessionId ?? '', { model: event.target.value })
                }
              >
                <option value="">{t('chat.modelAuto')}</option>
                {modelOptions.map((name) => (
                  <option key={name} value={name}>
                    {name}
                  </option>
                ))}
              </select>
            </label>
            <label>
              <span>{t('chat.effort')}</span>
              <select
                value={sessionEffort}
                disabled={detail === null || connection !== 'online'}
                onChange={(event) =>
                  void configureSession(selectedSessionId ?? '', { effort: event.target.value })
                }
              >
                <option value="">{t('chat.effortDefault')}</option>
                <option value="off">{t('chat.effortOff')}</option>
                <option value="low">{t('chat.effortLow')}</option>
                <option value="medium">{t('chat.effortMedium')}</option>
                <option value="high">{t('chat.effortHigh')}</option>
              </select>
            </label>
          </div>
          <textarea
            value={goal}
            rows={3}
            placeholder={t('chat.placeholder')}
            onChange={(event) => setGoal(event.target.value)}
          />
          <input
            type="text"
            className="image-paths"
            value={imagePaths}
            placeholder={t('chat.imagePlaceholder')}
            onChange={(event) => setImagePaths(event.target.value)}
            spellCheck={false}
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
