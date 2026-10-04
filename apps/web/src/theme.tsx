/**
 * Theme switching: light, dark, or follow the operating system.
 *
 * The palette lives entirely in CSS custom properties (see styles.css). This module only decides
 * which set is active and remembers the choice, so no component ever needs to know a colour.
 */

import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react';
import type { ReactNode } from 'react';

export type ThemeMode = 'light' | 'dark' | 'system';
export type ResolvedTheme = 'light' | 'dark';

const STORAGE_KEY = 'agentos.theme';
const MODES: ThemeMode[] = ['light', 'dark', 'system'];

export function isThemeMode(value: unknown): value is ThemeMode {
  return typeof value === 'string' && (MODES as string[]).includes(value);
}

function prefersDark(): boolean {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return true;
  return window.matchMedia('(prefers-color-scheme: dark)').matches;
}

/** Stored choice wins; otherwise follow the operating system. */
function initialMode(): ThemeMode {
  try {
    const stored = window.localStorage.getItem(STORAGE_KEY);
    if (isThemeMode(stored)) return stored;
  } catch {
    // A blocked localStorage (private mode, embedded webview) must not break the console.
  }
  return 'system';
}

function resolve(mode: ThemeMode): ResolvedTheme {
  if (mode === 'system') return prefersDark() ? 'dark' : 'light';
  return mode;
}

export interface ThemeValue {
  /** What the user picked. */
  mode: ThemeMode;
  /** What is actually rendered right now. */
  theme: ResolvedTheme;
  setMode: (next: ThemeMode) => void;
  /** Flip between light and dark, leaving "system" behind. */
  toggle: () => void;
}

const ThemeContext = createContext<ThemeValue | null>(null);

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [mode, setModeState] = useState<ThemeMode>(() => initialMode());
  const [theme, setTheme] = useState<ResolvedTheme>(() => resolve(initialMode()));

  // Apply the resolved theme to the document root: every selector keys off [data-theme].
  useEffect(() => {
    const next = resolve(mode);
    setTheme(next);
    const root = document.documentElement;
    root.dataset.theme = next;
    root.style.colorScheme = next;
  }, [mode]);

  // While following the system, react to the OS switching appearance.
  useEffect(() => {
    if (mode !== 'system' || typeof window.matchMedia !== 'function') return undefined;
    const query = window.matchMedia('(prefers-color-scheme: dark)');
    const listener = () => {
      const next = resolve('system');
      setTheme(next);
      document.documentElement.dataset.theme = next;
      document.documentElement.style.colorScheme = next;
    };
    query.addEventListener('change', listener);
    return () => query.removeEventListener('change', listener);
  }, [mode]);

  const setMode = useCallback((next: ThemeMode) => {
    setModeState(next);
    try {
      window.localStorage.setItem(STORAGE_KEY, next);
    } catch {
      // Preference is best-effort; the session still works without persistence.
    }
  }, []);

  const toggle = useCallback(() => {
    setMode(resolve(mode) === 'dark' ? 'light' : 'dark');
  }, [mode, setMode]);

  const value = useMemo<ThemeValue>(() => ({ mode, theme, setMode, toggle }), [mode, theme, setMode, toggle]);
  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}

export function useTheme(): ThemeValue {
  const value = useContext(ThemeContext);
  if (value === null) throw new Error('useTheme() must be used inside <ThemeProvider>');
  return value;
}
