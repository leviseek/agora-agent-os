/** View - Approvals: capability calls parked until an operator decides. */
import { useState } from 'react';
import { Badge, EmptyState, Panel } from '../components';
import { useI18n } from '../i18n';
import { useApp } from '../store';
import { formatTime } from '../format';

export function ApprovalsView() {
  const { t } = useI18n();
  const { approvals, approvalsUnsupported, decideApproval, busy, connection } = useApp();
  const [reasons, setReasons] = useState<Record<string, string>>({});

  return (
    <Panel
      title={t('approvals.title')}
      subtitle={t('approvals.subtitle', { n: approvals.length })}
      actions={<Badge tone={approvals.length > 0 ? 'warn' : 'ok'}>{approvals.length}</Badge>}
    >
      <p className="muted small">{t('approvals.hint')}</p>

      {approvalsUnsupported ? (
        <p className="notice">{t('approvals.unsupported')}</p>
      ) : null}

      {connection !== 'online' ? <p className="muted">{t('approvals.offline')}</p> : null}

      {approvals.length === 0 ? (
        <EmptyState title={t('approvals.emptyTitle')} hint={t('approvals.emptyHint')} />
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>{t('approvals.capability')}</th>
              <th>{t('approvals.arguments')}</th>
              <th>{t('approvals.waitingSince')}</th>
              <th>{t('approvals.decision')}</th>
            </tr>
          </thead>
          <tbody>
            {approvals.map((approval) => (
              <tr key={approval.id}>
                <td>
                  <strong>{approval.capability}</strong>
                  <div className="muted small">{approval.session_id}</div>
                </td>
                <td>
                  <code className="preview">{approval.arguments_preview}</code>
                </td>
                <td className="muted small">{formatTime(approval.created_at)}</td>
                <td>
                  <div className="row-actions">
                    <button
                      type="button"
                      className="btn btn-small"
                      disabled={busy}
                      onClick={() => void decideApproval(approval.id, true)}
                    >
                      {t('approvals.approve')}
                    </button>
                    <button
                      type="button"
                      className="btn btn-danger btn-small"
                      disabled={busy}
                      onClick={() =>
                        void decideApproval(
                          approval.id,
                          false,
                          reasons[approval.id] ?? t('approvals.defaultReason'),
                        )
                      }
                    >
                      {t('approvals.deny')}
                    </button>
                  </div>
                  <input
                    type="text"
                    className="deny-reason"
                    placeholder={t('approvals.reasonPlaceholder')}
                    value={reasons[approval.id] ?? ''}
                    onChange={(event) =>
                      setReasons((previous) => ({ ...previous, [approval.id]: event.target.value }))
                    }
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}
