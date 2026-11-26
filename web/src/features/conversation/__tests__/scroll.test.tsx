import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  BOTTOM_BAND_PX,
  useConversationScroll,
  type ConversationScroll,
  type ConversationScrollOptions,
} from '../useConversationScroll';

/**
 * The scroll engine is deliberately measurable without a real browser: a fixed
 * fake scroller plus a hand-run animation-frame queue drives every transition.
 */

let frameCallbacks = new Map<number, FrameRequestCallback>();
let nextFrameId = 0;

function runFrames() {
  const pending = [...frameCallbacks.values()];
  frameCallbacks.clear();
  act(() => {
    for (const callback of pending) callback(0);
  });
}

interface HarnessProps extends Partial<ConversationScrollOptions> {
  apiRef: { current: ConversationScroll | null };
}

function Harness({ apiRef, ...options }: HarnessProps) {
  const scroll = useConversationScroll({
    sessionId: 's1',
    hasLoaded: true,
    contentVersion: 'v1',
    olderCount: 0,
    latestSeq: 0,
    streaming: null,
    canMarkRead: false,
    ...options,
  });
  apiRef.current = scroll;
  return (
    <div data-testid="conversation-scroll" ref={scroll.scrollerRef} onScroll={scroll.onScroll}>
      <div data-testid="conversation-content" ref={scroll.contentRef}>
        <div data-testid="streaming-row" ref={scroll.streamingRef} />
      </div>
    </div>
  );
}

function installGeometry(element: HTMLElement, geometry: { scrollHeight: number; clientHeight: number }) {
  Object.defineProperty(element, 'scrollHeight', {
    configurable: true,
    get: () => geometry.scrollHeight,
  });
  Object.defineProperty(element, 'clientHeight', {
    configurable: true,
    get: () => geometry.clientHeight,
  });
}

function stubRect(element: HTMLElement, top: number) {
  element.getBoundingClientRect = () =>
    ({
      top,
      bottom: top + 100,
      left: 0,
      right: 100,
      width: 100,
      height: 100,
      x: 0,
      y: top,
      toJSON: () => ({}),
    }) as DOMRect;
}

function scrollTo(element: HTMLElement, top: number) {
  element.scrollTop = top;
  fireEvent.scroll(element);
}

beforeEach(() => {
  frameCallbacks = new Map();
  nextFrameId = 0;
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
    nextFrameId += 1;
    frameCallbacks.set(nextFrameId, callback);
    return nextFrameId;
  });
  vi.stubGlobal('cancelAnimationFrame', (handle: number) => {
    frameCallbacks.delete(handle);
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe('useConversationScroll', () => {
  it('starts following and snaps to the end once the transcript has loaded', () => {
    const apiRef: { current: ConversationScroll | null } = { current: null };
    const { rerender } = render(<Harness apiRef={apiRef} hasLoaded={false} />);
    const scroller = screen.getByTestId('conversation-scroll');
    installGeometry(scroller, { scrollHeight: 1000, clientHeight: 500 });
    scroller.scrollTop = 0;

    rerender(<Harness apiRef={apiRef} hasLoaded />);
    runFrames();

    expect(apiRef.current?.mode).toBe('following');
    expect(apiRef.current?.paused).toBe(false);
    expect(apiRef.current?.showJumpPill).toBe(false);
    expect(scroller.scrollTop).toBe(500);
  });

  it('detaches outside the 32px bottom band and re-attaches inside it', () => {
    const apiRef: { current: ConversationScroll | null } = { current: null };
    render(<Harness apiRef={apiRef} />);
    const scroller = screen.getByTestId('conversation-scroll');
    installGeometry(scroller, { scrollHeight: 1000, clientHeight: 500 });
    scroller.scrollTop = 500;
    runFrames();
    expect(apiRef.current?.mode).toBe('following');

    act(() => scrollTo(scroller, 500 - BOTTOM_BAND_PX - 1));
    expect(apiRef.current?.mode).toBe('detached');
    expect(apiRef.current?.showJumpPill).toBe(true);

    act(() => scrollTo(scroller, 500 - BOTTOM_BAND_PX + 1));
    expect(apiRef.current?.mode).toBe('following');
    expect(apiRef.current?.showJumpPill).toBe(false);
  });

  it('releases follow immediately and never chases growth while detached', () => {
    const apiRef: { current: ConversationScroll | null } = { current: null };
    const geometry = { scrollHeight: 1000, clientHeight: 500 };
    const { rerender } = render(<Harness apiRef={apiRef} contentVersion="v1" />);
    const scroller = screen.getByTestId('conversation-scroll');
    installGeometry(scroller, geometry);
    scroller.scrollTop = 0;
    runFrames();
    expect(scroller.scrollTop).toBe(500);

    // A tap on a tool card header calls this before the card expands.
    act(() => apiRef.current?.releaseFollow());
    expect(apiRef.current?.mode).toBe('detached');

    geometry.scrollHeight = 1400;
    rerender(<Harness apiRef={apiRef} contentVersion="v2" />);
    runFrames();

    expect(scroller.scrollTop).toBe(500);
    expect(apiRef.current?.mode).toBe('detached');
    expect(apiRef.current?.hasNew).toBe(true);
    expect(apiRef.current?.showJumpPill).toBe(true);
  });

  it('pauses follow for a streaming reply taller than the viewport, and the pill re-attaches it', () => {
    const apiRef: { current: ConversationScroll | null } = { current: null };
    const geometry = { scrollHeight: 1000, clientHeight: 500 };
    const { rerender } = render(<Harness apiRef={apiRef} streaming="row-1" contentVersion="v1" />);
    const scroller = screen.getByTestId('conversation-scroll');
    const row = screen.getByTestId('streaming-row');
    installGeometry(scroller, geometry);
    stubRect(scroller, 0);
    stubRect(row, -120);

    rerender(<Harness apiRef={apiRef} streaming="row-1" contentVersion="v2" />);
    runFrames();

    expect(apiRef.current?.mode).toBe('following');
    expect(apiRef.current?.paused).toBe(true);
    expect(apiRef.current?.showJumpPill).toBe(true);

    // While paused, growth must not pull the reader down.
    scroller.scrollTop = 200;
    geometry.scrollHeight = 1200;
    rerender(<Harness apiRef={apiRef} streaming="row-1" contentVersion="v3" />);
    runFrames();
    expect(scroller.scrollTop).toBe(200);

    // Tapping the pill is an explicit choice to follow this reply at the end.
    act(() => apiRef.current?.follow());
    expect(apiRef.current?.paused).toBe(false);
    expect(scroller.scrollTop).toBe(700);

    // The same reply keeps streaming: follow continues now that the reader asked.
    geometry.scrollHeight = 1400;
    rerender(<Harness apiRef={apiRef} streaming="row-1" contentVersion="v4" />);
    runFrames();
    expect(scroller.scrollTop).toBe(900);
    expect(apiRef.current?.paused).toBe(false);

    // A new reply is new information and gets the tall-reply pause again.
    rerender(<Harness apiRef={apiRef} streaming="row-2" contentVersion="v5" />);
    runFrames();
    expect(apiRef.current?.paused).toBe(true);
  });

  it('compensates the viewport synchronously when earlier history is prepended', () => {
    const apiRef: { current: ConversationScroll | null } = { current: null };
    const geometry = { scrollHeight: 1000, clientHeight: 500 };
    const { rerender } = render(<Harness apiRef={apiRef} olderCount={0} contentVersion="v1" />);
    const scroller = screen.getByTestId('conversation-scroll');
    installGeometry(scroller, geometry);
    scroller.scrollTop = 0;
    fireEvent.scroll(scroller);
    runFrames();
    expect(apiRef.current?.mode).toBe('detached');

    geometry.scrollHeight = 1400;
    rerender(<Harness apiRef={apiRef} olderCount={1} contentVersion="v1" />);

    expect(scroller.scrollTop).toBe(400);
    expect(apiRef.current?.mode).toBe('detached');
  });

  it('re-checks end-follow and tall-reply pause when throttled DOM growth arrives after the rAF', () => {
    let resizeCallback: ResizeObserverCallback | null = null;
    class FakeResizeObserver {
      constructor(cb: ResizeObserverCallback) {
        resizeCallback = cb;
      }
      observe() {}
      disconnect() {}
      unobserve() {}
    }
    vi.stubGlobal('ResizeObserver', FakeResizeObserver);

    const apiRef: { current: ConversationScroll | null } = { current: null };
    const geometry = { scrollHeight: 800, clientHeight: 500 };
    const { rerender } = render(<Harness apiRef={apiRef} streaming="row-1" contentVersion="v1" />);
    const scroller = screen.getByTestId('conversation-scroll');
    const row = screen.getByTestId('streaming-row');
    installGeometry(scroller, geometry);
    stubRect(scroller, 0);
    // Initial short streaming reply: top is inside viewport, so follow stays unpaused.
    stubRect(row, 200);
    runFrames();
    expect(apiRef.current?.paused).toBe(false);

    // Content version bumps before the 100ms Markdown throttle commits; rAF runs on the short DOM.
    rerender(<Harness apiRef={apiRef} streaming="row-1" contentVersion="v2" />);
    runFrames();
    expect(apiRef.current?.paused).toBe(false);

    // 100ms later, throttled Markdown renders tall content and ResizeObserver fires.
    geometry.scrollHeight = 2000;
    stubRect(row, -600);
    act(() => {
      resizeCallback?.(
        [{ contentRect: { height: 2000 } } as ResizeObserverEntry],
        {} as ResizeObserver,
      );
    });
    runFrames();

    expect(apiRef.current?.mode).toBe('following');
    expect(apiRef.current?.paused).toBe(true);
    expect(apiRef.current?.showJumpPill).toBe(true);
    expect(scroller.scrollTop).toBe(900);
  });
});
