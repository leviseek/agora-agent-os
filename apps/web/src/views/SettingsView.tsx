/** View 9 - Settings: appearance, the socket, /v1/meta, /v1/models and /v1/metrics. */

import { useState } from 'react';
import { ApiErrorBanner, Badge, JsonBlock, KeyValue, Panel, StatusDetailText } from '../components';
import { formatDuration, formatUptime, maskToken, metricLines } from '../format';
import { useAsyncData } from '../hooks';
import { useI18n } from '../i18n';
import { useApp } from '../store';
import { AppearanceSettings } from '../switches';
import type { ModelsResponse } from '../api';

export function SettingsView() {
  const { t, tState } = useI18n();
  const {
    client,
    meta,
    health,
    connection,
    wsStatus,
    wsDetail,
    lastSocketMessage,
    autoReconnect,
    setAutoReconnect,
    pingSocket,
    reconnectSocket,
    baseUrl,
    token,
  } = useApp();
  const [autoMetrics, setAutoMetrics] = useState(false);

  const models = useAsyncData<ModelsResponse>(() => client.listModels(), [client]);
  const metrics = useAsyncData<string>(
    () => client.metrics(),
    [client, autoMetrics],
    autoMetrics ? { intervalMs: 5000 } : undefined,
  );

  const lines = metrics.data === null ? [] : metricLines(metrics.data);

  return (
    <div className="stack">
      <Panel title={t('settings.appearance')} subtitle={t('settings.appearanceHint')}>
        <AppearanceSettings />
      </Panel>

      <Panel
        title={t('settings.socket')}
        subtitle={t('settings.socketHint')}
        actions={
          <>
            <Badge tone={wsStatus === 'open' ? 'ok' : wsStatus === 'reconnecting' ? 'warn' : 'error'}>
              {'ws ' + t('ws.status.' + wsStatus)}
            </Badge>
            <button type="button" className="btn btn-small" onClick={reconnectSocket}>
              {t('settings.reconnect')}
            </button>
            <button type="button" className="btn btn-ghost btn-small" onClick={pingSocket}>
              {t('events.ping')}
            </button>
          </>
        }
      >
        <label className="checkbox">
          <input
            type="checkbox"
            checked={autoReconnect}
            onChange={(event) => setAutoReconnect(event.target.checked)}
          />
          <span>{t('settings.autoReconnect')}</span>
        </label>
        <KeyValue
          rows={[
            [t('settings.connectionLabel'), connection],
            ['base_url', baseUrl.trim().length > 0 ? baseUrl : t('settings.sameOrigin')],
            ['socket_url', maskToken(client.wsUrl(meta === null ? null : meta.ws_path), token)],
            [t('settings.socketDetail'), wsDetail !== null ? <StatusDetailText detail={wsDetail} /> : '--'],
            [
              t('settings.lastFrame'),
              lastSocketMessage === null
                ? t('settings.noFrame')
                : lastSocketMessage.type + ' - ' + JSON.stringify(lastSocketMessage).slice(0, 240),
            ],
          ]}
        />
      </Panel>

      <Panel title={t('settings.node')} subtitle="GET /v1/meta">
        {meta === null ? (
          <p className="muted">{t('settings.notConnectedHint')}</p>
        ) : (
          <>
            <KeyValue
              rows={[
                ['node', meta.node],
                ['region', meta.region ?? '--'],
                ['domain_version', meta.domain_version],
                ['uptime', formatUptime(meta.uptime_ms)],
                ['auth_required', meta.auth_required ? t('common.yes') : t('common.no')],
                ['store_backend', meta.store_backend],
                ['blob_backend', meta.blob_backend ?? '--'],
                ['ws_path', meta.ws_path],
                ['grpc_addr', meta.grpc_addr],
                ['p2p_enabled', meta.p2p_enabled ? t('common.yes') : t('common.no')],
                ['capabilities', String(meta.capabilities)],
                ['workers_online', String(meta.workers_online)],
                ['sessions', String(meta.sessions)],
                ['workspace_root', meta.workspace_root],
              ]}
            />
            <details className="raw">
              <summary>{t('common.rawJson')}</summary>
              <JsonBlock value={meta} />
            </details>
          </>
        )}
      </Panel>

      <Panel
        title={t('settings.models')}
        subtitle={t('settings.modelsHint')}
        actions={
          <button type="button" className="btn btn-ghost btn-small" onClick={models.reload}>
            {models.loading ? t('common.refreshing') : t('common.refresh')}
          </button>
        }
        flush
      >
        <ApiErrorBanner error={models.error} scope="GET /v1/models" onRetry={models.reload} />

        {models.data === null ? (
          <p className="muted loading">{t('common.loading')}</p>
        ) : (
          <>
            <p className="muted small">
              {t('settings.defaultProviderValue', {
                name: models.data.default ?? t('settings.noneConfigured'),
              })}
            </p>
            {models.data.providers.length === 0 ? (
              <p className="muted">{t('settings.noProviders')}</p>
            ) : (
              <table className="table">
                <thead>
                  <tr>
                    <th>{t('capabilities.provider')}</th>
                    <th>{t('common.kind')}</th>
                    <th>{t('common.model')}</th>
                    <th>{t('capabilities.health')}</th>
                    <th>{t('settings.calls')}</th>
                    <th>{t('settings.failures')}</th>
                    <th>{t('settings.latency')}</th>
                    <th>{t('settings.tokens')}</th>
                  </tr>
                </thead>
                <tbody>
                  {models.data.providers.map((provider) => (
                    <tr key={provider.name}>
                      <td>{provider.name}</td>
                      <td>{String(provider.kind)}</td>
                      <td>{provider.model}</td>
                      <td>
                        <Badge tone={provider.health === 'healthy' ? 'ok' : provider.health === 'unavailable' ? 'error' : 'warn'}>
                          {tState(provider.health)}
                        </Badge>
                      </td>
                      <td>{provider.calls}</td>
                      <td>{provider.failures}</td>
                      <td>{formatDuration(provider.avg_latency_ms)}</td>
                      <td>{provider.total_tokens}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}

            <h3 className="section-title">{t('settings.configuredProviders')}</h3>
            {models.data.configured.length === 0 ? (
              <p className="muted">{t('settings.noConfigured')}</p>
            ) : (
              <table className="table">
                <thead>
                  <tr>
                    <th>{t('common.name')}</th>
                    <th>{t('common.kind')}</th>
                    <th>{t('common.model')}</th>
                    <th>{t('settings.enabled')}</th>
                    <th>{t('settings.keyEnv')}</th>
                    <th>{t('settings.configured')}</th>
                  </tr>
                </thead>
                <tbody>
                  {models.data.configured.map((provider, index) => (
                    <tr key={String(provider.name ?? index)}>
                      <td>{provider.name ?? '--'}</td>
                      <td>{String(provider.kind ?? '--')}</td>
                      <td>{provider.model ?? '--'}</td>
                      <td>{provider.enabled === true ? t('common.yes') : t('common.no')}</td>
                      <td>{provider.key_env ?? '--'}</td>
                      <td>
                        <Badge tone={provider.configured === true ? 'ok' : 'warn'}>
                          {provider.configured === true ? t('settings.configured') : t('settings.missingKey')}
                        </Badge>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </>
        )}
      </Panel>

      <Panel
        title={t('settings.metrics')}
        subtitle={
          autoMetrics
            ? t('settings.metricsCountAuto', { n: lines.length })
            : t('settings.metricsCount', { n: lines.length })
        }
        actions={
          <>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={autoMetrics}
                onChange={(event) => setAutoMetrics(event.target.checked)}
              />
              <span>{t('settings.autoRefresh')}</span>
            </label>
            <button type="button" className="btn btn-ghost btn-small" onClick={metrics.reload}>
              {metrics.loading ? t('common.refreshing') : t('common.refresh')}
            </button>
          </>
        }
      >
        <ApiErrorBanner error={metrics.error} scope="GET /v1/metrics" onRetry={metrics.reload} />
        {metrics.data === null ? (
          <p className="muted">{t('settings.noMetrics')}</p>
        ) : (
          <pre className="metrics">{metrics.data}</pre>
        )}
      </Panel>

      {health !== null ? (
        <Panel title={t('settings.lastHealth')} subtitle={t('settings.lastHealthHint')}>
          <KeyValue
            rows={[
              ['status', health.status],
              ['service', health.service],
              ['domain_version', health.domain_version],
            ]}
          />
        </Panel>
      ) : null}
    </div>
  );
}