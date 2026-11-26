// Server-sent runtime stream: `GET /api/v1/viewer/events?format=changes&since=…`.
//
// The stream owns only transport: connect, parse frames, back off, and pause
// while the tab is hidden. The caller owns the meaning of a frame and the
// cursor it has actually applied; each (re)connect asks for that cursor, so a
// frame that failed to apply never advances the resume point.

import { UnknownInternedReferenceError, type RuntimeCursor, type ViewerFrame } from './types';

export type StreamStatus = 'idle' | 'connecting' | 'live' | 'reconnecting' | 'offline' | 'paused';

export interface ViewerEventHandlers {
  /**
   * Apply one frame. Throwing means the frame cannot be applied to the
   * current state: the stream reconnects, and asks for a fresh snapshot when
   * the error is an unknown interned reference.
   */
  onFrame: (frame: ViewerFrame) => void;
  /** The cursor of the last frame the caller applied, or `null` for a fresh snapshot. */
  getCursor: () => RuntimeCursor | null;
  onStatus: (status: StreamStatus) => void;
}

export interface StreamEnvironment {
  EventSource?: typeof EventSource;
  isVisible?: () => boolean;
  isOnline?: () => boolean;
}

export const MIN_BACKOFF_MS = 250;
export const MAX_BACKOFF_MS = 30_000;

export function cursorText(cursor: RuntimeCursor | null | undefined): string | null {
  return cursor && typeof cursor.incarnation === 'string' && Number.isInteger(cursor.sequence)
    ? `${cursor.incarnation}:${cursor.sequence}`
    : null;
}

export function eventsUrl(cursor: RuntimeCursor | null): string {
  const query = new URLSearchParams({ format: 'changes' });
  const since = cursorText(cursor);
  if (since) query.set('since', since);
  return `/api/v1/viewer/events?${query}`;
}

export class ViewerEventStream {
  private source: EventSource | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private timerFresh = false;
  private delay = MIN_BACKOFF_MS;
  private running = false;
  /** A reconnect is owed but was held back (tab hidden or browser offline). */
  private owed: { fresh: boolean } | undefined;
  private status: StreamStatus = 'idle';
  private readonly env: Required<StreamEnvironment>;

  constructor(
    private readonly handlers: ViewerEventHandlers,
    env: StreamEnvironment = {},
  ) {
    this.env = {
      EventSource: env.EventSource ?? globalThis.EventSource,
      isVisible: env.isVisible ?? (() => typeof document === 'undefined' || !document.hidden),
      isOnline: env.isOnline ?? (() => typeof navigator === 'undefined' || navigator.onLine),
    };
    this.onVisibility = this.onVisibility.bind(this);
    this.onOnline = this.onOnline.bind(this);
    this.onOffline = this.onOffline.bind(this);
  }

  /** Connect now, resuming from the caller's cursor. */
  start(): void {
    if (this.running) return;
    this.running = true;
    document.addEventListener('visibilitychange', this.onVisibility);
    window.addEventListener('online', this.onOnline);
    window.addEventListener('offline', this.onOffline);
    this.connect(false);
  }

  stop(): void {
    this.running = false;
    this.owed = undefined;
    this.clearTimer();
    this.closeSource();
    document.removeEventListener('visibilitychange', this.onVisibility);
    window.removeEventListener('online', this.onOnline);
    window.removeEventListener('offline', this.onOffline);
    this.setStatus('idle');
  }

  /** Drop the connection and reconnect now for a fresh snapshot. */
  restartFresh(): void {
    if (!this.running) return;
    this.clearTimer();
    this.closeSource();
    this.connect(true);
  }

  private setStatus(status: StreamStatus): void {
    if (this.status === status) return;
    this.status = status;
    this.handlers.onStatus(status);
  }

  private clearTimer(): void {
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.timer = undefined;
  }

  private closeSource(): void {
    this.source?.close();
    this.source = undefined;
  }

  private connect(fresh: boolean): void {
    if (!this.running) return;
    this.clearTimer();
    this.closeSource();
    this.owed = undefined;
    this.setStatus(this.status === 'live' || this.status === 'reconnecting' ? 'reconnecting' : 'connecting');
    const source = new this.env.EventSource(eventsUrl(fresh ? null : this.handlers.getCursor()));
    this.source = source;
    let applied = false;
    source.addEventListener('open', () => {
      if (this.source !== source) return;
      this.setStatus('live');
    });
    source.addEventListener('runtime', event => {
      if (this.source !== source) return;
      try {
        this.handlers.onFrame(JSON.parse((event as MessageEvent<string>).data) as ViewerFrame);
      } catch (error) {
        console.error('Runtime stream requires a new snapshot', error);
        this.fail(source, error instanceof UnknownInternedReferenceError);
        return;
      }
      this.setStatus('live');
      if (!applied) {
        applied = true;
        this.delay = MIN_BACKOFF_MS;
      }
    });
    // Close on error so each retry uses the cursor the caller has applied;
    // EventSource's own retry would keep a stale query string.
    source.addEventListener('error', () => {
      if (this.source !== source) return;
      this.fail(source, false);
    });
  }

  private fail(source: EventSource, fresh: boolean): void {
    source.close();
    this.source = undefined;
    this.scheduleReconnect(fresh);
  }

  private scheduleReconnect(fresh: boolean): void {
    if (!this.running) return;
    if (!this.env.isOnline()) {
      this.owed = { fresh };
      this.setStatus('offline');
      return;
    }
    if (!this.env.isVisible()) {
      this.owed = { fresh };
      this.setStatus('paused');
      return;
    }
    this.setStatus('reconnecting');
    const delay = this.delay;
    this.delay = Math.min(this.delay * 2, MAX_BACKOFF_MS);
    this.clearTimer();
    this.timerFresh = fresh;
    this.timer = setTimeout(() => {
      this.timer = undefined;
      this.connect(fresh);
    }, delay);
  }

  /** Resume at once when the tab or the network returns; nothing waits out a backoff. */
  private resumeOwed(): void {
    if (!this.running || !this.owed) return;
    if (!this.env.isOnline() || !this.env.isVisible()) return;
    const { fresh } = this.owed;
    this.delay = MIN_BACKOFF_MS;
    this.connect(fresh);
  }

  private onVisibility(): void {
    if (!this.env.isVisible()) {
      // Hold a pending backoff while hidden; the open stream, if any, stays.
      if (this.timer !== undefined) {
        this.clearTimer();
        this.owed = { fresh: this.timerFresh };
        this.setStatus('paused');
      }
      return;
    }
    this.resumeOwed();
  }

  private onOnline(): void {
    this.resumeOwed();
  }

  private onOffline(): void {
    if (!this.running) return;
    // A connection open when the browser goes offline may never report an
    // error. Close it and resume from the applied cursor when connectivity returns.
    const fresh = this.owed?.fresh ?? (this.timer !== undefined && this.timerFresh);
    this.clearTimer();
    this.closeSource();
    this.owed = { fresh };
    this.setStatus('offline');
  }
}
