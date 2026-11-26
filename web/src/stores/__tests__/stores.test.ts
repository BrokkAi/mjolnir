import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ViewerFrame, ViewerSession, WireSessionRow } from '../../api/types';
import {
  FakeEventSource,
  cursor,
  entry,
  flush,
  jsonResponse,
  liveRow,
  mockFetch,
  snapshot,
  summaryRow,
  transcript,
} from '../../test-utils';
import { attentionSessions, effectiveRoute } from '../selectors';
import { useSessionStore } from '../useSessionStore';
import {
  TRANSCRIPT_CACHE_CAPACITY,
  applyTranscriptResponse,
  emptyTranscript,
  startTranscriptSync,
  useTranscriptStore,
  visibleOlderItems,
} from '../useTranscriptStore';
import { installUiListeners, isImplicitRoute, parseRoute, routeHash, useUiStore, type Route } from '../useUiStore';

const v = (latest_seq: number, presentation_key = 'k') => ({ latest_seq, presentation_key });
const sessions = () => useSessionStore.getState();
const transcripts = () => useTranscriptStore.getState();

function delta(
  from: number,
  to: number,
  extra: Partial<Extract<ViewerFrame, { kind: 'delta' }>> = {},
): Extract<ViewerFrame, { kind: 'delta' }> {
  return {
    kind: 'delta',
    from: cursor(from),
    cursor: cursor(to),
    metadata: { revision: to, server_time_ms: to, generated_at: `t${to}` },
    sessions: [],
    detail_changed: [],
    interned: {},
    ...extra,
  };
}

function resetStores() {
  sessions().handleSignedOut();
  useUiStore.setState({ layers: [], toasts: [], drawerOpen: false });
  location.hash = '';
}

beforeEach(() => {
  FakeEventSource.reset();
  vi.stubGlobal('EventSource', FakeEventSource);
  resetStores();
});

afterEach(() => {
  resetStores();
  vi.unstubAllGlobals();
});

describe('session store: snapshot and stream frames', () => {
  it('installs a snapshot, expanding interned fields, and merges deltas without touching other rows', () => {
    const { capabilities, ...rest } = liveRow('s1');
    const wire = { ...rest, capabilities_ref: '1111111111111111' } as unknown as WireSessionRow;
    sessions().applySnapshot(
      snapshot([wire, liveRow('s2')], { interned: { '1111111111111111': capabilities as never }, sequence: 1 }),
    );
    expect(sessions().sessionsById.s1.capabilities).toEqual(capabilities);
    const s2 = sessions().sessionsById.s2;

    // New row via a ref that arrives in the same delta, a removed row, and changed metadata.
    const added = { ...liveRow('s3'), capabilities_ref: '2222222222222222' } as unknown as WireSessionRow;
    delete (added as Partial<ViewerSession>).capabilities;
    expect(
      sessions().applyFrame(
        delta(1, 2, {
          sessions: [
            ['s3', added],
            ['s1', null],
          ],
          interned: { '2222222222222222': { ...capabilities, prompt: false } as never },
          metadata: { revision: 2, server_time_ms: 5, generated_at: 't2', operator_away: true },
        }),
      ),
    ).toBe(true);

    const state = sessions();
    expect(Object.keys(state.sessionsById).sort()).toEqual(['s2', 's3']);
    expect(state.sessionsById.s2).toBe(s2);
    expect(state.sessionsById.s3.capabilities.prompt).toBe(false);
    expect(state.meta?.operator_away).toBe(true);
    expect(state.meta?.profiles).toEqual([]);
    expect(state.cursor).toEqual(cursor(2));
  });

  it('ignores a repeated cursor, refuses a gap, and leaves state untouched on an unknown reference', () => {
    sessions().applySnapshot(snapshot([liveRow('s1')], { sequence: 1 }));
    expect(sessions().applyFrame(delta(1, 2))).toBe(true);
    expect(sessions().applyFrame(delta(1, 2))).toBe(false);
    expect(() => sessions().applyFrame(delta(5, 6))).toThrow(/gap/);

    const before = sessions().sessionsById;
    const bad = { ...liveRow('s9'), capabilities_ref: '9999999999999999' } as unknown as WireSessionRow;
    expect(() => sessions().applyFrame(delta(2, 3, { sessions: [['s9', bad]] }))).toThrow(/interned/);
    expect(sessions().sessionsById).toBe(before);
    expect(sessions().cursor).toEqual(cursor(2));
  });

  it('replaces everything on a snapshot frame after reset_required, taking its conversation_versions', () => {
    sessions().applySnapshot(snapshot([liveRow('s1')], { sequence: 1 }));
    sessions().applyFrame(delta(1, 2, { conversation_versions: { s1: v(4) } }));
    expect(sessions().conversationVersions).toEqual({ s1: v(4) });

    expect(sessions().applyFrame({ kind: 'reset_required' })).toBe(false);
    expect(sessions().cursor).toBeNull();
    sessions().applyFrame({
      kind: 'snapshot',
      cursor: cursor(1, 'new'),
      snapshot: snapshot([liveRow('s7')], { sequence: 1 }),
      conversation_versions: { s7: v(2) },
    });
    expect(Object.keys(sessions().sessionsById)).toEqual(['s7']);
    expect(sessions().conversationVersions).toEqual({ s7: v(2) });
    expect(sessions().cursor).toEqual(cursor(1, 'new'));
  });

  it('attaches a fetched detail row and reverts to the summary when the stream says detail changed', async () => {
    sessions().applySnapshot(snapshot([summaryRow('s1')], { sequence: 1 }));
    const detail = liveRow('s1', { title: 'Detailed' });
    mockFetch({ '/api/v1/sessions/s1/row': () => jsonResponse({ revision: 1, row: detail }) });

    await sessions().ensureSessionDetail('s1');
    expect(sessions().sessionsById.s1).toEqual(detail);
    expect(sessions().sessionsById.s1.detail).toBeUndefined();

    sessions().applyFrame(delta(1, 2, { detail_changed: ['s1'] }));
    expect(sessions().sessionsById.s1.detail).toBe(false);
  });

  it('drops a detail answer that arrives after the summary changed and reads the row again', async () => {
    sessions().applySnapshot(snapshot([summaryRow('s1')], { sequence: 1 }));
    let release!: () => void;
    let requests = 0;
    mockFetch({
      '/api/v1/sessions/s1/row': () => {
        requests += 1;
        if (requests > 1) return jsonResponse({ revision: 2, row: liveRow('s1', { title: 'Fresh' }) });
        return new Promise<Response>(resolve => {
          release = () => resolve(jsonResponse({ revision: 1, row: liveRow('s1', { title: 'Stale' }) }));
        });
      },
    });
    const pending = sessions().ensureSessionDetail('s1');
    await flush(1);
    sessions().applyFrame(delta(1, 2, { detail_changed: ['s1'] }));
    release();
    await pending;
    expect(requests).toBe(2);
    expect(sessions().sessionsById.s1.title).toBe('Fresh');
  });
});

describe('boot', () => {
  it('fetches the snapshot, and an initial conversation route fetches its transcript in parallel, without /auth/session or /options', async () => {
    location.hash = '#/conversation/s1';
    const net = mockFetch({
      '/api/v1/snapshot': () => jsonResponse(snapshot([liveRow('s1')])),
      '/api/v1/conversations/s1': () => jsonResponse(transcript()),
    });
    sessions().boot();
    await flush();
    expect(net.urls().sort()).toEqual(['/api/v1/conversations/s1', '/api/v1/snapshot']);
    expect(sessions().auth).toBe('signed_in');
    expect(FakeEventSource.last.url).toContain('since=inc%3A1');
    expect(transcripts().entries.s1.latestSeq).toBe(3);
    // The conversation fetch recorded its sequence so a return visit needs no request.
    expect(sessions().conversationVersions.s1).toEqual(v(3));
  });

  it('shows signed_out on a 401 snapshot and clears state', async () => {
    mockFetch({ '/api/v1/snapshot': () => new Response('', { status: 401 }) });
    await sessions().boot();
    expect(sessions().auth).toBe('signed_out');
    expect(FakeEventSource.instances).toHaveLength(0);
  });
});

describe('conversation fetch gating', () => {
  let net: ReturnType<typeof mockFetch>;
  let stop: () => void;

  async function open(id: string) {
    await transcripts().openConversation(id);
  }
  const conversationCalls = () => net.urls().filter(url => url.startsWith('/api/v1/conversations/'));

  beforeEach(() => {
    sessions().applySnapshot(snapshot([liveRow('s1'), liveRow('s2')], { sequence: 1 }));
    useSessionStore.setState({ auth: 'signed_in' });
    net = mockFetch({
      '/api/v1/conversations/s1': () => jsonResponse(transcript()),
      '/api/v1/conversations/s2': () => jsonResponse(transcript({ latest_seq: 3 })),
    });
    stop = startTranscriptSync();
  });
  afterEach(() => stop());

  it('sends no request for an unrelated session delta, one incremental request when the active conversation advances', async () => {
    await open('s1');
    expect(conversationCalls()).toEqual(['/api/v1/conversations/s1']);

    sessions().applyFrame(delta(1, 2, { conversation_versions: { s2: v(99) } }));
    await flush();
    expect(conversationCalls()).toHaveLength(1);

    net.impl.mockClear();
    net.calls.length = 0;
    sessions().applyFrame(delta(2, 3, { conversation_versions: { s1: v(5) } }));
    await flush();
    expect(conversationCalls()).toEqual(['/api/v1/conversations/s1?after_seq=3&presentation_key=k']);
  });

  it('refetches and replaces the transcript when only the presentation key changes at the same sequence', async () => {
    await open('s1');
    net = mockFetch({
      '/api/v1/conversations/s1': () =>
        jsonResponse(
          transcript({
            latest_seq: 3,
            presentation_key: 'regrouped',
            reset: true,
            entries: [entry(3, { stable_id: 'thought:3' }), entry(1, { stable_id: 'tool:1' })],
          }),
        ),
    });
    sessions().applyFrame(delta(1, 1, { conversation_versions: { s1: v(3, 'regrouped') } }));
    await flush();
    expect(conversationCalls()).toEqual(['/api/v1/conversations/s1?after_seq=3&presentation_key=k']);
    expect(transcripts().entries.s1.orderedIds).toEqual(['thought:3', 'tool:1']);
    expect(transcripts().entries.s1.presentationKey).toBe('regrouped');
  });

  it('applies a transcript-only frame whose runtime cursor did not move, and ignores a bare repeat', async () => {
    await open('s1');
    net.calls.length = 0;
    expect(sessions().applyFrame(delta(1, 1))).toBe(false);
    await flush();
    expect(conversationCalls()).toHaveLength(0);

    expect(sessions().applyFrame(delta(1, 1, { conversation_versions: { s1: v(4), gone: v(9) } }))).toBe(true);
    expect(sessions().conversationVersions.s1).toEqual(v(4));
    expect(sessions().conversationVersions.gone).toBeUndefined();
    expect(sessions().cursor).toEqual(cursor(1));
    await flush();
    expect(conversationCalls()).toEqual(['/api/v1/conversations/s1?after_seq=3&presentation_key=k']);
  });

  describe('stopped sessions with stored history only', () => {
    const page = (before: { position: number; stable_id: string } | null = null) => ({
      items: [
        { stable_id: 'user:1', position: 1, role: 'user', text: 'do it' },
        { stable_id: 'agent:2', position: 2, role: 'agent', text: 'done' },
      ],
      before,
      frontier: 2,
    });
    const stopped = (id: string) => {
      const row = liveRow(id, { lifecycle: 'suspended', subagent_parent_id: 's1' });
      return {
        ...row,
        capabilities: { ...row.capabilities, open: false, read_history: true, resume: false },
      };
    };

    it('reads GET /sessions/{id}/history once and never asks for a live conversation', async () => {
      sessions().applySnapshot(snapshot([liveRow('s1'), stopped('c1')], { sequence: 1 }));
      net = mockFetch({ '/api/v1/sessions/c1/history': () => jsonResponse(page()) });
      await open('c1');
      const cached = transcripts().entries.c1;
      expect(cached.hasLoaded).toBe(true);
      expect(cached.blocked).toBeNull();
      expect(cached.olderItems.map(item => item.stable_id)).toEqual(['user:1', 'agent:2']);
      expect(cached.hasMoreHistory).toBe(false);
      expect(visibleOlderItems(cached)).toHaveLength(2);

      await open('c1');
      expect(net.urls()).toEqual(['/api/v1/sessions/c1/history']);
    });

    it('falls back to stored history when the live conversation answers 404', async () => {
      const row = liveRow('c2');
      sessions().applySnapshot(
        snapshot([{ ...row, capabilities: { ...row.capabilities, read_history: true } }], { sequence: 1 }),
      );
      net = mockFetch({
        '/api/v1/conversations/c2': () => jsonResponse({ error: 'gone' }, { status: 404 }),
        '/api/v1/sessions/c2/history': () => jsonResponse(page({ position: 1, stable_id: 'user:1' })),
      });
      await open('c2');
      const cached = transcripts().entries.c2;
      expect(cached.blocked).toBeNull();
      expect(cached.hasLoaded).toBe(true);
      expect(cached.olderItems).toHaveLength(2);
      expect(cached.hasMoreHistory).toBe(true);
    });
  });

  it('merges the incremental answer by stable identity and keeps order', async () => {
    await open('s1');
    net = mockFetch({
      '/api/v1/conversations/s1': () =>
        jsonResponse(
          transcript({
            latest_seq: 5,
            entries: [entry(3, { updated_seq: 5, lines: ['edited'] }), entry(4, { stable_id: 'tool:1' })],
          }),
        ),
    });
    sessions().applyFrame(delta(1, 2, { conversation_versions: { s1: v(5) } }));
    await flush();
    const cached = transcripts().entries.s1;
    expect(cached.orderedIds).toEqual(['ordinal:1', 'ordinal:2', 'ordinal:3', 'tool:1']);
    expect(cached.entriesById['ordinal:3'].lines).toEqual(['edited']);
    expect(cached.latestSeq).toBe(5);
  });

  it('returns to an unchanged cached session with zero requests', async () => {
    await open('s1');
    await open('s2');
    expect(conversationCalls()).toHaveLength(2);
    transcripts().closeConversation();
    net.calls.length = 0;
    await open('s1');
    await open('s2');
    await open('s1');
    expect(conversationCalls()).toHaveLength(0);
    expect(transcripts().entries.s1.orderedIds).toHaveLength(3);
  });

  it('fetches only the missing part when a cached session comes back behind the server', async () => {
    await open('s1');
    transcripts().closeConversation();
    sessions().applyFrame(delta(1, 2, { conversation_versions: { s1: v(6) } }));
    await flush();
    expect(conversationCalls()).toHaveLength(1); // not the active conversation: nothing fetched
    await open('s1');
    expect(conversationCalls().at(-1)).toBe('/api/v1/conversations/s1?after_seq=3&presentation_key=k');
  });

  it('coalesces deltas that arrive while a fetch is in flight into one follow-up', async () => {
    await open('s1');
    let release!: (response: Response) => void;
    let seen = 0;
    net = mockFetch({
      '/api/v1/conversations/s1': () => {
        seen += 1;
        return seen === 1
          ? new Promise<Response>(resolve => (release = resolve))
          : jsonResponse(transcript({ latest_seq: 9, entries: [entry(9)] }));
      },
    });
    sessions().applyFrame(delta(1, 2, { conversation_versions: { s1: v(6) } }));
    await flush(1);
    sessions().applyFrame(delta(2, 3, { conversation_versions: { s1: v(7) } }));
    sessions().applyFrame(delta(3, 4, { conversation_versions: { s1: v(8) } }));
    release(jsonResponse(transcript({ latest_seq: 6, entries: [entry(6)] })));
    await flush();
    expect(conversationCalls()).toHaveLength(2);
    expect(transcripts().entries.s1.latestSeq).toBe(9);
  });

  // Hard-won: pre-push review round 4: a fetch answer at the same sequence overwrote a newer presentation key from the stream.
  it('keeps a presentation key the stream announced while an older fetch was in flight, then fetches it', async () => {
    await open('s1');
    let release!: (response: Response) => void;
    let seen = 0;
    net = mockFetch({
      '/api/v1/conversations/s1': () => {
        seen += 1;
        return seen === 1
          ? new Promise<Response>(resolve => (release = resolve))
          : jsonResponse(
              transcript({
                latest_seq: 5,
                presentation_key: 'B',
                reset: true,
                entries: [entry(5, { stable_id: 'regrouped:5' })],
              }),
            );
      },
    });
    sessions().applyFrame(delta(1, 2, { conversation_versions: { s1: v(5, 'A') } }));
    await flush(1);
    sessions().applyFrame(delta(2, 3, { conversation_versions: { s1: v(5, 'B') } }));
    release(jsonResponse(transcript({ latest_seq: 5, presentation_key: 'A', entries: [entry(5)] })));
    await flush();

    expect(conversationCalls()).toEqual([
      '/api/v1/conversations/s1?after_seq=3&presentation_key=k',
      '/api/v1/conversations/s1?after_seq=5&presentation_key=A',
    ]);
    expect(sessions().conversationVersions.s1.presentation_key).toBe('B');
    expect(transcripts().entries.s1.presentationKey).toBe('B');
    expect(transcripts().entries.s1.orderedIds).toEqual(['regrouped:5']);
  });

  it('does not retry by itself after a failed fetch', async () => {
    await open('s1');
    net = mockFetch({ '/api/v1/conversations/s1': () => new Response('boom', { status: 500 }) });
    sessions().applyFrame(delta(1, 2, { conversation_versions: { s1: v(6) } }));
    sessions().applyFrame(delta(2, 3, { conversation_versions: { s1: v(7) } }));
    await flush();
    expect(transcripts().entries.s1.error).toBe('boom');
    const after = conversationCalls().length;
    await flush();
    expect(conversationCalls()).toHaveLength(after);
  });

  // Hard-won: pre-push review round 2: a transcript fetch that failed during a stream drop never retried.
  it('retries a failed fetch when the stream reconnects or the browser comes back online', async () => {
    await open('s1');
    net = mockFetch({ '/api/v1/conversations/s1': () => new Response('boom', { status: 500 }) });
    sessions().applyFrame(delta(1, 2, { conversation_versions: { s1: v(6) } }));
    await flush();
    expect(transcripts().entries.s1.error).toBe('boom');
    expect(transcripts().entries.s1.latestSeq).toBe(3);

    useSessionStore.setState({ streamStatus: 'reconnecting' });
    net = mockFetch({
      '/api/v1/conversations/s1': () => jsonResponse(transcript({ latest_seq: 6, entries: [entry(6)] })),
    });
    useSessionStore.setState({ streamStatus: 'live' });
    await flush();
    expect(conversationCalls()).toEqual(['/api/v1/conversations/s1?after_seq=3&presentation_key=k']);
    expect(transcripts().entries.s1.error).toBeNull();
    expect(transcripts().entries.s1.latestSeq).toBe(6);

    // The same recovery on `online`, and nothing when the cache is already current.
    net = mockFetch({ '/api/v1/conversations/s1': () => new Response('boom', { status: 500 }) });
    sessions().applyFrame(delta(2, 3, { conversation_versions: { s1: v(9) } }));
    await flush();
    expect(transcripts().entries.s1.error).toBe('boom');
    net = mockFetch({
      '/api/v1/conversations/s1': () => jsonResponse(transcript({ latest_seq: 9, entries: [entry(9)] })),
    });
    window.dispatchEvent(new Event('online'));
    await flush();
    expect(transcripts().entries.s1.latestSeq).toBe(9);
    net.calls.length = 0;
    window.dispatchEvent(new Event('online'));
    await flush();
    expect(conversationCalls()).toHaveLength(0);
  });

  it('fetches again when a transitioning conversation becomes readable', async () => {
    sessions().applyFrame(
      delta(1, 2, {
        sessions: [['s1', liveRow('s1', { transitioning: true, verdict: { ...liveRow('s1').verdict, transition: 'moving' } })]],
      }),
    );
    net = mockFetch({
      '/api/v1/conversations/s1': () => new Response('{"error":"transitioning"}', { status: 409 }),
    });
    await open('s1');
    expect(transcripts().entries.s1.blocked).toBe('transitioning');

    net = mockFetch({ '/api/v1/conversations/s1': () => jsonResponse(transcript()) });
    sessions().applyFrame(delta(2, 3, { sessions: [['s1', liveRow('s1')]] }));
    await flush();
    expect(conversationCalls()).toHaveLength(1);
    expect(transcripts().entries.s1.blocked).toBeNull();
    expect(transcripts().entries.s1.hasLoaded).toBe(true);
  });

  it('keeps at most 16 sessions, never evicting the open one, and refetches an evicted session in full', async () => {
    const routes: Parameters<typeof mockFetch>[0] = {};
    for (let i = 0; i < TRANSCRIPT_CACHE_CAPACITY + 1; i += 1) {
      routes[`/api/v1/conversations/c${i}`] = () => jsonResponse(transcript());
    }
    net = mockFetch(routes);
    for (let i = 0; i < TRANSCRIPT_CACHE_CAPACITY + 1; i += 1) await open(`c${i}`);
    const cached = Object.keys(transcripts().entries);
    expect(cached).toHaveLength(TRANSCRIPT_CACHE_CAPACITY);
    expect(cached).not.toContain('c0');
    expect(cached).toContain(`c${TRANSCRIPT_CACHE_CAPACITY}`);
    net.calls.length = 0;
    await open('c0');
    expect(net.urls()).toEqual(['/api/v1/conversations/c0']);
  });

  it('drops a pending local prompt once the server row with its command_id arrives', async () => {
    await open('s1');
    transcripts().addOptimisticPrompt('s1', 'cmd-1', 'hello');
    transcripts().addOptimisticPrompt('s1', 'cmd-1', 'hello');
    expect(transcripts().entries.s1.pendingLocalPrompts).toHaveLength(1);
    net = mockFetch({
      '/api/v1/conversations/s1': () =>
        jsonResponse(transcript({ latest_seq: 4, entries: [entry(4, { role: 'user', command_id: 'cmd-1' })] })),
    });
    sessions().applyFrame(delta(1, 2, { conversation_versions: { s1: v(4) } }));
    await flush();
    expect(transcripts().entries.s1.pendingLocalPrompts).toHaveLength(0);
  });

  it('pages older history from the oldest live row and hides stored rows the live window already shows', async () => {
    await open('s1');
    net = mockFetch({
      '/api/v1/sessions/s1/history': () =>
        jsonResponse({
          items: [
            { stable_id: 'a', position: 0, role: 'user', text: 'old' },
            { stable_id: 'b', position: 1, role: 'agent', text: 'overlaps live row 1' },
          ],
          before: null,
          frontier: 3,
        }),
    });
    await transcripts().loadEarlierHistory('s1');
    expect(net.urls()).toEqual(['/api/v1/sessions/s1/history?before_position=1&before_id=']);
    const cached = transcripts().entries.s1;
    expect(cached.hasMoreHistory).toBe(false);
    expect(visibleOlderItems(cached).map(item => item.stable_id)).toEqual(['a']);
  });
});

describe('hash routing', () => {
  const cases: [string, Route][] = [
    ['#notifications', { name: 'notifications' }],
    ['#workspace/w1', { name: 'workspace', workspaceId: 'w1' }],
    ['#conversation/s1', { name: 'conversation', sessionId: 's1' }],
    ['#/conversation/s1', { name: 'conversation', sessionId: 's1' }],
    ['#workspace/w1/new', { name: 'new', workspaceId: 'w1' }],
    ['#workspace/w1/resume', { name: 'resume', workspaceId: 'w1' }],
    ['#workspace/w1/resume/s2', { name: 'resume', workspaceId: 'w1', sessionId: 's2' }],
    ['#workspace/w1/resume/archive/x9', { name: 'resume', workspaceId: 'w1', wikiId: 'x9' }],
    ['#workspace/w1/move/s2', { name: 'move', workspaceId: 'w1', sessionId: 's2' }],
    ['#targets', { name: 'targets' }],
    ['#quota', { name: 'quota' }],
    ['#settings', { name: 'settings', path: [] }],
    ['#settings/env/MY%20KEY', { name: 'settings', path: ['env', 'MY KEY'] }],
    ['#notifications/s1', { name: 'conversation', sessionId: 's1', from: 'notifications' }],
    ['#subagents/p1/s1', { name: 'conversation', sessionId: 's1', subagentParentId: 'p1' }],
    ['#reviewers/p1', { name: 'workspace', subagentParentId: 'p1', childKind: 'reviewer' }],
  ];

  it('parses every legacy hash form', () => {
    for (const [hash, route] of cases) expect(parseRoute(hash), hash).toEqual(route);
    expect(parseRoute('')).toEqual({ name: 'workspace' });
    expect(parseRoute('#nonsense/zz')).toEqual({ name: 'workspace' });
  });

  it('writes the canonical hash for each route, so parse(hash(route)) is the route', () => {
    for (const [hash, route] of cases) {
      const canonical = routeHash(route);
      expect(parseRoute(canonical), `${hash} -> ${canonical}`).toEqual(route);
    }
    expect(routeHash({ name: 'conversation', sessionId: 's1' })).toBe('#conversation/s1');
    expect(routeHash({ name: 'workspace' })).toBe('');
  });

  it('opens notifications by default only when something needs attention and the hash is implicit', () => {
    const attention = attentionSessions([liveRow('s1', { notification_card: { badge: 'needs_input', entries: [], dismissal: null, holder: null, form: null } }), liveRow('s2')]);
    expect(attention.map(row => row.id)).toEqual(['s1']);
    expect(isImplicitRoute('')).toBe(true);
    expect(isImplicitRoute('#workspace/w1')).toBe(false);
    expect(effectiveRoute({ name: 'workspace' }, true, true)).toEqual({ name: 'notifications' });
    expect(effectiveRoute({ name: 'workspace' }, true, false)).toEqual({ name: 'workspace' });
    expect(effectiveRoute({ name: 'workspace', workspaceId: 'w1' }, false, true)).toEqual({
      name: 'workspace',
      workspaceId: 'w1',
    });
  });

  it('navigate updates the store and location.hash, and a hashchange updates the store', async () => {
    const remove = installUiListeners();
    useUiStore.getState().navigate({ name: 'conversation', sessionId: 's4' });
    expect(location.hash).toBe('#conversation/s4');
    expect(useUiStore.getState().route).toEqual({ name: 'conversation', sessionId: 's4' });
    expect(useUiStore.getState().implicitRoute).toBe(false);

    location.hash = '#targets';
    await flush();
    expect(useUiStore.getState().route).toEqual({ name: 'targets' });
    remove();
  });

  it('opens an app URL from a notification through the router, ignoring other origins', () => {
    useUiStore.getState().navigateToUrl(`${location.origin}/#conversation/s8`);
    expect(useUiStore.getState().route).toEqual({ name: 'conversation', sessionId: 's8' });
    useUiStore.getState().navigateToUrl('https://elsewhere.example/#targets');
    expect(useUiStore.getState().route).toEqual({ name: 'conversation', sessionId: 's8' });
  });
});

describe('back layers, toasts, theme', () => {
  it('closes layers topmost first and reports whether one was open', () => {
    const remove = installUiListeners();
    const ui = useUiStore.getState();
    const closed: string[] = [];
    const unregisterA = ui.registerBackLayer(() => closed.push('a'));
    const unregisterB = ui.registerBackLayer(() => closed.push('b'));
    expect(useUiStore.getState().closeTopmostBackLayer()).toBe(true);
    unregisterB();
    expect(useUiStore.getState().closeTopmostBackLayer()).toBe(true);
    unregisterA();
    expect(closed).toEqual(['b', 'a']);
    expect(useUiStore.getState().closeTopmostBackLayer()).toBe(false);
    remove();
  });

  it('navigating retires open layers so closing one afterwards leaves history alone', () => {
    const remove = installUiListeners();
    const back = vi.spyOn(history, 'back');
    const unregister = useUiStore.getState().registerBackLayer(() => undefined);
    useUiStore.getState().navigate({ name: 'targets' });
    expect(useUiStore.getState().layers).toEqual([]);
    unregister();
    expect(back).not.toHaveBeenCalled();
    back.mockRestore();
    remove();
  });

  it('closes the topmost layer on Escape', () => {
    const remove = installUiListeners();
    const closed: string[] = [];
    const unregister = useUiStore.getState().registerBackLayer(() => closed.push('sheet'));
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', cancelable: true }));
    expect(closed).toEqual(['sheet']);
    unregister();
    remove();
  });

  it('queues and dismisses toasts, and applies and persists the theme without inline script', () => {
    const id = useUiStore.getState().pushToast('error', 'Nope');
    expect(useUiStore.getState().toasts).toMatchObject([{ id, kind: 'error', text: 'Nope' }]);
    useUiStore.getState().dismissToast(id);
    expect(useUiStore.getState().toasts).toEqual([]);

    useUiStore.getState().setTheme('light');
    expect(document.documentElement.getAttribute('data-theme')).toBe('light');
    expect(localStorage.getItem('mj-theme')).toBe('light');
    useUiStore.getState().setTheme('system');
    expect(document.documentElement.hasAttribute('data-theme')).toBe(false);
  });
});

describe('applyTranscriptResponse keeps the server row order', () => {
  it('does not sort rows by id on reset, and appends new rows in server order on increments', () => {
    const loaded = applyTranscriptResponse(
      emptyTranscript('s1'),
      transcript({ reset: true, entries: [entry(5, { stable_id: 'thought:5' }), entry(2, { stable_id: 'tool:2' })] }),
    );
    expect(loaded.orderedIds).toEqual(['thought:5', 'tool:2']);

    const next = applyTranscriptResponse(
      loaded,
      transcript({
        latest_seq: 9,
        reset: false,
        entries: [
          entry(8, { stable_id: 'tool:8' }),
          entry(5, { stable_id: 'thought:5', updated_seq: 9, lines: ['edited'] }),
          entry(7, { stable_id: 'thought:7' }),
        ],
      }),
    );
    expect(next.orderedIds).toEqual(['thought:5', 'tool:2', 'tool:8', 'thought:7']);
    expect(next.entriesById['thought:5'].lines).toEqual(['edited']);
  });
});
