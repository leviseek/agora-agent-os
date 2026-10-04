/**
 * The view switcher: a tiny hash-free router.
 *
 * App renders one view at a time; views can jump to another view (for example Sessions
 * sends you to Chat after selecting a session) without threading callbacks through props.
 *
 * Definitions carry translation keys, never display text: the label has to change when the
 * language changes, and a component that captured a string at module load could not do that.
 */

import { createContext, useContext, useMemo, useState } from 'react';
import type { ReactNode } from 'react';
import type { MessageKey } from './i18n';

export type ViewKey =
  | 'connection'
  | 'sessions'
  | 'chat'
  | 'agent'
  | 'graph'
  | 'capabilities'
  | 'topology'
  | 'events'
  | 'settings';

export interface ViewDefinition {
  key: ViewKey;
  labelKey: MessageKey;
  hintKey: MessageKey;
}

export const VIEWS: ViewDefinition[] = [
  { key: 'connection', labelKey: 'nav.connection', hintKey: 'nav.connection.hint' },
  { key: 'sessions', labelKey: 'nav.sessions', hintKey: 'nav.sessions.hint' },
  { key: 'chat', labelKey: 'nav.chat', hintKey: 'nav.chat.hint' },
  { key: 'agent', labelKey: 'nav.agent', hintKey: 'nav.agent.hint' },
  { key: 'graph', labelKey: 'nav.graph', hintKey: 'nav.graph.hint' },
  { key: 'capabilities', labelKey: 'nav.capabilities', hintKey: 'nav.capabilities.hint' },
  { key: 'topology', labelKey: 'nav.topology', hintKey: 'nav.topology.hint' },
  { key: 'events', labelKey: 'nav.events', hintKey: 'nav.events.hint' },
  { key: 'settings', labelKey: 'nav.settings', hintKey: 'nav.settings.hint' },
];

export interface NavValue {
  view: ViewKey;
  setView: (next: ViewKey) => void;
}

const NavContext = createContext<NavValue | null>(null);

export function NavProvider({ children }: { children: ReactNode }) {
  const [view, setView] = useState<ViewKey>('connection');
  const value = useMemo<NavValue>(() => ({ view, setView }), [view]);
  return <NavContext.Provider value={value}>{children}</NavContext.Provider>;
}

export function useNav(): NavValue {
  const value = useContext(NavContext);
  if (value === null) throw new Error('useNav() must be used inside <NavProvider>');
  return value;
}
