/** View 1 - Connection / login: gateway URL, token, /healthz and /v1/meta. */

import { useEffect, useState } from 'react';
import { ApiErrorBanner, Badge, JsonBlock, KeyValue, Panel } from '../components';
import { formatUptime, maskToken } from '../format';
import { useI18n } from '../i18n';
import { useApp } from '../store';

export function ConnectionView() {
  const { t } = useI18n();
  const {
    baseUrl,
    token,
    setBaseUrl,
    setToken,
    connect,
    disconnect,
    connection,
    connectionError,
    health,
    meta,
    login,
    client,
  } = useApp();

  const [draftBaseUrl, setDraftBaseUrl] = useState(baseUrl);
  const [draftToken, setDraftToken] = useState(token);
  const [revealToken, setRevealToken] = useState(false);

  useEffect(() => {
    setDraftBaseUrl(baseUrl);
  }, [baseUrl]);
  useEffect(() => {
    setDraftToken(token);
  }, [token]);

  const applyDrafts = (): void => {
    setBaseUrl(draftBaseUrl.trim());
    setToken(draftToken);
  };

  const onConnect = (): void => {
    applyDrafts();
    void connect({ baseUrl: draftBaseUrl.trim(), token: draftToken });
  };

  const onDisconnect = (): void => {
    disconnect();
    setDraftToken('');
  };

  // The socket URL carries ?token=... when a token is set: mask it before rendering.
  const socketUrl = maskToken(client.wsUrl(meta === null ? null : meta.ws_path), token);

  return (
    <div className="stack">
      <Panel
        title={t('connection.title')}
        subtitle={t('connection.baseUrlHint')}
        actions={
          <>
            <Badge tone={connection === 'online' ? 'ok' : connection === 'connecting' ? 'warn' : 'error'}>
              {t('connection.status.' + connection)}
            </Badge>
            <button type="button" className="btn" onClick={onConnect} disabled={connection === 'connecting'}>
              {connection === 'connecting' ? t('connection.connecting') : t('connection.connect')}
            </button>
            <button type="button" className="btn btn-ghost" onClick={onDisconnect}>
              {t('connection.disconnect')}
            </button>
          </>
        }
      >
        <div className="form-grid">
          <label className="field">
            <span>{t('connection.baseUrl')}</span>
            <input
              type="text"
              value={draftBaseUrl}
              placeholder={t('connection.baseUrlPlaceholder')}
              onChange={(event) => setDraftBaseUrl(event.target.value)}
              spellCheck={false}
            />
          </label>
          <label className="field">
            <span>{t('connection.token')}</span>
            <div className="field-inline">
              <input
                type={revealToken ? 'text' : 'password'}
                value={draftToken}
                placeholder={t('connection.tokenHint')}
                onChange={(event) => setDraftToken(event.target.value)}
                spellCheck={false}
                autoComplete="off"
              />
              <button type="button" className="btn btn-ghost btn-small" onClick={() => setRevealToken(!revealToken)}>
                {revealToken ? t('common.hide') : t('common.show')}
              </button>
            </div>
          </label>
        </div>
        <p className="muted small">
          {t('connection.storageNote', { tokenKey: 'agentos.token', baseUrlKey: 'agentos.baseUrl' })}
        </p>

        {connectionError !== null ? (
          <ApiErrorBanner error={connectionError} scope={t('connection.connect')} onRetry={onConnect} />
        ) : null}

        {login !== null ? (
          <div className="banner banner-ok">
            <div className="banner-head">
              <Badge tone={login.auth_required ? 'ok' : 'info'}>
                {login.auth_required ? t('connection.tokenAccepted') : t('connection.openNode')}
              </Badge>
              {login.note !== undefined ? <span className="banner-scope">{login.note}</span> : null}
            </div>
          </div>
        ) : null}
      </Panel>

      <Panel title="/healthz" subtitle={t('connection.healthHint')}>
        {health === null ? (
          <p className="muted">{t('connection.noResponse')}</p>
        ) : (
          <KeyValue
            rows={[
              ['status', <Badge tone={health.status === 'ok' ? 'ok' : 'warn'}>{health.status}</Badge>],
              ['service', health.service],
              ['domain_version', health.domain_version],
            ]}
          />
        )}
      </Panel>

      <Panel title="/v1/meta" subtitle={t('connection.metaHint')}>
        {meta === null ? (
          <p className="muted">{t('connection.noResponse')}</p>
        ) : (
          <>
            <KeyValue
              rows={[
                ['node', meta.node],
                ['region', meta.region ?? '--'],
                ['domain_version', meta.domain_version],
                ['uptime_ms', formatUptime(meta.uptime_ms) + ' (' + meta.uptime_ms + ' ms)'],
                ['auth_required', meta.auth_required ? t('common.yes') : t('common.no')],
                ['store_backend', meta.store_backend],
                ['blob_backend', meta.blob_backend ?? '--'],
                ['ws_path', meta.ws_path],
                ['socket_url', socketUrl],
                ['grpc_addr', meta.grpc_addr],
                ['p2p_enabled', meta.p2p_enabled ? t('common.yes') : t('common.no')],
                ['transfer', typeof meta.transfer === 'string' ? meta.transfer : JSON.stringify(meta.transfer)],
                ['capabilities', String(meta.capabilities)],
                ['workers_online', String(meta.workers_online)],
                ['sessions', String(meta.sessions)],
                ['workspace_root', meta.workspace_root],
                ['limits.max_steps_per_run', String(meta.limits.max_steps_per_run)],
                ['limits.max_concurrent_tasks', String(meta.limits.max_concurrent_tasks)],
                ['limits.capability_timeout_ms', String(meta.limits.capability_timeout_ms)],
                ['limits.session_queue_capacity', String(meta.limits.session_queue_capacity)],
              ]}
            />
            <details className="raw">
              <summary>{t('common.rawJson')}</summary>
              <JsonBlock value={meta} />
            </details>
          </>
        )}
      </Panel>
    </div>
  );
}
