import { useEffect, useRef } from 'react';

interface EdgeSwipeOptions {
  enabled: boolean;
  open: boolean;
  onOpen: () => void;
  onClose: () => void;
}

/** Width of the left strip where an opening swipe may start. */
const EDGE_PX = 24;
/** Horizontal travel that commits the gesture. */
const COMMIT_PX = 56;

/**
 * Open the drawer with a rightward swipe from the left edge, and close it with
 * a leftward swipe. Passive listeners only: vertical scrolling is never
 * blocked, and a gesture whose first movement is mostly vertical is ignored.
 */
export function useEdgeSwipe({ enabled, open, onOpen, onClose }: EdgeSwipeOptions): void {
  const latest = useRef({ open, onOpen, onClose });
  latest.current = { open, onOpen, onClose };

  useEffect(() => {
    if (!enabled) return;
    let start: { x: number; y: number; mode: 'open' | 'close' } | null = null;
    let decided = false;

    const onStart = (event: TouchEvent) => {
      const touch = event.touches[0];
      if (!touch || event.touches.length !== 1) return;
      start = null;
      decided = false;
      if (latest.current.open) start = { x: touch.clientX, y: touch.clientY, mode: 'close' };
      else if (touch.clientX <= EDGE_PX) start = { x: touch.clientX, y: touch.clientY, mode: 'open' };
    };

    const onMove = (event: TouchEvent) => {
      const touch = event.touches[0];
      if (!start || !touch) return;
      const dx = touch.clientX - start.x;
      const dy = touch.clientY - start.y;
      if (!decided && Math.max(Math.abs(dx), Math.abs(dy)) > 8) {
        decided = true;
        if (Math.abs(dy) > Math.abs(dx)) {
          start = null;
          return;
        }
      }
      if (start.mode === 'open' && dx >= COMMIT_PX) {
        start = null;
        latest.current.onOpen();
      } else if (start.mode === 'close' && dx <= -COMMIT_PX) {
        start = null;
        latest.current.onClose();
      }
    };

    const onEnd = () => {
      start = null;
    };

    document.addEventListener('touchstart', onStart, { passive: true });
    document.addEventListener('touchmove', onMove, { passive: true });
    document.addEventListener('touchend', onEnd, { passive: true });
    document.addEventListener('touchcancel', onEnd, { passive: true });
    return () => {
      document.removeEventListener('touchstart', onStart);
      document.removeEventListener('touchmove', onMove);
      document.removeEventListener('touchend', onEnd);
      document.removeEventListener('touchcancel', onEnd);
    };
  }, [enabled]);
}
