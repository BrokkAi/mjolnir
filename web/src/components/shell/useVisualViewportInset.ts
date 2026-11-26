import { useEffect } from 'react';

/**
 * Publish the on-screen keyboard height as `--mj-keyboard-inset` on <html>.
 *
 * Browsers that resize the layout viewport for the keyboard (Chrome with
 * `interactive-widget=resizes-content`) already shrink `100dvh`; the inset is
 * then zero. Browsers that only shrink the visual viewport (iOS Safari) report
 * the difference here, and bottom-docked UI pads by it with `pb-keyboard`.
 * Nothing resizes `body`, so the page does not reflow when the keyboard moves.
 */
export function useVisualViewportInset(): void {
  useEffect(() => {
    const viewport = window.visualViewport;
    if (!viewport) return;
    let frame = 0;
    const root = document.documentElement;
    const update = () => {
      frame = 0;
      const covered = window.innerHeight - viewport.height - viewport.offsetTop;
      root.style.setProperty('--mj-keyboard-inset', `${Math.max(0, Math.round(covered))}px`);
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(update);
    };
    update();
    viewport.addEventListener('resize', schedule);
    viewport.addEventListener('scroll', schedule);
    return () => {
      viewport.removeEventListener('resize', schedule);
      viewport.removeEventListener('scroll', schedule);
      if (frame) cancelAnimationFrame(frame);
      root.style.removeProperty('--mj-keyboard-inset');
    };
  }, []);
}
