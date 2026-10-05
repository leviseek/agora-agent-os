/** View - Archives: what has been moved out of the hot store, and how to get it back. */
import { useEffect, useState } from 'react';
import { useApp } from '../store';
import { useI18n } from '../i18n';
import { useNav } from '../navigation';
import { ApiErrorBanner, Badge, CopyableText, Panel } from '../components';
import type { ArchiveDetail, ArchiveEntry } from '../api';
import { formatDateTime } from '../format';

function formatBytes(bytes: number): string {
  if (bytes < 1024) return bytes + ' B';
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KiB';
  return (bytes / (1024 * 1024)).toFixed(1) + ' MiB';
}

function ownerLabel(entry: ArchiveEntry): string {
  const owner = entry.manifest.owner ?? null;
  if (owner === null) return '-';
  return owner.node_id == null || owner.node_id === '' ? owner.user_id : owner.user_id + '@' + owner.node_id;
}

/** What the column shows: the user id. The node is on hover, and one click away. */
function ownerUser(entry: ArchiveEntry): string {
  return entry.manifest.owner?.user_id ?? '-';
}

/** The first turns of an archived conversation, as a preview. */
function previewText(message: ArchiveDetail['preview'][number]): string {
  const parts = message.parts ?? [];
  const text = parts
    .map((part) => {
      if (part.type === 'text') return part.text ?? '';
      if (part.name !== undefined) return '[' + part.type + ' ' + part.name + ']';
      return '[' + part.type + ']';
    })
    .join(' ');
  return text.length > 220 ? text.slice(0, 220) + '...' : text;
}

export function ArchivesView() {
  const { t } = useI18n();
  const { setView } = useNav();
  const {
    connection,
    archives,
    archivesRoot,
    archivesEnabled,
    archivesLoading,
    archivesError,
    refreshArchives,
    getArchive,
    restoreArchive,
    deleteArchive,
    busy,
    actionError,
  } = useApp();
  const [selected, setSelected] = useState<ArchiveDetail | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);

  useEffect(() => {
    if (connection === 'online') void refreshArchives();
  }, [connection, refreshArchives]);

  const onPreview = async (id: string): Promise<void> => {
    setPreviewError(null);
    try {
      setSelected(await getArchive(id));
    } catch (cause) {
      setSelected(null);
      setPreviewError(cause instanceof Error ? cause.message : String(cause));
    }
  };

  const onRestore = async (id: string): Promise<void> => {
    const restored = await restoreArchive(id);
    // Straight into the conversation that came back: a restore nobody follows up on is a restore
    // that looks like nothing happened.
    if (restored !== null) setView('chat');
  };

  return (
    <div className="stack">
      <ApiErrorBanner error={actionError} scope="archive" onRetry={() => void refreshArchives()} />
      <Panel
        title={t('archives.title')}
        subtitle={t('archives.subtitle', { root: archivesRoot || t('archives.unknownRoot') })}
      >
        <div className="row-actions">
          <button type="button" className="btn btn-small" disabled={archivesLoading} onClick={() => void refreshArchives()}>
            {archivesLoading ? t('common.loading') : t('common.refresh')}
          </button>
          {!archivesEnabled ? <Badge tone="muted">{t('archives.disabled')}</Badge> : null}
        </div>
        {archivesError !== null ? (
          <ApiErrorBanner error={archivesError} scope="GET /v1/archives" onRetry={() => void refreshArchives()} />
        ) : null}
        {archives.length === 0 && !archivesLoading && archivesError === null ? (
          <p className="muted">{t('archives.empty')}</p>
        ) : null}
        {archives.length > 0 ? (
          <table className="table">
            <thead>
              <tr>
                <th>{t('archives.conversation')}</th>
                <th>{t('archives.owner')}</th>
                <th>{t('archives.contents')}</th>
                <th>{t('archives.size')}</th>
                <th>{t('archives.archivedAt')}</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {archives.map((entry) => (
                <tr key={entry.id} className={selected?.id === entry.id ? 'row-selected' : undefined}>
                  <td>
                    <div>{entry.manifest.title}</div>
                    <div className="muted small">{entry.manifest.session_id}</div>
                  </td>
                  <td>
                    <CopyableText value={ownerLabel(entry)} display={ownerUser(entry)} />
                  </td>
                  <td className="muted small">
                    {t('archives.counts', {
                      runs: entry.manifest.runs,
                      messages: entry.manifest.messages,
                      artifacts: entry.manifest.artifacts,
                    })}
                  </td>
                  <td>{formatBytes(entry.bytes)}</td>
                  <td>{formatDateTime(entry.manifest.archived_at)}</td>
                  <td className="cell-actions">
                    <button type="button" className="btn btn-small" onClick={() => void onPreview(entry.id)}>
                      {t('archives.preview')}
                    </button>
                    <button
                      type="button"
                      className="btn btn-small"
                      disabled={busy}
                      onClick={() => void onRestore(entry.id)}
                    >
                      {t('archives.restore')}
                    </button>
                    {confirming === entry.id ? (
                      <>
                        <button
                          type="button"
                          className="btn btn-danger btn-small"
                          disabled={busy}
                          onClick={() => {
                            setConfirming(null);
                            void deleteArchive(entry.id).then(() => setSelected(null));
                          }}
                        >
                          {t('archives.confirmDelete')}
                        </button>
                        <button type="button" className="btn btn-ghost btn-small" onClick={() => setConfirming(null)}>
                          {t('common.cancel')}
                        </button>
                      </>
                    ) : (
                      <button
                        type="button"
                        className="btn btn-ghost btn-small"
                        onClick={() => setConfirming(entry.id)}
                      >
                        {t('common.delete')}
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : null}
      </Panel>

      <Panel title={t('archives.previewTitle')} subtitle={t('archives.previewHint')}>
        {previewError !== null ? <p className="error">{previewError}</p> : null}
        {selected === null ? (
          <p className="muted">{t('archives.pickOne')}</p>
        ) : (
          <div className="stack-tight">
            <p className="muted small">
              {t('archives.pathLine', { path: selected.path, bytes: formatBytes(selected.bytes) })}
            </p>
            <p className="muted small">
              {t('archives.filesLine', { files: Object.keys(selected.manifest.files).length })}
            </p>
            <ul className="preview-list">
              {selected.preview.map((message, index) => (
                <li key={index} className={message.role === 'user' ? 'preview-user' : 'preview-agent'}>
                  <span className="muted small">{message.role}</span> {previewText(message)}
                </li>
              ))}
            </ul>
          </div>
        )}
      </Panel>
    </div>
  );
}
