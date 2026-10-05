/**
 * What a scope may use.
 *
 * One editor for two scopes on purpose: the question - "what may this reach?" - is the same for a
 * session and for a workspace, and the answer must not be. Who may take part is not here: since D20
 * a role is held on a workspace, so membership is edited where the workspace is (the workspaces view)
 * and this file only narrows capabilities.
 */
import { useCallback, useEffect, useState } from 'react';
import { useApp } from '../store';
import { useI18n } from '../i18n';
import { ApiErrorBanner } from '../components';
import type { SessionCapabilities } from '../api';

/** What a scope narrows, normalized so one editor renders both a session and a workspace. */
interface NarrowingView {
  runtime: string[];
  narrowing: SessionCapabilities;
  effective: string[];
  /** Which record answered, so the page can say where the change will land. */
  scope: 'session' | 'workspace';
}

/**
 * The runtime's capabilities, with this scope's narrowing on top.
 *
 * One component for two scopes because the question is the same and the answer must not be: pass
 * `workspaceId` to change what the whole working unit may use (which every session in it inherits),
 * or `sessionId` to read the narrowing in force for one conversation.
 */
export function CapabilitiesEditor({
  sessionId,
  workspaceId,
  canEdit,
}: {
  sessionId?: string;
  workspaceId?: string;
  canEdit: boolean;
}) {
  const { t } = useI18n();
  const {
    loadCapabilities,
    saveCapabilities,
    loadWorkspaceCapabilities,
    saveWorkspaceCapabilities,
    busy,
    actionError,
  } = useApp();
  const [state, setState] = useState<NarrowingView | null>(null);
  const [narrowing, setNarrowing] = useState(false);
  const [picked, setPicked] = useState<string[]>([]);

  const reload = useCallback(async (): Promise<void> => {
    let response: NarrowingView | null = null;
    if (workspaceId !== undefined) {
      const raw = await loadWorkspaceCapabilities(workspaceId);
      if (raw !== null) {
        response = {
          runtime: raw.runtime,
          narrowing: raw.workspace,
          effective: raw.effective,
          scope: 'workspace',
        };
      }
    } else if (sessionId !== undefined) {
      const raw = await loadCapabilities(sessionId);
      if (raw !== null) {
        response = {
          runtime: raw.runtime,
          narrowing: raw.session,
          effective: raw.effective,
          scope: raw.scope ?? 'session',
        };
      }
    }
    setState(response);
    if (response !== null && response.narrowing.allow !== null) {
      setNarrowing(true);
      setPicked(response.narrowing.allow);
    } else {
      setNarrowing(false);
      setPicked([]);
    }
  }, [loadCapabilities, loadWorkspaceCapabilities, sessionId, workspaceId]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const save = async (): Promise<void> => {
    // null is "whatever the runtime allows": the way back from a narrowing is to clear it, not to
    // tick everything, which would freeze today's list as a permanent allow list.
    const allow = narrowing ? picked : null;
    const ok =
      workspaceId !== undefined
        ? await saveWorkspaceCapabilities(workspaceId, allow)
        : sessionId !== undefined
          ? await saveCapabilities(sessionId, allow)
          : false;
    if (ok) await reload();
  };

  if (state === null) {
    return <p className="muted small">{t('common.loading')}</p>;
  }

  return (
    <div className="stack-tight">
      <ApiErrorBanner
        error={actionError}
        scope={
          state.scope === 'workspace'
            ? 'PUT /v1/workspaces/{id}/capabilities'
            : 'PUT /v1/sessions/{id}/capabilities'
        }
        onRetry={() => void reload()}
      />
      <p className="muted small">
        {state.scope === 'workspace'
          ? t('access.scope.workspace')
          : t('access.scope.session')}
      </p>
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
      {state.narrowing.deny.length > 0 ? (
        <p className="muted small">
          {t('access.denied', { list: state.narrowing.deny.join(', ') })}
        </p>
      ) : null}
      {state.narrowing.approval_required.length > 0 ? (
        <p className="muted small">
          {t('access.needsApproval', { list: state.narrowing.approval_required.join(', ') })}
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

