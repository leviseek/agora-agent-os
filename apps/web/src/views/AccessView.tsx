/** View - Access: what is waiting on you, and what you are waiting for.
 *
 * Access is decided at the workspace (docs/decisions.md D20): a role there is held in every session
 * of it. So the unit this page asks about and decides on is a workspace, and a request that predates
 * workspaces names its session instead.
 */
import { useCallback, useEffect, useMemo, useState } from 'react';
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

/** What a row is about. A workspace entry has no session; a legacy one has no workspace. */
function targetOf(entry: AccessInboxEntry): { kind: 'workspace' | 'session'; id: string; label: string } {
  if (entry.workspace_id != null && entry.workspace_id !== '') {
    return { kind: 'workspace', id: entry.workspace_id, label: entry.workspace_name ?? entry.workspace_id };
  }
  return { kind: 'session', id: entry.session_id ?? '', label: entry.session_title ?? '' };
}

/** A target the asker has no standing in: the only kind worth asking about. */
interface AskTarget {
  key: string;
  kind: 'workspace' | 'session';
  id: string;
  label: string;
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
    decideWorkspaceAccess,
    requestWorkspaceAccess,
    discoverableWorkspaces,
    sessions,
    selectSession,
    selectWorkspace,
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

  // Workspaces this person has no role in (the discovery index), and - for records written before
  // workspaces existed - sessions with no role either. An admin holds every right already.
  const askable = useMemo<AskTarget[]>(() => {
    const fromWorkspaces: AskTarget[] = discoverableWorkspaces.map((entry) => ({
      key: 'ws:' + entry.id,
      kind: 'workspace' as const,
      id: entry.id,
      label: entry.name,
    }));
    const fromSessions: AskTarget[] = sessions
      .filter((session) => (session.my_role ?? null) === null && (session.workspace_id ?? null) === null)
      .map((session) => ({
        key: 'ses:' + session.id,
        kind: 'session' as const,
        id: session.id,
        label: session.title,
      }));
    return [...fromWorkspaces, ...fromSessions];
  }, [discoverableWorkspaces, sessions]);

  useEffect(() => {
    if (target === '' && askable.length > 0) {
      const first = askable[0];
      if (first !== undefined) setTarget(first.key);
    }
  }, [askable, target]);

  const submit = useCallback(async (): Promise<void> => {
    const chosen = askable.find((entry) => entry.key === target);
    if (chosen === undefined) return;
    const trimmed = note.trim();
    const ok =
      chosen.kind === 'workspace'
        ? await requestWorkspaceAccess(chosen.id, role, trimmed.length === 0 ? undefined : trimmed)
        : await requestAccess(chosen.id, role, trimmed.length === 0 ? undefined : trimmed);
    if (ok) {
      setNote('');
      await refreshAccessInbox();
    }
  }, [askable, note, refreshAccessInbox, requestAccess, requestWorkspaceAccess, role, target]);

  const decide = useCallback(
    async (entry: AccessInboxEntry, approve: boolean): Promise<void> => {
      const about = targetOf(entry);
      const ok =
        about.kind === 'workspace'
          ? await decideWorkspaceAccess(about.id, entry.request.id, approve, decideRole[entry.request.id])
          : await decideAccess(about.id, entry.request.id, approve, decideRole[entry.request.id]);
      if (ok) await refreshAccessInbox();
    },
    [decideAccess, decideRole, decideWorkspaceAccess, refreshAccessInbox],
  );

  const openTarget = (entry: AccessInboxEntry): void => {
    const about = targetOf(entry);
    if (about.kind === 'workspace') {
      selectWorkspace(about.id);
      setView('workspaces');
      return;
    }
    selectSession(about.id);
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
              <span>{t('access.target')}</span>
              <select value={target} onChange={(event) => setTarget(event.target.value)}>
                {askable.map((entry) => (
                  <option key={entry.key} value={entry.key}>
                    {(entry.kind === 'workspace' ? t('access.kind.workspace') : t('access.kind.session')) +
                      ': ' +
                      entry.label}
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
                <th>{t('access.target')}</th>
                <th>{t('access.who')}</th>
                <th>{t('access.asked')}</th>
                <th>{t('access.note')}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {toDecide.map((entry) => {
                const about = targetOf(entry);
                return (
                  <tr key={entry.request.id}>
                    <td>
                      <button type="button" className="linkish" onClick={() => openTarget(entry)}>
                        {about.label}
                      </button>
                      <div className="muted small">
                        <Badge tone="muted">
                          {about.kind === 'workspace' ? t('access.kind.workspace') : t('access.kind.session')}
                        </Badge>{' '}
                        {t('access.ownedBy', { owner: ownerLabel(entry) })}
                      </div>
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
                        onClick={() => void decide(entry, true)}
                      >
                        {t('access.approve')}
                      </button>
                      <button
                        type="button"
                        className="btn btn-ghost btn-small"
                        disabled={busy}
                        onClick={() => void decide(entry, false)}
                      >
                        {t('access.reject')}
                      </button>
                    </td>
                  </tr>
                );
              })}
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
                <th>{t('access.target')}</th>
                <th>{t('access.askedFor')}</th>
                <th>{t('common.state')}</th>
                <th>{t('access.asked')}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {mine.map((entry) => {
                const about = targetOf(entry);
                return (
                  <tr key={entry.request.id}>
                    <td>
                      {about.label}
                      <div className="muted small">
                        <Badge tone="muted">
                          {about.kind === 'workspace' ? t('access.kind.workspace') : t('access.kind.session')}
                        </Badge>
                      </div>
                    </td>
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
                        <button type="button" className="btn btn-small" onClick={() => openTarget(entry)}>
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
                );
              })}
            </tbody>
          </table>
        )}
      </Panel>
    </div>
  );
}
