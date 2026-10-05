/** View - Access: what is waiting on you, and what you are waiting for. */
import { useCallback, useEffect, useState } from 'react';
import { useApp } from '../store';
import { useI18n } from '../i18n';
import { useNav } from '../navigation';
import { ApiErrorBanner, Badge, Panel } from '../components';
import { formatDateTime } from '../format';
import type { AccessInboxEntry } from '../api';

function who(entry: AccessInboxEntry): string {
  const principal = entry.request.principal;
  return principal.node_id == null || principal.node_id === ''
    ? principal.user_id
    : principal.user_id + '@' + principal.node_id;
}

function ownerLabel(entry: AccessInboxEntry): string {
  const owner = entry.session_owner ?? null;
  if (owner === null) return '-';
  return owner.node_id == null || owner.node_id === '' ? owner.user_id : owner.user_id + '@' + owner.node_id;
}

export function AccessView() {
  const { t } = useI18n();
  const { setView } = useNav();
  const {
    connection,
    accessInbox,
    refreshAccessInbox,
    decideAccess,
    requestAccess,
    sessions,
    selectSession,
    busy,
    actionError,
  } = useApp();
  const [role, setRole] = useState('participant');
  const [note, setNote] = useState('');
  const [target, setTarget] = useState('');
  const [decideRole, setDecideRole] = useState<Record<string, string>>({});

  useEffect(() => {
    if (connection === 'online') void refreshAccessInbox();
  }, [connection, refreshAccessInbox]);

  // Sessions this person has no standing in: the only ones worth asking about.
  const askable = sessions.filter((session) => (session.my_role ?? null) === null);
  useEffect(() => {
    if (target === '' && askable.length > 0) setTarget(askable[0].id);
  }, [askable, target]);

  const submit = useCallback(async (): Promise<void> => {
    if (target === '') return;
    const ok = await requestAccess(target, role, note.trim().length === 0 ? undefined : note.trim());
    if (ok) {
      setNote('');
      await refreshAccessInbox();
    }
  }, [note, refreshAccessInbox, requestAccess, role, target]);

  const decide = useCallback(
    async (sessionId: string, requestId: string, approve: boolean): Promise<void> => {
      const ok = await decideAccess(sessionId, requestId, approve, decideRole[requestId]);
      if (ok) await refreshAccessInbox();
    },
    [decideAccess, decideRole, refreshAccessInbox],
  );

  const openSession = (id: string): void => {
    selectSession(id);
    setView('chat');
  };

  const toDecide = accessInbox?.to_decide ?? [];
  const mine = accessInbox?.mine ?? [];

  return (
    <div className="stack">
      <ApiErrorBanner error={actionError} scope="access" onRetry={() => void refreshAccessInbox()} />

      <Panel title={t('access.askTitle')} subtitle={t('access.askPageHint')}>
        {askable.length === 0 ? (
          <p className="muted">{t('access.nothingToAsk')}</p>
        ) : (
          <div className="form-grid form-grid-inline">
            <label className="field">
              <span>{t('access.session')}</span>
              <select value={target} onChange={(event) => setTarget(event.target.value)}>
                {askable.map((session) => (
                  <option key={session.id} value={session.id}>
                    {session.title}
                  </option>
                ))}
              </select>
            </label>
            <label className="field">
              <span>{t('access.askRole')}</span>
              <select value={role} onChange={(event) => setRole(event.target.value)}>
                <option value="viewer">{t('sessions.role.viewer')}</option>
                <option value="participant">{t('sessions.role.participant')}</option>
                <option value="editor">{t('sessions.role.editor')}</option>
              </select>
            </label>
            <label className="field">
              <span>{t('access.askNote')}</span>
              <input
                type="text"
                value={note}
                placeholder={t('access.askNotePlaceholder')}
                onChange={(event) => setNote(event.target.value)}
              />
            </label>
            <button type="button" className="btn" disabled={busy} onClick={() => void submit()}>
              {t('access.askSubmit')}
            </button>
          </div>
        )}
      </Panel>

      <Panel
        title={t('access.toDecideTitle')}
        subtitle={t('access.toDecideHint', { n: toDecide.length })}
      >
        {toDecide.length === 0 ? (
          <p className="muted">{t('access.noneToDecide')}</p>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>{t('access.session')}</th>
                <th>{t('access.who')}</th>
                <th>{t('access.asked')}</th>
                <th>{t('access.note')}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {toDecide.map((entry) => (
                <tr key={entry.request.id}>
                  <td>
                    <button type="button" className="linkish" onClick={() => openSession(entry.session_id)}>
                      {entry.session_title}
                    </button>
                    <div className="muted small">{t('access.ownedBy', { owner: ownerLabel(entry) })}</div>
                  </td>
                  <td className="mono">{who(entry)}</td>
                  <td>{formatDateTime(entry.request.created_at)}</td>
                  <td className="muted small">{entry.request.note ?? ''}</td>
                  <td className="cell-actions">
                    <select
                      value={decideRole[entry.request.id] ?? entry.request.role}
                      onChange={(event) =>
                        setDecideRole((current) => ({ ...current, [entry.request.id]: event.target.value }))
                      }
                    >
                      <option value="viewer">{t('sessions.role.viewer')}</option>
                      <option value="participant">{t('sessions.role.participant')}</option>
                      <option value="editor">{t('sessions.role.editor')}</option>
                    </select>
                    <button
                      type="button"
                      className="btn btn-small"
                      disabled={busy}
                      onClick={() => void decide(entry.session_id, entry.request.id, true)}
                    >
                      {t('access.approve')}
                    </button>
                    <button
                      type="button"
                      className="btn btn-ghost btn-small"
                      disabled={busy}
                      onClick={() => void decide(entry.session_id, entry.request.id, false)}
                    >
                      {t('access.reject')}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      <Panel title={t('access.mineTitle')} subtitle={t('access.mineHint', { n: mine.length })}>
        {mine.length === 0 ? (
          <p className="muted">{t('access.noneMine')}</p>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>{t('access.session')}</th>
                <th>{t('access.askedFor')}</th>
                <th>{t('common.state')}</th>
                <th>{t('access.asked')}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {mine.map((entry) => (
                <tr key={entry.request.id}>
                  <td>{entry.session_title}</td>
                  <td>{t('sessions.role.' + entry.request.role)}</td>
                  <td>
                    <Badge
                      tone={
                        entry.request.state === 'pending'
                          ? 'warn'
                          : entry.request.state === 'approved'
                            ? 'ok'
                            : 'muted'
                      }
                    >
                      {t('access.state.' + entry.request.state)}
                    </Badge>
                    {entry.request.granted_role != null ? (
                      <span className="muted small"> {t('sessions.role.' + entry.request.granted_role)}</span>
                    ) : null}
                  </td>
                  <td>{formatDateTime(entry.request.created_at)}</td>
                  <td className="cell-actions">
                    {entry.request.state === 'approved' ? (
                      <button type="button" className="btn btn-small" onClick={() => openSession(entry.session_id)}>
                        {t('access.goThere')}
                      </button>
                    ) : (
                      <span className="muted small">
                        {entry.request.decided_by === null || entry.request.decided_by === undefined
                          ? t('access.waitingOwner')
                          : t('access.decidedBy', { who: entry.request.decided_by })}
                      </span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>
    </div>
  );
}
