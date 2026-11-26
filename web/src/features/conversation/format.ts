// Small, pure formatters shared by the conversation surfaces. Times are shown
// in the browser's own locale; durations and sizes are language-neutral.

/** `850` → `0.9s`, `12_400` → `12s`, `63_000` → `1m 3s`. */
export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '';
  if (ms < 1000) return `${Math.round(ms / 100) / 10}s`;
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.round((ms % 60_000) / 1000);
  return seconds ? `${minutes}m ${seconds}s` : `${minutes}m`;
}

/** A clock time such as `14:03`. */
export function formatClock(ms: number | null | undefined): string | null {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return null;
  try {
    return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  } catch {
    return null;
  }
}

/** `1536` → `1.5 KB`. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  return `${value.toFixed(digits)} ${units[unit]}`;
}

/** How long ago a start time was, e.g. `2m 5s`. */
export function formatSince(startedAtMs: number | null | undefined, now = Date.now()): string | null {
  if (startedAtMs === null || startedAtMs === undefined || !Number.isFinite(startedAtMs)) return null;
  return formatDuration(Math.max(0, now - startedAtMs));
}

export function describeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
