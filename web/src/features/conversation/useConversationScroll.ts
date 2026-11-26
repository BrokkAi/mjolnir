import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { markConversationRead } from '../../api/client';

/**
 * The end-follow state machine.
 *
 * The browser's own scroll anchoring is disabled on the scroller
 * (`overflow-anchor: none`), so every correction happens here, synchronously,
 * in a layout effect. The conversation is either following the end or detached;
 * while following, auto-scroll can additionally be *paused* because a reply
 * taller than the viewport is streaming and its beginning must stay readable.
 *
 * Nothing in this hook touches the transcript store: it only measures the
 * scroller and moves it, which is what makes the state transitions testable
 * with a plain fixed-size fake element.
 */

/** Inside this distance from the end the reader counts as following. */
export const BOTTOM_BAND_PX = 32;
/** A read receipt waits for scrolling to settle before it is sent. */
export const READ_RECEIPT_DELAY_MS = 500;
/** Streaming Markdown re-renders at most this often. */
export const STREAM_MARKDOWN_INTERVAL_MS = 100;

export type FollowMode = 'following' | 'detached';

export interface ConversationScrollOptions {
  sessionId: string;
  /** The transcript has loaded at least once, so the initial snap may happen. */
  hasLoaded: boolean;
  /** Changes whenever the rendered transcript may have grown. */
  contentVersion: string;
  /** Number of earlier (stored) rows currently rendered. */
  olderCount: number;
  /** Newest transcript sequence, for the read receipt. */
  latestSeq: number;
  /** Key of the assistant reply still being written, or `null`. */
  streaming: string | null;
  /** A read receipt is allowed (`capabilities.open` and not transitioning). */
  canMarkRead: boolean;
  enabled?: boolean;
}

export interface ConversationScroll {
  scrollerRef: React.RefObject<HTMLDivElement | null>;
  /** Attached to the inner content wrapper so delayed layout growth (such as throttled streaming Markdown commits) triggers follow checks. */
  contentRef: React.RefObject<HTMLDivElement | null>;
  /** Attached to the streaming assistant row, for the viewport-height pause. */
  streamingRef: React.RefObject<HTMLDivElement | null>;
  mode: FollowMode;
  /** Auto-follow stopped because the streaming reply is taller than the viewport. */
  paused: boolean;
  /** Whether a reply is currently streaming. */
  streaming: boolean;
  /** Content arrived while the reader was away from the end. */
  hasNew: boolean;
  showJumpPill: boolean;
  releaseFollow: () => void;
  follow: () => void;
  onScroll: () => void;
}

interface ScrollState {
  mode: FollowMode;
  paused: boolean;
  hasNew: boolean;
}

const INITIAL_STATE: ScrollState = { mode: 'following', paused: false, hasNew: false };

/** A read receipt means "the person saw this": the page must be visible and focused. */
function isPageActive(): boolean {
  return document.visibilityState === 'visible' && document.hasFocus();
}

function distanceFromEnd(element: HTMLElement): number {
  return element.scrollHeight - element.scrollTop - element.clientHeight;
}

export function useConversationScroll(options: ConversationScrollOptions): ConversationScroll {
  const {
    sessionId,
    hasLoaded,
    contentVersion,
    olderCount,
    latestSeq,
    streaming,
    canMarkRead,
    enabled = true,
  } = options;

  const scrollerRef = useRef<HTMLDivElement | null>(null);
  const contentRef = useRef<HTMLDivElement | null>(null);
  const streamingRef = useRef<HTMLDivElement | null>(null);
  const [state, setState] = useState<ScrollState>(INITIAL_STATE);
  const stateRef = useRef(state);
  stateRef.current = state;

  const update = useCallback((change: Partial<ScrollState>) => {
    setState(current => {
      const next = { ...current, ...change };
      if (next.mode === current.mode && next.paused === current.paused && next.hasNew === current.hasNew) {
        return current;
      }
      return next;
    });
  }, []);

  const [pageActive, setPageActive] = useState(isPageActive);
  useEffect(() => {
    const sync = () => setPageActive(isPageActive());
    sync();
    document.addEventListener('visibilitychange', sync);
    window.addEventListener('focus', sync);
    window.addEventListener('blur', sync);
    return () => {
      document.removeEventListener('visibilitychange', sync);
      window.removeEventListener('focus', sync);
      window.removeEventListener('blur', sync);
    };
  }, []);

  const frame = useRef(0);
  const snappedSession = useRef<string | null>(null);
  const previousContent = useRef(contentVersion);
  const previousOlder = useRef(olderCount);
  const previousHeight = useRef(0);
  const previousTop = useRef(0);
  const lastObservedContentHeight = useRef(0);
  const lastMarkedSeq = useRef(0);
  const streamingKeyRef = useRef(streaming);
  streamingKeyRef.current = streaming;
  /**
   * The reply for which the reader explicitly asked to follow the end again.
   * Choosing the pill must win over the tall-reply pause for that same reply,
   * or the pill could never be dismissed while text keeps arriving.
   */
  const tallPauseDismissedFor = useRef<string | null>(null);

  const scrollToEnd = useCallback(
    (skipTallPause: boolean) => {
      const element = scrollerRef.current;
      if (!element) return;
      element.scrollTop = Math.max(0, element.scrollHeight - element.clientHeight);
      if (skipTallPause) return;
      // A streaming reply taller than the viewport leaves its own top above the
      // scroller's top edge once the end is in view. Stop at the reply's top
      // edge and freeze auto-follow so the reply stays readable from line 1.
      const reply = streamingRef.current;
      if (!reply || typeof reply.getBoundingClientRect !== 'function') return;
      const replyTop = reply.getBoundingClientRect().top;
      const viewportTop = element.getBoundingClientRect().top;
      const overshoot = viewportTop - replyTop;
      if (overshoot >= 1) {
        element.scrollTop = Math.max(0, element.scrollTop - overshoot);
        update({ paused: true, hasNew: false });
      }
    },
    [update],
  );

  /** At most one scroll-to-end per animation frame. */
  const scheduleFollow = useCallback(() => {
    if (frame.current) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = 0;
      const current = stateRef.current;
      if (current.mode !== 'following' || current.paused) return;
      const key = streamingKeyRef.current;
      scrollToEnd(Boolean(key) && tallPauseDismissedFor.current === key);
    });
  }, [scrollToEnd]);

  const cancelScheduledFollow = useCallback(() => {
    if (!frame.current) return;
    cancelAnimationFrame(frame.current);
    frame.current = 0;
  }, []);

  /** Snap to the end and re-attach to it. */
  const follow = useCallback(() => {
    cancelScheduledFollow();
    tallPauseDismissedFor.current = streamingKeyRef.current;
    update({ mode: 'following', paused: false, hasNew: false });
    scrollToEnd(true);
  }, [cancelScheduledFollow, scrollToEnd, update]);

  /**
   * Detach immediately. Callers run this *before* a tapped disclosure expands,
   * so the growing card cannot pull the row the reader tapped off screen.
   */
  const releaseFollow = useCallback(() => {
    cancelScheduledFollow();
    update({ mode: 'detached', paused: false });
  }, [cancelScheduledFollow, update]);

  const onScroll = useCallback(() => {
    const element = scrollerRef.current;
    if (!element) return;
    // Keep the prepend anchor on the live position: a reader who scrolls while
    // an earlier-history request is in flight is restored from where they are now.
    previousTop.current = element.scrollTop;
    previousHeight.current = element.scrollHeight;
    const distance = distanceFromEnd(element);
    const current = stateRef.current;
    if (distance > BOTTOM_BAND_PX) {
      if (current.paused) return;
      if (current.mode !== 'detached') update({ mode: 'detached', paused: false });
      return;
    }
    // Returning to the bottom band re-attaches a detached reader. A paused
    // follow stays paused until the pill is tapped: the engine's own scroll to
    // the end also lands in the band, so clearing the latch here would undo it.
    if (current.mode === 'detached') update({ mode: 'following', paused: false, hasNew: false });
  }, [update]);

  // A different reply means the old explicit choice no longer applies.
  useEffect(() => {
    if (tallPauseDismissedFor.current !== streaming) tallPauseDismissedFor.current = null;
  }, [streaming]);

  // A new conversation starts following, with a clean read-receipt frontier.
  useEffect(() => {
    snappedSession.current = null;
    previousContent.current = contentVersion;
    lastMarkedSeq.current = 0;
    tallPauseDismissedFor.current = null;
    update({ ...INITIAL_STATE, hasNew: false });
    return cancelScheduledFollow;
    // `contentVersion` is only read to prime the growth comparison on switch.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  // Snap to the end exactly once, when the initial transcript arrives.
  useEffect(() => {
    if (!enabled || !hasLoaded || snappedSession.current === sessionId) return;
    snappedSession.current = sessionId;
    previousContent.current = contentVersion;
    scheduleFollow();
  }, [enabled, hasLoaded, sessionId, contentVersion, scheduleFollow]);

  // Growing content follows the end only while following and not paused.
  useEffect(() => {
    const grew = previousContent.current !== contentVersion;
    previousContent.current = contentVersion;
    if (!enabled || !hasLoaded || !grew) return;
    const current = stateRef.current;
    if (current.mode === 'following') {
      if (current.paused) {
        if (!current.hasNew) update({ hasNew: true });
      } else {
        scheduleFollow();
      }
    } else if (!current.hasNew) {
      update({ hasNew: true });
    }
  }, [enabled, hasLoaded, contentVersion, scheduleFollow, update]);

  // Observe actual DOM content growth (e.g. throttled streaming Markdown
  // trailing commits or late media layout) so end-follow and tall-reply pause
  // run after the text has actually painted, never only before the throttle.
  useEffect(() => {
    if (!enabled || !hasLoaded || typeof ResizeObserver === 'undefined') return;
    const content = contentRef.current;
    if (!content) return;
    lastObservedContentHeight.current = content.getBoundingClientRect().height;
    const observer = new ResizeObserver(entries => {
      const entry = entries[entries.length - 1];
      const nextHeight = entry ? entry.contentRect.height : content.getBoundingClientRect().height;
      const grew = nextHeight > lastObservedContentHeight.current + 0.5;
      lastObservedContentHeight.current = nextHeight;
      if (!grew || snappedSession.current !== sessionId) return;
      const current = stateRef.current;
      if (current.mode !== 'following') return;
      if (current.paused) {
        if (!current.hasNew) update({ hasNew: true });
      } else {
        scheduleFollow();
      }
    });
    observer.observe(content);
    return () => observer.disconnect();
  }, [enabled, hasLoaded, sessionId, scheduleFollow, update]);

  // Prepend anchoring: `olderCount` grew, so the content above the viewport
  // grew too. The layout effect runs synchronously after the DOM commit and
  // before paint, so restoring the delta cannot produce a visible jump.
  useLayoutEffect(() => {
    const element = scrollerRef.current;
    // The first load of a session is snapped to the end, not anchored.
    const prepended = olderCount > previousOlder.current && snappedSession.current === sessionId;
    if (element) {
      if (prepended) {
        element.scrollTop = previousTop.current + (element.scrollHeight - previousHeight.current);
      }
      previousHeight.current = element.scrollHeight;
      previousTop.current = element.scrollTop;
    }
    previousOlder.current = olderCount;
  });

  // A read receipt is a courtesy, not a correctness path: debounce it while
  // following, and never send one for a session that cannot take one or while
  // the page is hidden or unfocused. Returning to the page re-runs this effect.
  useEffect(() => {
    if (!enabled || !canMarkRead || !hasLoaded || !pageActive) return;
    if (state.mode !== 'following' || state.paused) return;
    if (latestSeq <= lastMarkedSeq.current) return;
    const seq = latestSeq;
    const timer = setTimeout(() => {
      if (!isPageActive()) return;
      lastMarkedSeq.current = seq;
      void markConversationRead(sessionId, seq).catch(() => {
        // The next stream advance retries; a receipt never blocks reading.
      });
    }, READ_RECEIPT_DELAY_MS);
    return () => clearTimeout(timer);
  }, [enabled, canMarkRead, hasLoaded, state.mode, state.paused, pageActive, latestSeq, sessionId]);

  return {
    scrollerRef,
    contentRef,
    streamingRef,
    mode: state.mode,
    paused: state.paused,
    streaming: streaming !== null,
    hasNew: state.hasNew,
    showJumpPill: state.mode === 'detached' || state.paused,
    releaseFollow,
    follow,
    onScroll,
  };
}
