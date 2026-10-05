/**
 * View - Workspaces: the unit of ownership, sharing and filesystem isolation.
 *
 * A workspace owns its sessions and the directory they may touch, so this is where a working unit is
 * created, shared and narrowed - and where the console's session list gets its grouping from.
 * See docs/decisions.md D20.
 */
import { useEffect, useState } from 'react';
import type { FormEvent } from 'react';
import { ApiErrorBanner, Badge, EmptyState, Loading, Panel } from '../components';
import type { Tone } from '../components';
import { formatDateTime } from '../format';
import { useI18n } from '../i18n';
import { useNav } from '../navigation';
import { useApp } from '../store';
import { CapabilitiesEditor } from './SessionAccess';

function roleTone(role: string | null): Tone {
  switch (role) {
    case 'owner':
      return 'ok';
    case 'editor':
      return 'info';
    case 'participant':
      return 'warn';
    case 'viewer':
      return 'muted';
    default:
      return 'muted';
  }
}

export function WorkspacesView() {
  const { t } = useI18n();
  const { setView } = useNav();
  const {
    connection,
    workspaces,
    discoverableWorkspaces,
    workspacesError,
    workspacesLoading,
    refreshWorkspaces,
    createWorkspace,
    renameWorkspace,
    selectedWorkspaceId,
    selectWorkspace,
    grantWorkspaceAccess,
    revokeWorkspaceAccess,
    requestWorkspaceAccess,
    sessions,
    selectSession,
    busy,
    actionError,
  } = useApp();

  const [name, setName] = useState('');
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameText, setRenameText] = useState('');
  const [memberUser, setMemberUser] = useState('');
  const [memberNode, setMemberNode] = useState('');
  const [memberRole, setMemberRole] = useState('participant');
  const [askRole, setAskRole] = useState('participant');

  useEffect(() => {
    if (connection === 'online') void refreshWorkspaces();
  }, [connection, refreshWorkspaces]);

  const selected = workspaces.find((entry) => entry.workspace.id === selectedWorkspaceId) ?? null;
  const selectedSessions =
    selected === null
      ? []
      : sessions.filter((session) => (session.workspace_id ?? null) === selected.workspace.id);

  const onCreate = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault();
    if (name.trim().length === 0) return;
    const id = await createWorkspace(name);
    if (id !== null) setName('');
  };

  const onRename = async (id: string): Promise<void> => {
    if (renameText.trim().length === 0) return;
    const ok = await renameWorkspace(id, renameText);
    if (ok) {
      setRenaming(null);
      setRenameText('');
    }
  };

  const onAddMember = async (): Promise<void> => {
    if (selected === null || memberUser.trim().length === 0) return;
    const ok = await grantWorkspaceAccess(
      selected.workspace.id,
      memberUser,
      memberNode.trim().length === 0 ? null : memberNode.trim(),
      memberRole,
    );
    if (ok) {
      setMemberUser('');
      setMemberNode('');
    }
  };

  const openSession = (id: string): void => {
    selectSession(id);
    setView('chat');
  };

  return (
    <div className="stack">
      <ApiErrorBanner error={actionError} scope="workspaces" onRetry={() => void refreshWorkspaces()} />

      <Panel
        title={t('workspaces.title')}
        subtitle={t('workspaces.subtitle')}
        actions={
          <>
            {workspaces.length > 0 ? <Badge tone="info">{t('workspaces.count', { n: workspaces.length })}</Badge> : null}
            <button
              type="button"
              className="btn btn-ghost btn-small"
              onClick={() => selectWorkspace(null)}
              disabled={selectedWorkspaceId === null}
            >
              {t('workspaces.showAll')}
            </button>
            <button type="button" className="btn btn-ghost btn-small" onClick={() => void refreshWorkspaces()}>
              {t('common.refresh')}
            </button>
          </>
        }
      >
        <form className="form-grid form-grid-inline" onSubmit={(event) => void onCreate(event)}>
          <label className="field">
            <span>{t('workspaces.newName')}</span>
            <input
              type="text"
              value={name}
              placeholder={t('workspaces.newNamePlaceholder')}
              onChange={(event) => setName(event.target.value)}
            />
          </label>
          <button type="submit" className="btn" disabled={busy || name.trim().length === 0}>
            {t('workspaces.create')}
          </button>
        </form>
        <p className="muted small">{t('workspaces.newHint')}</p>

        {workspacesError !== null ? (
          <p className="muted">{workspacesError.message}</p>
        ) : workspacesLoading && workspaces.length === 0 ? (
          <Loading label={t('common.loading')} />
        ) : workspaces.length === 0 ? (
          <EmptyState title={t('workspaces.empty')} hint={t('workspaces.emptyHint')} />
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>{t('workspaces.name')}</th>
                <th>{t('workspaces.owner')}</th>
                <th>{t('workspaces.myRole')}</th>
                <th>{t('workspaces.sessions')}</th>
                <th>{t('workspaces.created')}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {workspaces.map((entry) => {
                const workspace = entry.workspace;
                const isSelected = workspace.id === selectedWorkspaceId;
                return (
                  <tr key={workspace.id} className={isSelected ? 'row-selected' : undefined}>
                    <td>
                      <button
                        type="button"
                        className="linkish"
                        onClick={() => selectWorkspace(isSelected ? null : workspace.id)}
                      >
                        {workspace.name}
                      </button>
                      {workspace.metadata?.default === 'true' ? (
                        <span className="muted small"> {t('workspaces.defaultBadge')}</span>
                      ) : null}
                      <div className="muted small mono">{workspace.id}</div>
                    </td>
                    <td className="mono">
                      {workspace.owner.node_id == null || workspace.owner.node_id === ''
                        ? workspace.owner.user_id
                        : workspace.owner.user_id + '@' + workspace.owner.node_id}
                    </td>
                    <td>
                      <Badge tone={roleTone(entry.workspace_role)}>
                        {entry.workspace_role === null
                          ? t('workspaces.noRole')
                          : t('sessions.role.' + entry.workspace_role)}
                      </Badge>
                    </td>
                    <td>{entry.session_count ?? 0}</td>
                    <td className="muted small">{formatDateTime(workspace.created_at)}</td>
                    <td className="cell-actions">
                      <button
                        type="button"
                        className="btn btn-small"
                        onClick={() => {
                          selectWorkspace(workspace.id);
                          setView('sessions');
                        }}
                      >
                        {t('workspaces.openSessions')}
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </Panel>

      {discoverableWorkspaces.length > 0 ? (
        <Panel
          title={t('workspaces.discoverTitle')}
          subtitle={t('workspaces.discoverHint')}
          actions={
            <label className="field field-inline">
              <span>{t('workspaces.askRole')}</span>
              <select value={askRole} onChange={(event) => setAskRole(event.target.value)}>
                <option value="viewer">{t('sessions.role.viewer')}</option>
                <option value="participant">{t('sessions.role.participant')}</option>
                <option value="editor">{t('sessions.role.editor')}</option>
              </select>
            </label>
          }
        >
          <table className="table">
            <thead>
              <tr>
                <th>{t('workspaces.name')}</th>
                <th>{t('workspaces.owner')}</th>
                <th>{t('workspaces.sessions')}</th>
                <th>{t('workspaces.created')}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {discoverableWorkspaces.map((entry) => (
                <tr key={entry.id}>
                  <td>
                    {entry.name}
                    <div className="muted small mono">{entry.id}</div>
                  </td>
                  <td className="mono">
                    {entry.owner.node_id == null || entry.owner.node_id === ''
                      ? entry.owner.user_id
                      : entry.owner.user_id + '@' + entry.owner.node_id}
                  </td>
                  <td>{entry.session_count}</td>
                  <td className="muted small">{formatDateTime(entry.created_at)}</td>
                  <td className="cell-actions">
                    <button
                      type="button"
                      className="btn btn-small"
                      disabled={busy}
                      onClick={async () => {
                        const ok = await requestWorkspaceAccess(entry.id, askRole);
                        if (ok) await refreshWorkspaces();
                      }}
                    >
                      {t('workspaces.ask')}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </Panel>
      ) : null}

      {selected !== null ? (
        <Panel
          title={t('workspaces.detailTitle', { name: selected.workspace.name })}
          subtitle={t('workspaces.detailHint')}
          actions={
            <Badge tone={roleTone(selected.workspace_role)}>
              {selected.workspace_role === null
                ? t('workspaces.noRole')
                : t('sessions.role.' + selected.workspace_role)}
            </Badge>
          }
        >
          <div className="stack-tight">
            {selected.can.includes('grant') ? (
              <div className="form-grid form-grid-inline">
                <label className="field">
                  <span>{t('workspaces.rename')}</span>
                  <input
                    type="text"
                    value={renaming === selected.workspace.id ? renameText : selected.workspace.name}
                    onChange={(event) => {
                      setRenaming(selected.workspace.id);
                      setRenameText(event.target.value);
                    }}
                  />
                </label>
                <button
                  type="button"
                  className="btn btn-small"
                  disabled={busy || renaming !== selected.workspace.id}
                  onClick={() => void onRename(selected.workspace.id)}
                >
                  {t('workspaces.renameSubmit')}
                </button>
              </div>
            ) : null}

            <h4 className="section-title">{t('workspaces.members')}</h4>
            <p className="muted small">{t('workspaces.membersHint')}</p>
            <table className="table table-nested">
              <thead>
                <tr>
                  <th>{t('workspaces.member')}</th>
                  <th>{t('workspaces.myRole')}</th>
                  <th>{t('workspaces.grantedBy')}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                <tr>
                  <td className="mono">{selected.workspace.owner.user_id}</td>
                  <td>
                    <Badge tone="ok">{t('sessions.role.owner')}</Badge>
                  </td>
                  <td className="muted small">-</td>
                  <td />
                </tr>
                {selected.workspace.grants.map((grant) => (
                  <tr key={grant.user_id + '@' + (grant.node_id ?? '')}>
                    <td className="mono">
                      {grant.node_id == null || grant.node_id === ''
                        ? grant.user_id
                        : grant.user_id + '@' + grant.node_id}
                    </td>
                    <td>
                      <Badge tone={roleTone(grant.role)}>{t('sessions.role.' + grant.role)}</Badge>
                    </td>
                    <td className="muted small">{grant.granted_by ?? '-'}</td>
                    <td className="cell-actions">
                      {selected.can.includes('grant') ? (
                        <button
                          type="button"
                          className="btn btn-ghost btn-small"
                          disabled={busy}
                          onClick={() =>
                            void revokeWorkspaceAccess(selected.workspace.id, grant.user_id, grant.node_id)
                          }
                        >
                          {t('workspaces.revoke')}
                        </button>
                      ) : null}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>

            {selected.can.includes('grant') ? (
              <div className="form-grid form-grid-inline">
                <label className="field">
                  <span>{t('workspaces.memberUser')}</span>
                  <input
                    type="text"
                    value={memberUser}
                    placeholder={t('workspaces.memberUserPlaceholder')}
                    onChange={(event) => setMemberUser(event.target.value)}
                  />
                </label>
                <label className="field">
                  <span>{t('workspaces.memberNode')}</span>
                  <input
                    type="text"
                    value={memberNode}
                    placeholder={t('workspaces.memberNodePlaceholder')}
                    onChange={(event) => setMemberNode(event.target.value)}
                  />
                </label>
                <label className="field">
                  <span>{t('workspaces.memberRole')}</span>
                  <select value={memberRole} onChange={(event) => setMemberRole(event.target.value)}>
                    <option value="viewer">{t('sessions.role.viewer')}</option>
                    <option value="participant">{t('sessions.role.participant')}</option>
                    <option value="editor">{t('sessions.role.editor')}</option>
                    <option value="owner">{t('sessions.role.owner')}</option>
                  </select>
                </label>
                <button
                  type="button"
                  className="btn btn-small"
                  disabled={busy || memberUser.trim().length === 0}
                  onClick={() => void onAddMember()}
                >
                  {t('workspaces.grant')}
                </button>
              </div>
            ) : (
              <p className="muted small">{t('access.ownerOnly')}</p>
            )}

            <h4 className="section-title">{t('workspaces.capabilities')}</h4>
            <CapabilitiesEditor workspaceId={selected.workspace.id} canEdit={selected.can.includes('grant')} />

            <h4 className="section-title">{t('workspaces.sessionsIn')}</h4>
            {selectedSessions.length === 0 ? (
              <p className="muted small">{t('workspaces.noSessions')}</p>
            ) : (
              <table className="table table-nested">
                <thead>
                  <tr>
                    <th>{t('sessions.sessionTitle')}</th>
                    <th>{t('common.state')}</th>
                    <th>{t('sessions.owner')}</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {selectedSessions.map((session) => (
                    <tr key={session.id}>
                      <td>{session.title}</td>
                      <td>
                        <Badge tone="muted">{t('state.' + session.state)}</Badge>
                      </td>
                      <td className="mono">{session.owner?.user_id ?? session.user_id}</td>
                      <td className="cell-actions">
                        <button type="button" className="btn btn-small" onClick={() => openSession(session.id)}>
                          {t('workspaces.openChat')}
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        </Panel>
      ) : null}
    </div>
  );
}
