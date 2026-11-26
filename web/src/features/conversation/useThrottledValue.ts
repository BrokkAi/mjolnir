import { useEffect, useRef, useState } from 'react';

/**
 * Trailing-edge throttle for values that are expensive to render.
 *
 * Streaming text arrives one packet at a time; re-parsing Markdown for every
 * packet is what makes a phone conversation scroll jitter. The leading update
 * is immediate, updates inside the interval collapse into one trailing update
 * with the newest value, and the final value is always applied, so text is
 * never dropped or shortened.
 */
export function useThrottledValue<T>(value: T, intervalMs: number): T {
  const [throttled, setThrottled] = useState(value);
  const lastRun = useRef(0);

  useEffect(() => {
    if (intervalMs <= 0) {
      setThrottled(value);
      return;
    }
    const now = Date.now();
    const elapsed = now - lastRun.current;
    if (elapsed >= intervalMs) {
      lastRun.current = now;
      setThrottled(value);
      return;
    }
    const timer = setTimeout(() => {
      lastRun.current = Date.now();
      setThrottled(value);
    }, intervalMs - elapsed);
    return () => clearTimeout(timer);
  }, [value, intervalMs]);

  return throttled;
}
