/**
 * Language and theme switches.
 *
 * Three shapes, one source of truth: a compact pair for the sidebar, a full row with a label and
 * a hint for the Settings view. Both read the same providers, so a change in one place is visible
 * in the other immediately.
 */

import { LOCALES, LOCALE_LABELS, useI18n } from './i18n';
import { useTheme } from './theme';
import type { ThemeMode } from './theme';

const THEME_MODES: ThemeMode[] = ['light', 'dark', 'system'];

const THEME_LABEL_KEYS: Record<ThemeMode, string> = {
  light: 'switch.theme.light',
  dark: 'switch.theme.dark',
  system: 'switch.theme.system',
};

export function LanguageSwitch() {
  const { locale, setLocale } = useI18n();
  return (
    <div className="switch-group" role="group" aria-label="language">
      {LOCALES.map((candidate) => (
        <button
          key={candidate}
          type="button"
          className={candidate === locale ? 'switch-option switch-option-active' : 'switch-option'}
          aria-pressed={candidate === locale}
          onClick={() => setLocale(candidate)}
        >
          {LOCALE_LABELS[candidate]}
        </button>
      ))}
    </div>
  );
}

export function ThemeSwitch() {
  const { mode, setMode } = useTheme();
  const { t } = useI18n();
  return (
    <div className="switch-group" role="group" aria-label="theme">
      {THEME_MODES.map((candidate) => (
        <button
          key={candidate}
          type="button"
          className={candidate === mode ? 'switch-option switch-option-active' : 'switch-option'}
          aria-pressed={candidate === mode}
          title={t(THEME_LABEL_KEYS[candidate])}
          onClick={() => setMode(candidate)}
        >
          {t(THEME_LABEL_KEYS[candidate])}
        </button>
      ))}
    </div>
  );
}

export function ShellSwitches() {
  return (
    <div className="shell-switches">
      <LanguageSwitch />
      <ThemeSwitch />
    </div>
  );
}

/** The Settings view embeds this: two labelled rows, no duplicated state. */
export function AppearanceSettings() {
  const { t } = useI18n();
  return (
    <div>
      <div className="switch-row">
        <span className="switch-row-label">
          <strong>{t('switch.language')}</strong>
          <span className="switch-row-hint">{t('switch.language.hint')}</span>
        </span>
        <LanguageSwitch />
      </div>
      <div className="switch-row">
        <span className="switch-row-label">
          <strong>{t('switch.theme')}</strong>
          <span className="switch-row-hint">{t('switch.theme.hint')}</span>
        </span>
        <ThemeSwitch />
      </div>
    </div>
  );
}
