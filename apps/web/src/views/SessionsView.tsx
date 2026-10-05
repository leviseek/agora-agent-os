/** View 2 - Sessions: list, create, select, close. */

import { useEffect, useState } from 'react';
import type { FormEvent } from 'react';
import { ApiErrorBanner, Badge, EmptyState, Loading, Mono, Panel } from '../components';
import type { Tone } from '../components';
import { formatDateTime } from '../format';
import { useI18n } from '../i18n';
import { useNav } from '../navigation';
import { useApp } from '../store';

function stateTone(state: string): Tone {
  switch (state) {
    case 'active':
      return 'ok';
    case 'idle':
      return 'info';
    case 'creating':
    case 'closing':
    case 'suspended':
      return 'warn';
    case 'failed':
      return 'error';
    default:
      return 'muted';
  }
}

export function SessionsView() {
  const { t, tState } = useI18n();
  const {
    sessions,
    sessionsError,
    sessionsLoading,
    refreshSessions,
    createSession,
    selectedSessionId,
    selectSession,
    closeSession,
    connection,
    detail,
    busy,
    sessionQuery,
    setSessionQuery,
    renameSession,
    viewDrafts,
    updateViewDraft,
  } = useApp();
  const { setView } = useNav();

  // Everything typed here lives in the store, not in component state: App renders one view at a
  // time, so a click on another tab unmounts this form. The user id also survives a reload.
  const { title, userId, renamingId, renameText } = viewDrafts.sessions;
  const setTitle = (next: string): void => updateViewDraft('sessions', { title: next });
  const setUserId = (next: string): void => updateViewDraft('sessions', { userId: next });
  const draftTitle = renamingId === null ? null : renameText;
  const setDraftTitle = (next: string | null): void =>
    updateViewDraft('sessions', { renamingId: next === null ? null : selectedSessionId, renameText: next ?? '' });
  const [creating, setCreating] = useState(false);

  useEffect(() => {
    if (connection === 'online') void refreshSessions();
  }, [connection, refreshSessions]);

  const onCreate = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault();
    setCreating(true);
    const id = await createSession(title, userId);
    setCreating(false);
    if (id !== null) {
      setTitle('');
      setView('chat');
    }
  };

  const onSelect = (id: string): void => {
    selectSession(id);
    setView('chat');
  };

  return (
    <div className="stack">
      <Panel title={t('sessions.create')} subtitle="POST /v1/sessions">
        <form className="form-grid form-grid-inline" onSubmit={(event) => void onCreate(event)}>
          <label className="field">
            <span>{t('sessions.sessionTitle')}</span>
            <input
              type="text"
              value={title}
              placeholder={t('sessions.titlePlaceholder')}
              onChange={(event) => setTitle(event.target.value)}
            />
          </label>
          <label className="field">
            <span>{t('sessions.userId')}</span>
            <input
              type="text"
              value={userId}
              placeholder={t('sessions.userPlaceholder')}
              onChange={(event) => setUserId(event.target.value)}
            />
          </label>
          <button type="submit" className="btn" disabled={creating || connection !== 'online'}>
            {creating ? t('common.creating') : t('sessions.create')}
          </button>
        </form>
      </Panel>

      <Panel
        title={t('sessions.title')}
        subtitle={t('sessions.count', { n: sessions.length })}
        flush
        actions={
          <button type="button" className="btn btn-ghost btn-small" onClick={() => void refreshSessions()}>
            {sessionsLoading ? t('common.refreshing') : t('common.refresh')}
          </button>
        }
      >
        <ApiErrorBanner error={sessionsError} scope="GET /v1/sessions" onRetry={() => void refreshSessions()} />

        <div className="search-row">
          <input
            type="search"
            value={sessionQuery}
            placeholder={t('sessions.searchHint')}
            onChange={(event) => setSessionQuery(event.target.value)}
            spellCheck={false}
          />
          {sessionQuery.length > 0 ? (
            <button type="button" className="btn btn-ghost btn-small" onClick={() => setSessionQuery('')}>
              {t('sessions.clearSearch')}
            </button>
          ) : null}
        </div>

        {sessions.length === 0 ? (
          sessionsLoading ? (
            <Loading label={t('sessions.loading')} />
          ) : (
            <EmptyState title={t('sessions.emptyTitle')} hint={t('sessions.emptyHint')} />
          )
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>{t('sessions.sessionTitle')}</th>
                <th>{t('sessions.sessionId')}</th>
                <th>{t('common.state')}</th>
                <th>{t('sessions.user')}</th>
                <th>{t('sessions.msgs')}</th>
                <th>{t('common.updated')}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {sessions.map((session) => (
                <tr
                  key={session.id}
                  className={session.id === selectedSessionId ? 'row-selected' : undefined}
                  onClick={() => selectSession(session.id)}
                >
                  <td>{session.title}</td>
                  <td>
                    <Mono title={session.id}>{session.id}</Mono>
                  </td>
                  <td>
                    <Badge tone={stateTone(session.state)}>{tState(session.state)}</Badge>
                  </td>
                  <td>{session.user_id}</td>
                  <td>{session.message_count}</td>
                  <td>{formatDateTime(session.updated_at)}</td>
                  <td className="cell-actions">
                    <button
                      type="button"
                      className="btn btn-small"
                      onClick={(event) => {
                        event.stopPropagation();
                        onSelect(session.id);
                      }}
                    >
                      {t('sessions.open')}
                    </button>
                    <button
                      type="button"
                      className="btn btn-ghost btn-small"
                      disabled={busy}
                      onClick={(event) => {
                        event.stopPropagation();
                        void closeSession(session.id);
                      }}
                    >
                      {t('sessions.close')}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      <Panel title={t('sessions.selectedTitle')} subtitle={t('sessions.selectedHint')}>
        {detail === null ? (
          <p className="muted">{t('chat.noSession')}</p>
        ) : (
          <div className="stack-tight">
            <p>
              {draftTitle === null ? (
                <>
                  <strong>{detail.session.title}</strong>{' '}
                  <Badge tone={stateTone(detail.session.state)}>{tState(detail.session.state)}</Badge>{' '}
                  <button
                    type="button"
                    className="btn btn-ghost btn-small"
                    onClick={() => setDraftTitle(detail.session.title)}
                  >
                    {t('sessions.rename')}
                  </button>
                </>
              ) : (
                <>
                  <input
                    type="text"
                    className="title-input"
                    value={draftTitle}
                    maxLength={200}
                    onChange={(event) => setDraftTitle(event.target.value)}
                    spellCheck={false}
                  />{' '}
                  <button
                    type="button"
                    className="btn btn-small"
                    disabled={busy || draftTitle.trim().length === 0}
                    onClick={() => {
                      void renameSession(detail.session.id, draftTitle.trim());
                      setDraftTitle(null);
                    }}
                  >
                    {t('sessions.saveTitle')}
                  </button>{' '}
                  <button type="button" className="btn btn-ghost btn-small" onClick={() => setDraftTitle(null)}>
                    {t('common.cancel')}
                  </button>
                </>
              )}
            </p>
            <p className="muted small">
              {t('sessions.actorLine', {
                actor: detail.session.actor_id,
                worker: detail.session.worker_id ?? t('sessions.unassigned'),
                created: formatDateTime(detail.session.created_at),
              })}
            </p>
            {detail.runtime === null ? (
              <p className="muted">{t('sessions.noRuntime')}</p>
            ) : (
              <>
                <p className="muted small">
                  {t('sessions.runtimeLine', {
                    messages: detail.runtime.messages,
                    goals: detail.runtime.goals_handled,
                    runs: detail.runtime.runs.length,
                    graphs: detail.runtime.graphs.length,
                  })}
                </p>
                {detail.runtime.runs.length > 0 ? (
                  <table className="table table-nested">
                    <thead>
                      <tr>
                        <th>{t('sessions.run')}</th>
                        <th>{t('common.state')}</th>
                        <th>{t('agent.steps')}</th>
                        <th>{t('common.model')}</th>
                        <th>{t('agent.goal')}</th>
                      </tr>
                    </thead>
                    <tbody>
                      {detail.runtime.runs.map((run) => (
                        <tr key={run.agent_id}>
                          <td>
                            <Mono title={run.agent_id}>{run.agent_id}</Mono>
                          </td>
                          <td>
                            <Badge
                              tone={run.state === 'succeeded' ? 'ok' : run.state === 'failed' ? 'error' : 'info'}
                            >
                              {tState(run.state)}
                            </Badge>
                          </td>
                          <td>{run.steps}</td>
                          <td>{run.model ?? '--'}</td>
                          <td className="cell-clip">{run.goal}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                ) : (
                  <p className="muted">{t('sessions.noRuns')}</p>
                )}
              </>
            )}
          </div>
        )}
      </Panel>
    </div>
  );
}
