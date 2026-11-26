import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiError, apiFetch, configureApi, dispatchAction } from '../client';
import { MAX_BACKOFF_MS, MIN_BACKOFF_MS, ViewerEventStream, type StreamStatus } from '../events';
import { UnknownInternedReferenceError, type RuntimeCursor, type ViewerFrame } from '../types';
import { FakeEventSource, cursor, jsonResponse, snapshot } from '../../test-utils';

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
  configureApi({ onUnauthorized: () => {}, onUpgrade: () => {} });
});

describe('apiFetch', () => {
  it('sends credentials, turns 401 into the auth callback, and extracts JSON errors', async () => {
    const unauthorized = vi.fn();
    configureApi({ onUnauthorized: unauthorized });
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(new Response('', { status: 401 }))
      .mockResolvedValueOnce(jsonResponse({ error: 'session busy', code: 'busy' }, { status: 409 }));
    vi.stubGlobal('fetch', fetchMock);

    await expect(apiFetch('/api/v1/snapshot')).rejects.toMatchObject({ status: 401 });
    expect(unauthorized).toHaveBeenCalledOnce();
    expect(fetchMock.mock.calls[0][1]).toMatchObject({ credentials: 'include' });

    const failure = await apiFetch('/api/v1/actions', { method: 'POST', json: {} }).catch(error => error);
    expect(failure).toBeInstanceOf(ApiError);
    expect(failure).toMatchObject({ message: 'session busy', status: 409, code: 'busy' });
  });

  it('waits out an upgrade 503 and sends the identical request body again', async () => {
    const upgrade = vi.fn();
    configureApi({ onUpgrade: upgrade });
    const pending = () => new Response('', { status: 503, headers: { 'x-mj-upgrade': 'pending' } });
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url === '/') return new Response('<html>', { status: 200 });
      if (fetchMock.mock.calls.filter(([u]) => u === '/api/v1/actions').length === 1) return pending();
      void init;
      return new Response(null, { status: 202 });
    });
    vi.stubGlobal('fetch', fetchMock);

    const action = { action: 'prompt', command_id: 'c1', session_id: 's1', text: 'hi' } as const;
    const result = await dispatchAction(action);

    expect(result).toEqual({ accepted: true });
    const posts = fetchMock.mock.calls.filter(([url]) => url === '/api/v1/actions');
    expect(posts).toHaveLength(2);
    expect(posts[1][1]?.body).toBe(posts[0][1]?.body);
    expect(JSON.parse(posts[1][1]?.body as string)).toMatchObject({ command_id: 'c1' });
    expect(upgrade.mock.calls).toEqual([[true], [false]]);
  });

  it('answers an empty 202 and returns the set-container-settings preview body', async () => {
    const preview = { session_id: 's1', available: true };
    vi.stubGlobal(
      'fetch',
      vi.fn(async (_url: string, init?: RequestInit) =>
        JSON.parse(init?.body as string).action === 'set-container-settings'
          ? jsonResponse(preview)
          : new Response(null, { status: 202 }),
      ),
    );
    expect(await dispatchAction({ action: 'cancel', session_id: 's1' })).toEqual({ accepted: true });
    expect(await dispatchAction({ action: 'set-container-settings', session_id: 's1' })).toEqual({
      accepted: true,
      preview,
    });
  });
});

describe('ViewerEventStream', () => {
  let applied: RuntimeCursor | null;
  let statuses: StreamStatus[];
  let frames: ViewerFrame[];
  let visible: boolean;
  let stream: ViewerEventStream;

  beforeEach(() => {
    vi.useFakeTimers();
    FakeEventSource.reset();
    applied = null;
    statuses = [];
    frames = [];
    visible = true;
    stream = new ViewerEventStream(
      {
        getCursor: () => applied,
        onStatus: status => statuses.push(status),
        onFrame: frame => {
          frames.push(frame);
          if (frame.kind === 'snapshot') applied = frame.cursor;
          if (frame.kind === 'delta') applied = frame.cursor;
        },
      },
      {
        EventSource: FakeEventSource as unknown as typeof EventSource,
        isVisible: () => visible,
        isOnline: () => true,
      },
    );
  });

  afterEach(() => stream.stop());

  it('parses snapshot, delta and reset_required frames, including conversation_versions', () => {
    stream.start();
    const source = FakeEventSource.last;
    expect(source.url).toBe('/api/v1/viewer/events?format=changes');
    source.emit('open');
    const delta: ViewerFrame = {
      kind: 'delta',
      from: cursor(1),
      cursor: cursor(2),
      metadata: { revision: 2, server_time_ms: 2, generated_at: 'x' },
      sessions: [],
      detail_changed: [],
      interned: {},
      conversation_versions: { s1: { latest_seq: 7, presentation_key: 'k' } },
    };
    source.emit('runtime', { kind: 'snapshot', cursor: cursor(1), snapshot: snapshot([]) });
    source.emit('runtime', delta);
    source.emit('runtime', { kind: 'reset_required' });
    expect(frames.map(frame => frame.kind)).toEqual(['snapshot', 'delta', 'reset_required']);
    expect((frames[1] as Extract<ViewerFrame, { kind: 'delta' }>).conversation_versions).toEqual({
      s1: { latest_seq: 7, presentation_key: 'k' },
    });
    expect(statuses).toContain('live');
  });

  it('reconnects with exponential backoff from the applied cursor and resets after a good frame', () => {
    stream.start();
    const first = FakeEventSource.last;
    first.emit('runtime', { kind: 'snapshot', cursor: cursor(5), snapshot: snapshot([], { sequence: 5 }) });
    // Two failures before any new frame: 250 ms, then 500 ms.
    first.emit('error');
    expect(first.closed).toBe(true);
    vi.advanceTimersByTime(MIN_BACKOFF_MS - 1);
    expect(FakeEventSource.instances).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(FakeEventSource.instances).toHaveLength(2);
    expect(FakeEventSource.last.url).toContain('since=inc%3A5');

    FakeEventSource.last.emit('error');
    vi.advanceTimersByTime(MIN_BACKOFF_MS * 2 - 1);
    expect(FakeEventSource.instances).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(FakeEventSource.instances).toHaveLength(3);

    // A frame resets the delay.
    FakeEventSource.last.emit('runtime', { kind: 'reset_required' });
    FakeEventSource.last.emit('error');
    vi.advanceTimersByTime(MIN_BACKOFF_MS);
    expect(FakeEventSource.instances).toHaveLength(4);
  });

  it('caps the backoff at 30 s', () => {
    stream.start();
    for (let i = 0; i < 12; i += 1) {
      FakeEventSource.last.emit('error');
      vi.advanceTimersByTime(MAX_BACKOFF_MS);
    }
    const count = FakeEventSource.instances.length;
    FakeEventSource.last.emit('error');
    vi.advanceTimersByTime(MAX_BACKOFF_MS - 1);
    expect(FakeEventSource.instances).toHaveLength(count);
    vi.advanceTimersByTime(1);
    expect(FakeEventSource.instances).toHaveLength(count + 1);
  });

  it('holds reconnects while the tab is hidden and resumes at once when it is visible', () => {
    stream.start();
    visible = false;
    FakeEventSource.last.emit('error');
    vi.advanceTimersByTime(MAX_BACKOFF_MS * 2);
    expect(FakeEventSource.instances).toHaveLength(1);
    expect(statuses.at(-1)).toBe('paused');

    visible = true;
    document.dispatchEvent(new Event('visibilitychange'));
    expect(FakeEventSource.instances).toHaveLength(2);
  });

  it('cancels a pending backoff when the tab hides, then connects immediately on return', () => {
    stream.start();
    FakeEventSource.last.emit('error'); // schedules a 250 ms timer
    visible = false;
    document.dispatchEvent(new Event('visibilitychange'));
    vi.advanceTimersByTime(MAX_BACKOFF_MS);
    expect(FakeEventSource.instances).toHaveLength(1);
    visible = true;
    document.dispatchEvent(new Event('visibilitychange'));
    expect(FakeEventSource.instances).toHaveLength(2);
  });

  it('reconnects from the applied cursor after offline even if the open stream never errors', () => {
    stream.start();
    const first = FakeEventSource.last;
    first.emit('runtime', { kind: 'snapshot', cursor: cursor(5), snapshot: snapshot([], { sequence: 5 }) });
    window.dispatchEvent(new Event('offline'));
    expect(first.closed).toBe(true);
    expect(statuses.at(-1)).toBe('offline');
    window.dispatchEvent(new Event('online'));
    expect(FakeEventSource.instances).toHaveLength(2);
    expect(FakeEventSource.last.url).toContain('since=inc%3A5');
  });

  it('asks for a fresh snapshot when a frame throws an unknown-reference error', () => {
    let throwNext = true;
    const local = new ViewerEventStream(
      {
        getCursor: () => cursor(9),
        onStatus: () => {},
        onFrame: () => {
          if (throwNext) {
            throwNext = false;
            throw new UnknownInternedReferenceError('capabilities');
          }
        },
      },
      { EventSource: FakeEventSource as unknown as typeof EventSource, isVisible: () => true, isOnline: () => true },
    );
    vi.spyOn(console, 'error').mockImplementation(() => {});
    local.start();
    expect(FakeEventSource.last.url).toContain('since=inc%3A9');
    FakeEventSource.last.emit('runtime', { kind: 'reset_required' });
    vi.advanceTimersByTime(MIN_BACKOFF_MS);
    expect(FakeEventSource.last.url).toBe('/api/v1/viewer/events?format=changes');
    local.stop();
  });
});
