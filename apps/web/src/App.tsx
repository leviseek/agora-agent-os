/** Console shell: sidebar view switcher + one main panel per view. */

import { ApiErrorBanner, Badge, StatusDetailText, ViewBoundary } from './components';
import { I18nProvider, useI18n } from './i18n';
import { NavProvider, VIEWS, useNav } from './navigation';
import { AppProvider, useApp } from './store';
import { ShellSwitches } from './switches';
import { ThemeProvider } from './theme';
import { ConnectionView } from './views/ConnectionView';
import { WorkspacesView } from './views/WorkspacesView';
import { SessionsView } from './views/SessionsView';
import { ChatView } from './views/ChatView';
import { AgentStateView } from './views/AgentStateView';
import { TaskGraphView } from './views/TaskGraphView';
import { ApprovalsView } from './views/ApprovalsView';
import { ArchivesView } from './views/ArchivesView';
import { AccessView } from './views/AccessView';
import { CapabilitiesView } from './views/CapabilitiesView';
import { TopologyView } from './views/TopologyView';
import { EventsView } from './views/EventsView';
import { SettingsView } from './views/SettingsView';

export default function App() {
  return (
    <I18nProvider>
      <ThemeProvider>
        <AppProvider>
          <NavProvider>
            <Console />
          </NavProvider>
        </AppProvider>
      </ThemeProvider>
    </I18nProvider>
  );
}

function Console() {
  const { view, setView } = useNav();
  const { t } = useI18n();
  const {
    connection,
    meta,
    health,
    wsStatus,
    wsDetail,
    actionError,
    clearActionError,
    selectedSessionId,
    sessions,
    busy,
    accessInbox,
  } = useApp();
  const selected = sessions.find((session) => session.id === selectedSessionId) ?? null;
  const waiting = accessInbox?.to_decide.length ?? 0;
  const active = VIEWS.find((definition) => definition.key === view);

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-mark">agora</span>
          <span className="brand-sub">{t('shell.brandSub')}</span>
        </div>
        <nav className="nav">
          {VIEWS.map((definition) => (
            <button
              key={definition.key}
              type="button"
              className={definition.key === view ? 'nav-item nav-item-active' : 'nav-item'}
              onClick={() => setView(definition.key)}
              title={t(definition.hintKey)}
            >
              <span className="nav-label">
                {t(definition.labelKey)}
                {definition.key === 'access' && waiting > 0 ? (
                  // What needs a decision, on the tab itself: a page that has to be opened before it
                  // can tell you it has work is a page that hides work.
                  <span className="nav-badge" title={t('nav.access.waiting', { n: waiting })}>
                    {waiting}
                  </span>
                ) : null}
              </span>
              <span className="nav-hint">{t(definition.hintKey)}</span>
            </button>
          ))}
        </nav>
        <footer className="sidebar-footer">
          <ShellSwitches />
          <div className="status-line">
            <span className={'dot dot-' + connection} />
            <span className="muted">{meta !== null ? meta.node : t('shell.notConnected')}</span>
          </div>
          <div className="status-line">
            <span className={'dot dot-ws-' + wsStatus} />
            <span className="muted">{t('shell.wsPrefix') + t('ws.status.' + wsStatus)}</span>
          </div>
          {busy ? <span className="muted small">{t('shell.working')}</span> : null}
          {health !== null ? (
            <span className="muted small">{t('shell.domainPrefix') + health.domain_version}</span>
          ) : null}
          {selected !== null ? (
            <span className="muted small">{t('shell.sessionPrefix') + selected.title}</span>
          ) : null}
        </footer>
      </aside>

      <main className="main">
        <header className="topbar">
          <div className="topbar-title">
            <h1>{active !== undefined ? t(active.labelKey) : t('shell.console')}</h1>
            <p className="muted">{active !== undefined ? t(active.hintKey) : ''}</p>
          </div>
          <div className="topbar-meta">
            <Badge tone={connection === 'online' ? 'ok' : connection === 'connecting' ? 'warn' : 'error'}>
              {t('connection.status.' + connection)}
            </Badge>
            <Badge tone={wsStatus === 'open' ? 'ok' : wsStatus === 'reconnecting' ? 'warn' : 'muted'}>
              {'ws: ' + t('ws.status.' + wsStatus)}
            </Badge>
            {selected !== null ? (
              <Badge tone="info">{selected.title}</Badge>
            ) : (
              <Badge tone="muted">{t('shell.noSession')}</Badge>
            )}
          </div>
        </header>

        {wsDetail !== null && wsStatus !== 'open' ? (
          <p className="muted small ws-detail">
            <StatusDetailText detail={wsDetail} />
          </p>
        ) : null}

        {actionError !== null ? (
          <div className="action-error">
            <ApiErrorBanner error={actionError} scope={t('shell.lastAction')} />
            <button type="button" className="btn btn-ghost btn-small" onClick={clearActionError}>
              {t('shell.dismiss')}
            </button>
          </div>
        ) : null}

        <div className="view">
          {connection !== 'online' && view !== 'connection' ? (
            <div className="banner banner-warn">
              <p>{t('shell.offlineBanner')}</p>
            </div>
          ) : null}
          <ViewBoundary resetKey={view}>{renderView(view)}</ViewBoundary>
        </div>
      </main>
    </div>
  );
}

function renderView(view: string) {
  switch (view) {
    case 'connection':
      return <ConnectionView />;
    case 'workspaces':
      return <WorkspacesView />;
    case 'sessions':
      return <SessionsView />;
    case 'chat':
      return <ChatView />;
    case 'agent':
      return <AgentStateView />;
    case 'graph':
      return <TaskGraphView />;
    case 'capabilities':
      return <CapabilitiesView />;
    case 'archives':
      return <ArchivesView />;
    case 'access':
      return <AccessView />;
    case 'approvals':
      return <ApprovalsView />;
    case 'topology':
      return <TopologyView />;
    case 'events':
      return <EventsView />;
    case 'settings':
      return <SettingsView />;
    default:
      return null;
  }
}