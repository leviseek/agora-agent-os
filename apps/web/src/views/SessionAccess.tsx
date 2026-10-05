/**
 * What a session may use, and who else may take part.
 *
 * Both halves answer the same question from two sides: capabilities are what the conversation may
 * reach, access requests are who gets to have the conversation. They live together because the
 * person who decides one is the person who decides the other.
 */
import { useCallback, useEffect, useState } from 'react';
import { useApp } from '../store';
import { useI18n } from '../i18n';
import { ApiErrorBanner, Badge } from '../components';
import type { AccessRequest, SessionCapabilitiesResponse } from '../api';
import { formatDateTime } from '../format';

/** The runtime's capabilities, with the session's narrowing on top. */
export function CapabilitiesEditor({ sessionId, canEdit }: { sessionId: string; canEdit: boolean }) {
  const { t } = useI18n();
  const { loadCapabilities, saveCapabilities, busy, actionError } = useApp();
  const [state, setState] = useState<SessionCapabilitiesResponse | null>(null);
  const [narrowing, setNarrowing] = useState(false);
  const [picked, setPicked] = useState<string[]>([]);

  const reload = useCallback(async (): Promise<void> => {
    const response = await loadCapabilities(sessionId);
    setState(response);
    if (response !== null && response.session.allow !== null) {
      setNarrowing(true);
      setPicked(response.session.allow);
    } else {
      setNarrowing(false);
      setPicked([]);
    }
  }, [loadCapabilities, sessionId]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const save = async (): Promise<void> => {
    // null is "whatever the runtime allows": the way back from a narrowing is to clear it, not to
    // tick everything, which would freeze today's list as a permanent allow list.
    const ok = await saveCapabilities(sessionId, narrowing ? picked : null);
    if (ok) await reload();
  };

  if (state === null) {
    return <p className="muted small">{t('common.loading')}</p>;
  }

  return (
    <div className="stack-tight">
      <ApiErrorBanner error={actionError} scope="PUT /v1/sessions/{id}/capabilities" onRetry={() => void reload()} />
      <fieldset className="fieldset">
        <legend>{t('access.narrowing')}</legend>
        <label className="check">
          <input
            type="radio"
            name="narrowing"
            checked={!narrowing}
            disabled={!canEdit}
            onChange={() => setNarrowing(false)}
          />
          <span>{t('access.allCapabilities', { n: state.runtime.length })}</span>
        </label>
        <label className="check">
          <input
            type="radio"
            name="narrowing"
            checked={narrowing}
            disabled={!canEdit}
            onChange={() => setNarrowing(true)}
          />
          <span>{t('access.onlyPicked')}</span>
        </label>
      </fieldset>
      {narrowing ? (
        <div className="capability-grid">
          {state.runtime.map((name) => (
            <label key={name} className="check">
              <input
                type="checkbox"
                checked={picked.includes(name)}
                disabled={!canEdit}
                onChange={(event) =>
                  setPicked((current) =>
                    event.target.checked
                      ? [...current, name]
                      : current.filter((entry) => entry !== name),
                  )
                }
              />
              <span className="mono">{name}</span>
            </label>
          ))}
        </div>
      ) : null}
      <p className="muted small">
        {t('access.effective', { n: state.effective.length, list: state.effective.join(', ') })}
      </p>
      {state.session.deny.length > 0 ? (
        <p className="muted small">
          {t('access.denied', { list: state.session.deny.join(', ') })}
        </p>
      ) : null}
      {state.session.approval_required.length > 0 ? (
        <p className="muted small">
          {t('access.needsApproval', { list: state.session.approval_required.join(', ') })}
        </p>
      ) : null}
      {canEdit ? (
        <div className="row-actions">
          <button type="button" className="btn btn-small" disabled={busy} onClick={() => void save()}>
            {t('access.saveCapabilities')}
          </button>
        </div>
      ) : (
        <p className="muted small">{t('access.ownerOnly')}</p>
      )}
    </div>
  );
}

/** Asking for access to a conversation that is not yours. */
export function RequestAccessForm({ sessionId, onDone }: { sessionId: string; onDone: () => void }) {
  const { t } = useI18n();
  const { requestAccess, busy, actionError } = useApp();
  const [role, setRole] = useState('participant');
  const [note, setNote] = useState('');

  const submit = async (): Promise<void> => {
    const ok = await requestAccess(sessionId, role, note.trim().length === 0 ? undefined : note.trim());
    if (ok) onDone();
  };

  return (
    <div className="stack-tight">
      <ApiErrorBanner error={actionError} scope="POST /v1/sessions/{id}/access-requests" />
      <div className="form-grid form-grid-inline">
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
        <button type="button" className="btn btn-small" disabled={busy} onClick={() => void submit()}>
          {t('access.askSubmit')}
        </button>
        <button type="button" className="btn btn-ghost btn-small" onClick={onDone}>
          {t('common.cancel')}
        </button>
      </div>
    </div>
  );
}

/** The requests on a session: the owner decides, a requester watches their own. */
export function AccessRequests({ sessionId, focusRequest }: { sessionId: string; focusRequest?: boolean }) {
  const { t } = useI18n();
  const { loadAccessRequests, decideAccess, busy, actionError } = useApp();
  const [requests, setRequests] = useState<AccessRequest[]>([]);
  const [mayDecide, setMayDecide] = useState(false);
  const [role, setRole] = useState<Record<string, string>>({});

  const reload = useCallback(async (): Promise<void> => {
    const response = await loadAccessRequests(sessionId);
    setRequests(response?.requests ?? []);
    setMayDecide(response?.may_decide ?? false);
  }, [loadAccessRequests, sessionId]);

  useEffect(() => {
    void reload();
  }, [reload, focusRequest]);

  const decide = async (id: string, approve: boolean): Promise<void> => {
    const ok = await decideAccess(sessionId, id, approve, role[id]);
    if (ok) await reload();
  };

  if (requests.length === 0) {
    return <p className="muted small">{t('access.none')}</p>;
  }

  return (
    <div className="stack-tight">
      <ApiErrorBanner error={actionError} scope="access requests" onRetry={() => void reload()} />
      <table className="table table-nested">
        <thead>
          <tr>
            <th>{t('access.who')}</th>
            <th>{t('access.asked')}</th>
            <th>{t('common.state')}</th>
            <th>{t('access.note')}</th>
            {mayDecide ? <th /> : null}
          </tr>
        </thead>
        <tbody>
          {requests.map((request) => (
            <tr key={request.id}>
              <td className="mono">
                {request.principal.node_id == null || request.principal.node_id === ''
                  ? request.principal.user_id
                  : request.principal.user_id + '@' + request.principal.node_id}
              </td>
              <td>{formatDateTime(request.created_at)}</td>
              <td>
                <Badge tone={request.state === 'pending' ? 'warn' : request.state === 'approved' ? 'ok' : 'muted'}>
                  {t('access.state.' + request.state)}
                </Badge>
                {request.granted_role != null ? (
                  <span className="muted small"> {t('sessions.role.' + request.granted_role)}</span>
                ) : null}
              </td>
              <td className="muted small">{request.note ?? ''}</td>
              {mayDecide ? (
                <td className="cell-actions">
                  {request.state === 'pending' ? (
                    <>
                      <select
                        value={role[request.id] ?? request.role}
                        onChange={(event) =>
                          setRole((current) => ({ ...current, [request.id]: event.target.value }))
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
                        onClick={() => void decide(request.id, true)}
                      >
                        {t('access.approve')}
                      </button>
                      <button
                        type="button"
                        className="btn btn-ghost btn-small"
                        disabled={busy}
                        onClick={() => void decide(request.id, false)}
                      >
                        {t('access.reject')}
                      </button>
                    </>
                  ) : (
                    <span className="muted small">
                      {t('access.decidedBy', { who: request.decided_by ?? '?' })}
                    </span>
                  )}
                </td>
              ) : null}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
