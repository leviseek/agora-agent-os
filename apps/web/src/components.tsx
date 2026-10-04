/** Small shared presentational pieces. No data fetching happens here. */

import { Component } from 'react';
import type { ErrorInfo, ReactNode } from 'react';
import { jsonText } from './api';
import type { ApiError } from './api';
import { tGlobal, useI18n } from './i18n';
import type { WsDetail } from './ws';

/**
 * Keeps one broken view from taking the console with it.
 *
 * React unmounts the whole tree when a render throws, which turns any bad value into a blank
 * page with no way back. The boundary shows what happened and lets the user move on: changing the
 * view resets it.
 */
export class ViewBoundary extends Component<
  { children: ReactNode; resetKey: string },
  { error: Error | null }
> {
  constructor(props: { children: ReactNode; resetKey: string }) {
    super(props);
    this.state = { error: null };
  }

  static getDerivedStateFromError(error: Error): { error: Error } {
    return { error };
  }

  override componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error('[console] view crashed', error, info.componentStack);
  }

  override componentDidUpdate(previous: { resetKey: string }): void {
    if (previous.resetKey !== this.props.resetKey && this.state.error !== null) {
      this.setState({ error: null });
    }
  }

  override render(): ReactNode {
    if (this.state.error === null) return this.props.children;
    return (
      <div className="banner banner-error">
        <div className="banner-head">
          <span className="banner-scope">{tGlobal('error.viewCrashed', { message: this.state.error.message })}</span>
        </div>
        <p className="muted small">{tGlobal('error.viewCrashedHint')}</p>
        <button type="button" className="btn btn-small" onClick={() => window.location.reload()}>
          {tGlobal('error.reload')}
        </button>
      </div>
    );
  }
}

export type Tone = 'neutral' | 'info' | 'ok' | 'warn' | 'error' | 'muted';

export function ApiErrorBanner({
  error,
  scope,
  onRetry,
}: {
  error: ApiError | null;
  scope?: string;
  onRetry?: (() => void) | undefined;
}) {
  const { t } = useI18n();
  if (error === null) return null;
  return (
    <div className="banner banner-error" role="alert">
      <div className="banner-head">
        <span className="banner-code">{error.label}</span>
        {scope !== undefined ? <span className="banner-scope">{scope}</span> : null}
        {error.retryable ? <span className="banner-tag">{t('common.retryable')}</span> : null}
        {onRetry !== undefined ? (
          <button type="button" className="btn btn-ghost btn-small" onClick={onRetry}>
            {t('common.retry')}
          </button>
        ) : null}
      </div>
      <p className="banner-message">{error.message}</p>
      {error.details !== null && error.details !== undefined ? (
        <details className="banner-details">
          <summary>{t('common.details')}</summary>
          <pre>{jsonText(error.details)}</pre>
        </details>
      ) : null}
    </div>
  );
}

export function Panel({
  title,
  subtitle,
  actions,
  children,
  flush = false,
}: {
  title: string;
  subtitle?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
  flush?: boolean;
}) {
  return (
    <section className="panel">
      <header className="panel-head">
        <div>
          <h2>{title}</h2>
          {subtitle !== undefined ? <p className="panel-subtitle">{subtitle}</p> : null}
        </div>
        {actions !== undefined ? <div className="panel-actions">{actions}</div> : null}
      </header>
      <div className={flush ? 'panel-body panel-body-flush' : 'panel-body'}>{children}</div>
    </section>
  );
}

export function EmptyState({ title, hint }: { title: string; hint?: ReactNode }) {
  return (
    <div className="empty">
      <p className="empty-title">{title}</p>
      {hint !== undefined ? <p className="empty-hint">{hint}</p> : null}
    </div>
  );
}

/**
 * Render a socket status detail. Data in, sentence out: the same detail re-renders in the new
 * language as soon as the locale changes.
 */
export function StatusDetailText({ detail }: { detail: WsDetail | null }) {
  const { t } = useI18n();
  if (detail === null) return null;
  if ('text' in detail) return <>{detail.text}</>;
  return <>{t(detail.key, detail.params)}</>;
}

export function Loading({ label }: { label?: string }) {
  const { t } = useI18n();
  return <p className="muted loading">{label ?? t('common.loading')}</p>;
}

export function Badge({ children, tone = 'neutral' }: { children: ReactNode; tone?: Tone }) {
  return <span className={'badge badge-' + tone}>{children}</span>;
}

export function JsonBlock({ value, empty }: { value: unknown; empty?: string }) {
  const { t } = useI18n();
  const fallback = empty ?? t('common.noData');
  if (value === null || value === undefined) return <p className="muted">{fallback}</p>;
  const text = jsonText(value);
  if (text.length === 0) return <p className="muted">{fallback}</p>;
  return <pre className="json">{text}</pre>;
}

export function KeyValue({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <dl className="kv">
      {rows.map(([key, value]) => (
        <div className="kv-row" key={key}>
          <dt>{key}</dt>
          <dd>{value}</dd>
        </div>
      ))}
    </dl>
  );
}

export function Mono({ children, title }: { children: ReactNode; title?: string }) {
  return (
    <code className="mono" title={title}>
      {children}
    </code>
  );
}