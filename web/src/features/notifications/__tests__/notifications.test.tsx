import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { NotificationCard, SessionRow } from '../../../api/types';
import { useSessionStore } from '../../../stores/useSessionStore';
import { useUiStore } from '../../../stores/useUiStore';
import { liveRow, metadata, mockFetch } from '../../../test-utils';
import NotificationsView from '../NotificationsView';

const NOW = Date.parse('2026-11-25T12:00:00Z');

function card(overrides: Partial<NotificationCard> = {}): NotificationCard {
  return {
    badge: 'needs_input',
    entries: [
      {
        completed_ordinal: 4,
        completed_at_ms: NOW - 60_000,
        kind: 'needs_input',
        message: { found: 'message-4' },
      },
    ],
    dismissal: { question: { turn_id: 'turn-1', completed_ordinal: 4 } },
    holder: null,
    form: null,
    since_ms: NOW - 120_000,
    ...overrides,
  };
}

function hydrate(rows: SessionRow[], overrides: Parameters<typeof metadata>[0] = {}) {
  useSessionStore.getState().handleSignedOut();
  useSessionStore.setState({
    auth: 'signed_in',
    meta: metadata({
      server_time_ms: NOW,
      auto_afk_minutes: 0,
      workspaces: [
        { id: 'w1', name: 'Alpha', last_opened_at: '2026-11-25T10:00:00Z' },
        { id: 'w2', name: 'Beta', last_opened_at: '2026-11-24T10:00:00Z' },
      ],
      ...overrides,
    }),
    sessionsById: Object.fromEntries(rows.map(row => [row.id, row])),
  });
}

function actionCalls(calls: { url: string; init?: RequestInit }[]) {
  return calls
    .filter(call => call.url === '/api/v1/actions')
    .map(call => JSON.parse(String(call.init?.body)) as Record<string, unknown>);
}

beforeEach(() => {
  useUiStore.setState({ layers: [], route: { name: 'notifications' } });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe('Notifications inbox', () => {
  it('lists only sessions needing attention, ordered newest first with priority breaking ties', () => {
    hydrate([
      liveRow('s1', {
        title: 'Needs a decision',
        attention: 'needs_input',
        last_message_at_ms: NOW - 1_000,
      }),
      liveRow('s2', {
        title: 'Failed launch',
        attention: 'failed',
        last_message_at_ms: NOW - 500,
      }),
      liveRow('s3', {
        title: 'Important note',
        attention: 'important',
        notification_card: card({ badge: 'important', since_ms: NOW - 5_000 }),
      }),
      liveRow('s6', { title: 'Tied question', attention: 'needs_input', last_message_at_ms: NOW - 500 }),
      liveRow('s4', {
        title: 'Muted session',
        attention: 'needs_input',
        notifications_ignored: true,
      }),
      liveRow('s5', { title: 'Just idle', attention: 'idle' }),
    ]);
    mockFetch({});
    render(<NotificationsView />);

    const items = screen.getAllByTestId(/^notification-/);
    expect(items.map(item => item.getAttribute('data-testid'))).toEqual([
      'notification-s6',
      'notification-s2',
      'notification-s1',
      'notification-s3',
    ]);
    expect(screen.getByText('4 awaiting you')).toBeInTheDocument();
    expect(screen.queryByText('Just idle')).not.toBeInTheDocument();
    expect(screen.queryByText('Muted session')).not.toBeInTheDocument();
    expect(screen.getAllByText('Alpha').length).toBeGreaterThan(0);
  });

  it('orders a card by its latest stack entry instead of its oldest item', () => {
    hydrate([
      liveRow('older', { title: 'Older', attention: 'failed', last_message_at_ms: NOW - 5_000 }),
      liveRow('stack', { title: 'New stack entry', notification_card: card({ since_ms: NOW - 120_000, entries: [
        { completed_ordinal: 1, completed_at_ms: NOW - 120_000, kind: 'important', message: 'none' },
        { completed_ordinal: 2, completed_at_ms: NOW - 1_000, kind: 'important', message: 'none' },
      ] }) }),
    ]);
    mockFetch({});
    render(<NotificationsView />);
    expect(screen.getAllByTestId(/^notification-/).map(item => item.getAttribute('data-testid')))
      .toEqual(['notification-stack', 'notification-older']);
  });

  it('renders stack entries newest-first and breaks timestamp ties by ordinal', () => {
    const entries: NotificationCard['entries'] = [
      { completed_ordinal: 1, completed_at_ms: NOW - 120_000, kind: 'needs_input', message: 'none' },
      { completed_ordinal: 2, completed_at_ms: NOW - 60_000, kind: 'important', message: 'none' },
      { completed_ordinal: 3, completed_at_ms: NOW - 1_000, kind: 'needs_input', message: 'none' },
      { completed_ordinal: 4, completed_at_ms: NOW - 1_000, kind: 'important', message: 'none' },
    ];
    hydrate([liveRow('stack', { notification_card: card({ entries }) })]);
    mockFetch({});
    render(<NotificationsView />);
    const item = screen.getByTestId('notification-stack');
    const rows = within(item).getAllByRole('button', { name: 'Show full message' }).map(button => button.parentElement?.textContent);
    expect(rows.map(row => row?.includes('Important update') ? 'important' : 'needs_input'))
      .toEqual(['important', 'needs_input', 'important', 'needs_input']);
    expect(entries.map(entry => entry.completed_ordinal)).toEqual([1, 2, 3, 4]);
  });

  it('dismisses the notification identity it showed and removes it optimistically', async () => {
    hydrate([
      liveRow('s1', {
        title: 'Needs a decision',
        attention: 'needs_input',
        notification_card: card({
          dismissal: { question: { turn_id: 'turn-9', completed_ordinal: 7 } },
        }),
      }),
    ]);
    const fetchMock = mockFetch({
      '/api/v1/actions': () => new Response(null, { status: 202 }),
    });
    render(<NotificationsView />);

    const item = screen.getByTestId('notification-s1');
    fireEvent.click(within(item).getByRole('button', { name: 'Dismiss' }));
    await waitFor(() => expect(actionCalls(fetchMock.calls)).toHaveLength(1));
    expect(actionCalls(fetchMock.calls)[0]).toEqual({
      action: 'dismiss-notification',
      session_id: 's1',
      target: { question: { turn_id: 'turn-9', completed_ordinal: 7 } },
    });
    await waitFor(() => expect(screen.queryByTestId('notification-s1')).not.toBeInTheDocument());
  });

  it('loads the full notification message lazily when a stack entry is expanded', async () => {
    hydrate([
      liveRow('s1', {
        title: 'Needs a decision',
        attention: 'needs_input',
        notification_card: card(),
      }),
    ]);
    const fetchMock = mockFetch({
      '/api/v1/sessions/s1/notification-message': () =>
        new Response(
          JSON.stringify({ session_id: 's1', text: '## Full message\n\n**bold detail**' }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        ),
    });
    render(<NotificationsView />);

    expect(fetchMock.calls).toHaveLength(0);
    const item = screen.getByTestId('notification-s1');
    fireEvent.click(within(item).getByRole('button', { name: 'Show full message' }));

    await waitFor(() => expect(fetchMock.calls).toHaveLength(1));
    expect(fetchMock.calls[0].url).toContain('/api/v1/sessions/s1/notification-message');
    expect(fetchMock.calls[0].url).toContain('message_id=message-4');
    expect(await screen.findByRole('heading', { name: 'Full message' })).toBeInTheDocument();
    expect(screen.getByText('bold detail')).toBeInTheDocument();
  });

  it('toggles Operator Away with set-operator-away', async () => {
    hydrate([liveRow('s1', { title: 'Needs a decision', attention: 'needs_input' })], {
      auto_afk_minutes: 0,
      operator_away: false,
    });
    const fetchMock = mockFetch({
      '/api/v1/actions': () => new Response(null, { status: 202 }),
    });
    render(<NotificationsView />);

    fireEvent.click(screen.getByRole('button', { name: 'Go AFK' }));
    await waitFor(() => expect(actionCalls(fetchMock.calls)).toHaveLength(1));
    expect(actionCalls(fetchMock.calls)[0]).toEqual({ action: 'set-operator-away', away: true });
  });

  it('shows the all-caught-up state when nothing is waiting', () => {
    hydrate([liveRow('s1', { attention: 'idle' })]);
    mockFetch({});
    render(<NotificationsView />);

    expect(screen.getByRole('heading', { name: 'All caught up' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Open Workspaces' })).toBeInTheDocument();
  });
});
