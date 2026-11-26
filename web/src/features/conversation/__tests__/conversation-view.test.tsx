import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { emptyTranscript, useTranscriptStore } from '../../../stores/useTranscriptStore';
import { useSessionStore } from '../../../stores/useSessionStore';
import { entry, jsonResponse, liveRow, mockFetch, snapshot } from '../../../test-utils';
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
});
