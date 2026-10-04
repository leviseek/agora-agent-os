/**
 * Presentation helpers shared by every view. Pure functions, no React.
 *
 * The active locale is module state rather than a parameter: every call site would otherwise have
 * to thread it through, and there is exactly one UI language at a time. I18nProvider sets it.
 */

let activeLocale = 'en-US';

export function setFormatLocale(locale: string): void {
  activeLocale = locale;
}

export function getFormatLocale(): string {
  return activeLocale;
}

export function formatTime(ts: number | null | undefined): string {
  if (ts === null || ts === undefined || !Number.isFinite(ts)) return '--';
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return '--';
  return date.toLocaleTimeString(activeLocale, { hour12: false }) + '.' + String(date.getMilliseconds()).padStart(3, '0');
}

export function formatDateTime(ts: number | null | undefined): string {
  if (ts === null || ts === undefined || !Number.isFinite(ts)) return '--';
  const date = new Date(ts);
  if (Number.isNaN(date.getTime())) return '--';
  return date.toLocaleString(activeLocale, { hour12: false });
}

export function formatDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return '--';
  if (ms < 1000) return Math.round(ms) + ' ms';
  if (ms < 60_000) return (ms / 1000).toFixed(2) + ' s';
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.round((ms % 60_000) / 1000);
  return minutes + ' m ' + seconds + ' s';
}

export function formatUptime(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return '--';
  const totalSeconds = Math.floor(ms / 1000);
  const days = Math.floor(totalSeconds / 86_400);
  const hours = Math.floor((totalSeconds % 86_400) / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  if (days > 0) return days + 'd ' + hours + 'h ' + minutes + 'm';
  if (hours > 0) return hours + 'h ' + minutes + 'm ' + seconds + 's';
  if (minutes > 0) return minutes + 'm ' + seconds + 's';
  return seconds + 's';
}

export function formatBytes(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || !Number.isFinite(bytes)) return '--';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return (unit === 0 ? String(Math.round(value)) : value.toFixed(1)) + ' ' + units[unit];
}

export function shortId(id: string | null | undefined, keep = 8): string {
  if (id === null || id === undefined || id.length === 0) return '--';
  if (id.length <= keep + 4) return id;
  return id.slice(0, keep) + '...';
}

/** The socket URL carries the token as ?token=; never render it in full. */
export function maskToken(text: string, token: string): string {
  if (token.length === 0) return text;
  return text.split(token).join('<token>');
}

export function severityRank(severity: string): number {
  switch (severity) {
    case 'debug':
      return 0;
    case 'info':
      return 1;
    case 'warn':
      return 2;
    case 'error':
      return 3;
    default:
      return 1;
  }
}

export function percent(part: number, whole: number): string {
  if (!Number.isFinite(part) || !Number.isFinite(whole) || whole <= 0) return '0%';
  return Math.round((part / whole) * 100) + '%';
}

/** Metrics text is rendered line by line so comments and samples stay readable. */
export function metricLines(text: string): string[] {
  return text.split(/\r?\n/).filter((line) => line.trim().length > 0);
}