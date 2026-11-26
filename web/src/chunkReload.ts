// A daemon upgrade replaces the hashed chunks under /assets/, so a tab that
// was open before the upgrade can fail to load a screen's chunk. The new build
// is already on the server: one reload fixes it. The reload is guarded by a
// short-lived sessionStorage stamp so an offline or broken server never loops.

export const CHUNK_RELOAD_KEY = 'mj-chunk-reload';
/** A second automatic reload inside this window is refused. */
export const CHUNK_RELOAD_WINDOW_MS = 15_000;

const CHUNK_ERROR_PATTERNS = [
  /Failed to fetch dynamically imported module/i,
  /Importing a module script failed/i,
  /error loading dynamically imported module/i,
  /Unable to preload CSS/i,
  /ChunkLoadError/i,
];

export function isChunkLoadError(error: unknown): boolean {
  const text = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
  return CHUNK_ERROR_PATTERNS.some(pattern => pattern.test(text));
}

/**
 * Reload once for a stale chunk. Returns `false`, without reloading, when a
 * reload already ran within the window.
 */
export function reloadOnceForStaleChunk(now: number = Date.now()): boolean {
  try {
    const last = Number(sessionStorage.getItem(CHUNK_RELOAD_KEY));
    if (Number.isFinite(last) && last > 0 && now - last < CHUNK_RELOAD_WINDOW_MS) return false;
    sessionStorage.setItem(CHUNK_RELOAD_KEY, String(now));
  } catch {
    // Without sessionStorage the loop guard is unavailable: show the manual reload card instead.
    return false;
  }
  location.reload();
  return true;
}

/** Vite fires this when a dynamic import's preload fails. Returns the remover. */
export function installChunkReloadListener(): () => void {
  const onPreloadError = (event: Event) => {
    if (reloadOnceForStaleChunk()) event.preventDefault();
  };
  window.addEventListener('vite:preloadError', onPreloadError);
  return () => window.removeEventListener('vite:preloadError', onPreloadError);
}
