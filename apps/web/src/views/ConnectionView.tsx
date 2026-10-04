/** View 1 - Connection / login: gateway URL, token, /healthz and /v1/meta. */

import { useEffect, useState } from 'react';
import { ApiErrorBanner, Badge, JsonBlock, KeyValue, Panel } from '../components';
import type { NodeListResponse, NodeSummary } from '../api';
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

  // Discovery: other runtimes the connected node can see. Polled rather than pushed because the
  // advertisement TTL is seconds - a poll every few seconds is simpler than a second socket, and
  // the panel is only mounted while the user is on this view.
  const [nodes, setNodes] = useState<NodeListResponse | null>(null);
  const [nodesError, setNodesError] = useState<string | null>(null);
  const [nodesNonce, setNodesNonce] = useState(0);

  useEffect(() => {
    if (connection !== 'online') {
      setNodes(null);
      return undefined;
    }
    let cancelled = false;
    const load = async (): Promise<void> => {
      try {
        const response = await client.listNodes();
        if (!cancelled) {
          setNodes(response);
          setNodesError(null);
        }
      } catch (error) {
        if (!cancelled) setNodesError(error instanceof Error ? error.message : String(error));
      }
    };
    void load();
    const timer = window.setInterval(() => void load(), 4000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [connection, client, nodesNonce]);

  /**
   * Nodes are named by their owner, and two checkouts often carry the same name. When that happens
   * the port is what tells them apart, so it is appended to the label instead of leaving two
   * identical rows in the list.
   */
  const nodeLabel = (node: NodeSummary, all: NodeSummary[]): string => {
    const duplicates = all.filter((other) => other.name === node.name).length;
    if (duplicates < 2) return node.name;
    const port = node.address.split(':').pop();
    return port === undefined || port.length === 0 ? node.name : node.name + ':' + port;
  };

  /** Jump to another node: same console, different runtime. */
  const switchTo = (node: NodeSummary): void => {
    setDraftBaseUrl(node.address);
    setBaseUrl(node.address);
    // A node that wants a token cannot be entered silently: leave the URL in place and let the
    // user paste the token, which is exactly what pressing Connect would have required anyway.
    if (node.auth_required !== true) {
      void connect({ baseUrl: node.address, token: draftToken });
    }
  };

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

      <Panel
        title={t('nodes.title')}
        subtitle={t('nodes.subtitle')}
        actions={
          <>
            {nodes !== null ? <Badge tone="info">{t('nodes.count', { n: nodes.nodes.length })}</Badge> : null}
            <button
              type="button"
              className="btn btn-ghost btn-small"
              onClick={() => setNodesNonce(nodesNonce + 1)}
              disabled={connection !== 'online'}
            >
              {t('common.refresh')}
            </button>
          </>
        }
      >
        {connection !== 'online' ? (
          <p className="muted">{t('nodes.notConnected')}</p>
        ) : nodesError !== null ? (
          <p className="muted">{nodesError}</p>
        ) : nodes === null ? (
          <p className="muted">{t('nodes.scanning')}</p>
        ) : nodes.discovery.enabled === false ? (
          <p className="muted">{t('nodes.disabled')}</p>
        ) : (
          <>
            <div className="node-row node-row-self">
              <span className="node-name">{nodeLabel(nodes.self, [nodes.self, ...nodes.nodes])}</span>
              <span className="mono">{nodes.self.address}</span>
              <Badge tone="muted">{t('nodes.self')}</Badge>
              <span className="muted small">{t('nodes.capabilities', { n: nodes.self.capabilities?.length ?? 0 })}</span>
            </div>

            {nodes.nodes.length === 0 ? (
              <p className="muted">{t('nodes.empty')}</p>
            ) : (
              nodes.nodes.map((node) => (
                <div className="node-row" key={node.node_id}>
                  <span className="node-name">{nodeLabel(node, [nodes.self, ...nodes.nodes])}</span>
                  <span className="mono">{node.address}</span>
                  {node.auth_required === true ? <Badge tone="warn">{t('nodes.needsToken')}</Badge> : null}
                  <span className="muted small">{t('nodes.capabilities', { n: node.capabilities?.length ?? 0 })}</span>
                  <span className="muted small">
                    {t('nodes.age', { s: Math.max(0, Math.round((node.age_ms ?? 0) / 1000)) })}
                  </span>
                  <button type="button" className="btn btn-small" onClick={() => switchTo(node)}>
                    {t('nodes.use')}
                  </button>
                </div>
              ))
            )}

            <p className="muted small">
              {t('nodes.backendNote', {
                backend: nodes.discovery.backend,
                dir: nodes.discovery.dir,
                ttl: Math.round(nodes.discovery.ttl_ms / 1000),
              })}
            </p>
          </>
        )}
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
