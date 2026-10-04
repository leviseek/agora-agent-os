/** View 6 - Capabilities: discovery table plus a JSON invoke box. */

import { useMemo, useState } from 'react';
import { ApiError, isRecord } from '../api';
import type { CapabilityDescriptor, InvokeResponse } from '../api';
import { ApiErrorBanner, Badge, EmptyState, JsonBlock, KeyValue, Loading, Mono, Panel } from '../components';
import type { Tone } from '../components';
import { useAsyncData } from '../hooks';
import { useI18n } from '../i18n';
import { useApp } from '../store';

function healthTone(health: string): Tone {
  switch (health) {
    case 'healthy':
      return 'ok';
    case 'degraded':
      return 'warn';
    case 'unavailable':
      return 'error';
    default:
      return 'muted';
  }
}

/** Permission flags are machine identifiers; the empty set gets a localized label instead. */
function permissionSummary(capability: CapabilityDescriptor, pureLabel: string): string {
  const permission = capability.permission;
  const flags: string[] = [];
  if (permission.fs_read) flags.push('fs_read');
  if (permission.fs_write) flags.push('fs_write');
  if (permission.network) flags.push('network');
  if (permission.process_exec) flags.push('process_exec');
  if (permission.secret_names.length > 0) flags.push('secrets:' + permission.secret_names.join('|'));
  return flags.length === 0 ? pureLabel : flags.join(' + ');
}

function providerText(provider: unknown): string {
  if (typeof provider === 'string') return provider;
  if (isRecord(provider)) {
    const entries = Object.entries(provider);
    if (entries.length === 0) return '{}';
    return entries.map(([key, value]) => key + '=' + JSON.stringify(value)).join(', ');
  }
  return String(provider);
}

/** Build a usable starter document from the capability's JSON Schema. */
function templateFromSchema(schema: unknown): string {
  if (!isRecord(schema)) return '{\n}';
  const properties = schema['properties'];
  if (!isRecord(properties)) return '{\n}';
  const draft: Record<string, unknown> = {};
  for (const [key, raw] of Object.entries(properties)) {
    if (!isRecord(raw)) {
      draft[key] = '';
      continue;
    }
    const examples = raw['examples'];
    if (Array.isArray(examples) && examples.length > 0) {
      draft[key] = examples[0];
      continue;
    }
    if (raw['default'] !== undefined) {
      draft[key] = raw['default'];
      continue;
    }
    const type = raw['type'];
    if (type === 'number' || type === 'integer') draft[key] = 0;
    else if (type === 'boolean') draft[key] = false;
    else if (type === 'array') draft[key] = [];
    else if (type === 'object') draft[key] = {};
    else draft[key] = '';
  }
  return JSON.stringify(draft, null, 2);
}

export function CapabilitiesView() {
  const { t, tState } = useI18n();
  const { client } = useApp();
  const [draftQuery, setDraftQuery] = useState({ q: '', tags: '' });
  const [query, setQuery] = useState({ q: '', tags: '' });
  const [selectedName, setSelectedName] = useState<string | null>(null);
  const [inputText, setInputText] = useState('{\n}');
  const [result, setResult] = useState<InvokeResponse | null>(null);
  const [invokeError, setInvokeError] = useState<ApiError | null>(null);
  const [invoking, setInvoking] = useState(false);

  const capabilities = useAsyncData<CapabilityDescriptor[]>(
    () =>
      client
        .listCapabilities({ q: query.q.trim(), tags: query.tags.trim() })
        .then((response) => response.capabilities),
    [client, query.q, query.tags],
  );

  const list = capabilities.data ?? [];
  const selected = useMemo(
    () => list.find((capability) => capability.name === selectedName) ?? null,
    [list, selectedName],
  );

  const onSelect = (capability: CapabilityDescriptor): void => {
    setSelectedName(capability.name);
    setInputText(templateFromSchema(capability.input_schema));
    setResult(null);
    setInvokeError(null);
  };

  const onInvoke = async (): Promise<void> => {
    if (selected === null) return;
    let parsed: unknown;
    try {
      parsed = JSON.parse(inputText.length === 0 ? '{}' : inputText);
    } catch (cause) {
      setInvokeError(
        new ApiError({
          code: 'invalid_json',
          message: t('capabilities.invalidJsonDetail', {
            reason: cause instanceof Error ? cause.message : String(cause),
          }),
        }),
      );
      return;
    }
    setInvoking(true);
    setInvokeError(null);
    setResult(null);
    try {
      const response = await client.invokeCapability(selected.name, parsed);
      setResult(response);
    } catch (cause) {
      setInvokeError(cause instanceof ApiError ? cause : new ApiError({ code: 'invoke_failed', message: String(cause) }));
    } finally {
      setInvoking(false);
    }
  };

  return (
    <div className="stack">
      <Panel
        title={t('capabilities.title')}
        subtitle={t('capabilities.subtitle')}
        flush
        actions={
          <button type="button" className="btn btn-ghost btn-small" onClick={capabilities.reload}>
            {capabilities.loading ? t('common.refreshing') : t('common.refresh')}
          </button>
        }
      >
        <form
          className="form-grid form-grid-inline filter-bar"
          onSubmit={(event) => {
            event.preventDefault();
            setQuery({ q: draftQuery.q, tags: draftQuery.tags });
          }}
        >
          <label className="field">
            <span>{t('capabilities.search')}</span>
            <input
              type="text"
              value={draftQuery.q}
              placeholder={t('capabilities.searchPlaceholder')}
              onChange={(event) => setDraftQuery({ ...draftQuery, q: event.target.value })}
            />
          </label>
          <label className="field">
            <span>{t('capabilities.tags')}</span>
            <input
              type="text"
              value={draftQuery.tags}
              placeholder={t('capabilities.tagsPlaceholder')}
              onChange={(event) => setDraftQuery({ ...draftQuery, tags: event.target.value })}
            />
          </label>
          <button type="submit" className="btn">
            {t('common.search')}
          </button>
          <button
            type="button"
            className="btn btn-ghost"
            onClick={() => {
              setDraftQuery({ q: '', tags: '' });
              setQuery({ q: '', tags: '' });
            }}
          >
            {t('common.clear')}
          </button>
        </form>

        <ApiErrorBanner error={capabilities.error} scope="GET /v1/capabilities" onRetry={capabilities.reload} />

        {list.length === 0 ? (
          capabilities.loading ? (
            <Loading label={t('capabilities.loading')} />
          ) : (
            <EmptyState title={t('capabilities.empty')} hint={t('capabilities.emptyHint')} />
          )
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>{t('common.name')}</th>
                <th>{t('common.version')}</th>
                <th>{t('common.kind')}</th>
                <th>{t('capabilities.health')}</th>
                <th>{t('capabilities.tags')}</th>
                <th>{t('capabilities.permission')}</th>
                <th>{t('capabilities.provider')}</th>
                <th>{t('capabilities.timeout')}</th>
                <th>{t('capabilities.load')}</th>
              </tr>
            </thead>
            <tbody>
              {list.map((capability) => (
                <tr
                  key={capability.id}
                  className={capability.name === selectedName ? 'row-selected' : undefined}
                  onClick={() => onSelect(capability)}
                >
                  <td>
                    <strong>{capability.name}</strong>
                    <div className="muted small">{capability.description}</div>
                  </td>
                  <td>{capability.version}</td>
                  <td>
                    <Badge tone="muted">{capability.kind}</Badge>
                  </td>
                  <td>
                    <Badge tone={healthTone(capability.health)}>{tState(capability.health)}</Badge>
                  </td>
                  <td>{capability.tags.length === 0 ? '--' : capability.tags.join(', ')}</td>
                  <td className="cell-clip">{permissionSummary(capability, t('capabilities.pure'))}</td>
                  <td className="cell-clip">{providerText(capability.provider)}</td>
                  <td>{capability.timeout_ms + ' ms'}</td>
                  <td>
                    {capability.load === null
                      ? '--'
                      : t('capabilities.loadDetail', {
                          inflight: capability.load.inflight,
                          calls: capability.load.total_calls,
                          latency: capability.load.avg_latency_ms.toFixed(1),
                        })}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      <Panel
        title={t('capabilities.invoke')}
        subtitle={
          selected === null
            ? t('capabilities.invokeHint')
            : t('capabilities.invokePath', { name: selected.name })
        }
        actions={
          <>
            {selected !== null ? <Mono title={selected.id}>{selected.id}</Mono> : null}
            <button
              type="button"
              className="btn"
              disabled={selected === null || invoking}
              onClick={() => void onInvoke()}
            >
              {invoking ? t('capabilities.invoking') : t('capabilities.invoke')}
            </button>
          </>
        }
      >
        {selected === null ? (
          <p className="muted">{t('capabilities.noSelection')}</p>
        ) : (
          <>
            <KeyValue
              rows={[
                [t('common.name'), selected.name],
                [t('common.version'), selected.version],
                [t('common.kind'), selected.kind],
                [t('capabilities.idempotent'), selected.idempotent ? t('common.yes') : t('common.no')],
                [t('capabilities.permission'), permissionSummary(selected, t('capabilities.pure'))],
                [t('capabilities.timeout'), String(selected.timeout_ms)],
              ]}
            />
            <h3 className="section-title">{t('capabilities.input')}</h3>
            <textarea
              className="json-input"
              rows={8}
              spellCheck={false}
              value={inputText}
              onChange={(event) => setInputText(event.target.value)}
            />
            <details className="raw">
              <summary>input_schema</summary>
              <JsonBlock value={selected.input_schema} />
            </details>
            {invokeError !== null ? (
              <ApiErrorBanner error={invokeError} scope={t('capabilities.invoke')} onRetry={() => void onInvoke()} />
            ) : null}
            {result !== null ? (
              <>
                <h3 className="section-title">{t('capabilities.output')}</h3>
                <KeyValue
                  rows={[
                    [t('common.capability'), result.capability + '@' + result.version],
                    [t('common.duration'), String(result.duration_ms)],
                    [t('common.attempts'), String(result.attempts)],
                    [t('capabilities.provider'), providerText(result.provider)],
                  ]}
                />
                <JsonBlock value={result.output} empty={t('capabilities.noOutput')} />
              </>
            ) : null}
          </>
        )}
      </Panel>
    </div>
  );
}
