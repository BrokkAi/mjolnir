import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { emptyTranscript, useTranscriptStore } from '../../../stores/useTranscriptStore';
import { useSessionStore } from '../../../stores/useSessionStore';
import { useConversationRoute } from '../../../components/shell/useConversationRoute';
import { useUiStore } from '../../../stores/useUiStore';
import { cursor, entry, jsonResponse, liveRow, mockFetch, snapshot } from '../../../test-utils';
import ConversationView from '../ConversationView';

/**
 * The view reads transcript state; the shell's route hook owns opening and
 * closing the conversation. This smoke test pins that division so a future
 * change cannot start a second fetch or close the conversation on unmount.
 */

beforeEach(() => {
  useSessionStore.getState().handleSignedOut();
  useTranscriptStore.getState().clearAll();
  useSessionStore.getState().applySnapshot(snapshot([liveRow('s1', { title: 'Build the thing' })]));
  useTranscriptStore.setState({
    entries: {
      s1: {
        ...emptyTranscript('s1'),
        hasLoaded: true,
        latestSeq: 1,
        orderedIds: ['ordinal:1'],
        entriesById: { 'ordinal:1': entry(1, { lines: ['Hello from the agent'] }) },
      },
    },
    order: ['s1'],
    activeId: 's1',
  });
});

afterEach(() => {
  cleanup();
  useSessionStore.getState().handleSignedOut();
  useTranscriptStore.getState().clearAll();
  vi.unstubAllGlobals();
});

describe('ConversationView', () => {
  it('keeps historical sub-agents accessible and badges only working children', () => {
    mockFetch({ '/api/v1/sessions/s1/client-state': () => jsonResponse({ draft: '', through_event_ordinal: 0 }) });
    const row = liveRow('s1', { subagent_session_ids: ['c1'] });
    useSessionStore.getState().applySnapshot(snapshot([row]));
    render(<ConversationView sessionId="s1" />);
    const idle = screen.getAllByRole('button', { name: 'Sub-agents and reviewers' });
    expect(idle.length).toBeGreaterThan(0);
    expect(idle[0].querySelector('span')).toBeNull();
    act(() => useSessionStore.setState(state => ({ sessionsById: { ...state.sessionsById, s1: { ...row, verdict: { ...row.verdict, working_children: 2 } } } })));
    expect(screen.getAllByRole('button', { name: 'Sub-agents and reviewers, 2 active' })[0]).toHaveTextContent('2');
    act(() => useSessionStore.setState(state => ({ sessionsById: { ...state.sessionsById, s1: { ...row, detail: false, queued_prompt_count: 0, pending_elicitation_count: 0, pending_elicitation_ids: [], subagent_session_ids: [], reviewer_session_ids: [], verdict: { ...row.verdict, working_children: 2 } } } })));
    expect(screen.getAllByRole('button', { name: 'Sub-agents and reviewers, 2 active' })[0]).toHaveTextContent('2');
  });

  it('renders the single-scrollport conversation without opening or closing it', () => {
    const fetchMock = mockFetch({
      '/api/v1/sessions/s1/client-state': () =>
        jsonResponse({ draft: '', through_event_ordinal: 0 }),
    });

    const { unmount } = render(
      <ConversationView sessionId="s1" subagentParentId={undefined} />,
    );

    expect(screen.getByTestId('conversation-view')).toHaveClass('overflow-hidden');
    expect(screen.getByTestId('conversation-scroll')).toBeInTheDocument();
    expect(screen.getByText('Hello from the agent')).toBeInTheDocument();
    expect(screen.getByRole('heading', { level: 2 })).toHaveTextContent('Build the thing');
    expect(screen.getByTestId('composer')).toBeInTheDocument();

    // No conversation fetch: the shell already opened this transcript.
    expect(fetchMock.urls().filter(url => url.includes('/api/v1/conversations/'))).toHaveLength(0);

    unmount();
    expect(useTranscriptStore.getState().activeId).toBe('s1');
  });

  it('shows a stopped session\u2019s stored history with a read-only notice instead of a composer', () => {
    const row = liveRow('c1', { title: 'Stopped child', lifecycle: 'suspended' });
    useSessionStore.getState().applySnapshot(
      snapshot([
        { ...row, capabilities: { ...row.capabilities, open: false, read_history: true, resume: false } },
      ]),
    );
    useTranscriptStore.setState({
      entries: {
        c1: {
          ...emptyTranscript('c1'),
          hasLoaded: true,
          hasMoreHistory: false,
          olderItems: [{ stable_id: 'user:1', position: 1, role: 'user', text: 'stored question' }],
        },
      },
      order: ['c1'],
      activeId: 'c1',
    });
    mockFetch({});

    render(<ConversationView sessionId="c1" />);

    expect(screen.getByText('stored question')).toBeInTheDocument();
    expect(screen.queryByText(/No messages yet/)).not.toBeInTheDocument();
    expect(screen.queryByTestId('composer')).not.toBeInTheDocument();
    expect(screen.getByText(/stored history is shown read-only/)).toBeInTheDocument();
  });

  it('opens the full detail of a trimmed history row on demand', async () => {
    const row = liveRow('c1', { title: 'Stopped child', lifecycle: 'suspended' });
    useSessionStore.getState().applySnapshot(
      snapshot([
        { ...row, capabilities: { ...row.capabilities, open: false, read_history: true, resume: false } },
      ]),
    );
    useTranscriptStore.setState({
      entries: {
        c1: {
          ...emptyTranscript('c1'),
          hasLoaded: true,
          hasMoreHistory: false,
          olderItems: [
            {
              stable_id: 'tool:7',
              position: 1,
              role: 'tool',
              label: 'Run tests',
              text: 'summary only',
              tool_detail_trimmed: true,
            },
          ],
        },
      },
      order: ['c1'],
      activeId: 'c1',
    });
    const fetchMock = mockFetch({
      '/api/v1/sessions/c1/items/tool%3A7/detail': () =>
        jsonResponse({
          status: 'full',
          stable_id: 'tool:7',
          entry: { tool_input: { command: 'cargo test' }, tool_content: [] },
          body: { kind: 'system', text: '' },
          call: {},
          diffs: [],
          terminal_outputs: [{ terminal_id: 't1', output: 'full history output' }],
        }),
    });

    render(<ConversationView sessionId="c1" />);
    expect(fetchMock.calls.filter(call => call.url.includes('/detail'))).toHaveLength(0);

    fireEvent.click(screen.getByRole('button', { name: 'Full detail' }));

    await waitFor(() => expect(screen.getByText('full history output')).toBeInTheDocument());
    expect(screen.getByText(/"command": "cargo test"/)).toBeInTheDocument();
    expect(fetchMock.calls.filter(call => call.url.includes('/detail'))).toHaveLength(1);
    fireEvent.click(screen.getByRole('button', { name: 'Hide detail' }));
    expect(screen.queryByText('full history output')).not.toBeInTheDocument();
  });

  it('keeps the cached rows visible and shows an error banner with Retry when a later fetch failed', () => {
    const fetchMock = mockFetch({
      '/api/v1/sessions/s1/client-state': () => jsonResponse({ draft: '', through_event_ordinal: 0 }),
      '/api/v1/conversations/s1': () => jsonResponse({ latest_seq: 1, entries: [] }),
    });
    useTranscriptStore.setState(state => ({
      entries: { ...state.entries, s1: { ...state.entries.s1, error: 'network down' } },
    }));

    render(<ConversationView sessionId="s1" subagentParentId={undefined} />);

    expect(screen.getByText('Hello from the agent')).toBeInTheDocument();
    expect(screen.getByRole('alert')).toHaveTextContent('network down');
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    expect(fetchMock.urls().some(url => url.startsWith('/api/v1/conversations/s1'))).toBe(true);
  });

  it('keeps the conversation, composer and focus mounted while detail refreshes after detail_changed', async () => {
    useSessionStore.getState().applyFrame({
      kind: 'snapshot',
      cursor: cursor(1),
      snapshot: snapshot([liveRow('s1', { title: 'Build the thing' })], { sequence: 1 }),
    });
    let rowRequests = 0;
    let release!: () => void;
    const fetchMock = mockFetch({
      '/api/v1/sessions/s1/client-state': () => jsonResponse({ draft: '', through_event_ordinal: 0 }),
      '/api/v1/conversations/s1': () => jsonResponse({ latest_seq: 1, entries: [] }),
      '/api/v1/sessions/s1/row': () => {
        rowRequests += 1;
        return new Promise<Response>(resolve => {
          release = () => resolve(jsonResponse({ revision: 2, row: liveRow('s1', { title: 'Refreshed' }) }));
        });
      },
    });
    useUiStore.setState({ route: { name: 'conversation', sessionId: 's1' }, implicitRoute: false });
    function Harness() {
      useConversationRoute();
      return <ConversationView sessionId="s1" />;
    }
    render(<Harness />);

    const textarea = screen.getByLabelText('Message');
    const feed = screen.getByTestId('conversation-scroll');
    textarea.focus();
    expect(rowRequests).toBe(0);

    act(() => {
      useSessionStore.getState().applyFrame({
        kind: 'delta',
        from: cursor(1),
        cursor: cursor(2),
        metadata: { revision: 2, server_time_ms: 2, generated_at: 't2' },
        sessions: [],
        detail_changed: ['s1'],
        interned: {},
      });
    });

    // The background refresh starts, and nothing unmounted meanwhile.
    await waitFor(() => expect(rowRequests).toBe(1));
    expect(fetchMock.urls()).toContain('/api/v1/sessions/s1/row');
    expect(screen.queryByText('Loading session details…')).not.toBeInTheDocument();
    expect(screen.getByLabelText('Message')).toBe(textarea);
    expect(screen.getByTestId('conversation-scroll')).toBe(feed);
    expect(document.activeElement).toBe(textarea);

    await act(async () => {
      release();
    });
    await waitFor(() => expect(screen.getByRole('heading', { level: 2 })).toHaveTextContent('Refreshed'));
    expect(screen.getByLabelText('Message')).toBe(textarea);
    expect(document.activeElement).toBe(textarea);
    expect(useSessionStore.getState().staleDetailIds.s1).toBeUndefined();
  });

  it('retires an optimistic prompt once the session queues it, and it does not return when the queue empties', () => {
    mockFetch({ '/api/v1/sessions/s1/client-state': () => jsonResponse({ draft: '', through_event_ordinal: 0 }) });
    useTranscriptStore.getState().addOptimisticPrompt('s1', 'cmd-1', 'run the tests');
    useTranscriptStore.getState().addOptimisticPrompt('s1', 'cmd-2', 'another');
    render(<ConversationView sessionId="s1" />);
    expect(screen.getAllByText('run the tests')).toHaveLength(1);

    const queued = (ids: string[]) =>
      act(() => {
        useSessionStore.getState().applySnapshot(
          snapshot([
            liveRow('s1', {
              title: 'Build the thing',
              queued_prompts: ids.map(id => ({ id, text: 'run the tests', created_at: '1' })),
            }),
          ]),
        );
      });

    queued(['cmd-1']);
    const pending = () => useTranscriptStore.getState().entries.s1.pendingLocalPrompts.map(prompt => prompt.commandId);
    expect(pending()).toEqual(['cmd-2']);
    // The queue strip shows the prompt; the transcript no longer says "Sending…" for it.
    expect(screen.getAllByText('run the tests')).toHaveLength(1);

    queued([]);
    expect(pending()).toEqual(['cmd-2']);
  });
});
