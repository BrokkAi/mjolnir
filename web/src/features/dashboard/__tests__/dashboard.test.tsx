import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { SessionRow } from '../../../api/types';
import { useLifecycleModal } from '../../lifecycle/useLifecycleModal';
import { useSessionStore } from '../../../stores/useSessionStore';
import { useUiStore } from '../../../stores/useUiStore';
import { liveRow, metadata, mockFetch } from '../../../test-utils';
import DashboardView from '../DashboardView';

function hydrate(rows: SessionRow[], overrides: Parameters<typeof metadata>[0] = {}) {
  useSessionStore.getState().handleSignedOut();
  useSessionStore.setState({
    auth: 'signed_in',
    meta: metadata({
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
  useLifecycleModal.setState({ modal: null });
  useUiStore.setState({
    layers: [],
    route: { name: 'workspace' },
    implicitRoute: false,
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe('Workspaces dashboard', () => {
  it('filters to one workspace and links New and Resume at that workspace', () => {
    hydrate([
      liveRow('a', { title: 'Alpha session', workspace_id: 'w1' }),
      liveRow('b', { title: 'Beta session', workspace_id: 'w2' }),
      liveRow('stopped', { title: 'Stopped session', workspace_id: 'w1', verdict: { ...liveRow('stopped').verdict, live_dashboard: false } }),
    ]);
    mockFetch({});
    render(<DashboardView workspaceId="w1" />);

    expect(screen.getByTestId('session-card-a')).toBeInTheDocument();
    expect(screen.queryByTestId('session-card-b')).not.toBeInTheDocument();
    expect(screen.queryByTestId('session-card-stopped')).not.toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Alpha' })).toBeInTheDocument();
    expect(screen.getByRole('link', { name: /New session/ })).toHaveAttribute(
      'href',
      '#workspace/w1/new',
    );
    expect(screen.getByRole('link', { name: /Resume/ })).toHaveAttribute(
      'href',
      '#workspace/w1/resume',
    );

    fireEvent.click(screen.getByRole('button', { name: 'All workspaces' }));
    expect(useUiStore.getState().route).toEqual({ name: 'workspace' });

    cleanup();
    render(<DashboardView />);
    expect(screen.getByTestId('session-card-a')).toBeInTheDocument();
    expect(screen.getByTestId('session-card-b')).toBeInTheDocument();
  });

  it('shows active counts and keeps historical children reachable in actions', () => {
    const base = liveRow('idle');
    hydrate([
      liveRow('idle', { title: 'Idle parent', subagent_session_ids: ['child', 'review'], reviewer_session_ids: ['review'], verdict: { ...base.verdict, working_descendants: 0, working_children: 0 } }),
      liveRow('child', { subagent_parent_id: 'idle', active: false }),
      liveRow('review', { subagent_parent_id: 'idle', active: true, verdict: { ...base.verdict, working: false } }),
      liveRow('busy', { title: 'Busy parent', subagent_session_ids: ['managed'], verdict: { ...base.verdict, working_descendants: 3, working_children: 2 } }),
    ]);
    mockFetch({});
    render(<DashboardView />);
    const idle = within(screen.getByTestId('session-card-idle'));
    expect(idle.queryByRole('button', { name: /sub-agent|reviewer/i })).not.toBeInTheDocument();
    fireEvent.click(idle.getByRole('button', { name: 'Actions for Idle parent' }));
    fireEvent.click(idle.getByRole('menuitem', { name: 'Sub-agents' }));
    expect(useUiStore.getState().route).toEqual({ name: 'workspace', subagentParentId: 'idle' });
    fireEvent.click(idle.getByRole('button', { name: 'Actions for Idle parent' }));
    fireEvent.click(idle.getByRole('menuitem', { name: 'Reviewers' }));
    expect(useUiStore.getState().route).toEqual({ name: 'workspace', subagentParentId: 'idle', childKind: 'reviewer' });
    fireEvent.click(within(screen.getByTestId('session-card-busy')).getByRole('button', { name: /2 sub-agents/ }));
    expect(useUiStore.getState().route).toEqual({ name: 'workspace', subagentParentId: 'busy' });
  });

  it('opens the conversation for a native-only sub-agent count', () => {
    const base = liveRow('native');
    hydrate([liveRow('native', { title: 'Native parent', verdict: { ...base.verdict, working_children: 2 } })]);
    mockFetch({});
    render(<DashboardView />);
    fireEvent.click(within(screen.getByTestId('session-card-native')).getByRole('button', { name: '2 sub-agents' }));
    expect(useUiStore.getState().route).toEqual({ name: 'conversation', sessionId: 'native' });
  });

  it('counts working reviewers without inflating the worker pill or counting parked reviewers', () => {
    const base = liveRow('parent');
    hydrate([
      liveRow('parent', { title: 'Parent', subagent_session_ids: ['r1', 'r2'], reviewer_session_ids: ['r1', 'r2'], verdict: { ...base.verdict, working_descendants: 1, working_children: 0 } }),
      liveRow('r1', { subagent_parent_id: 'parent', active: true, verdict: { ...base.verdict, working: true } }),
      liveRow('r2', { subagent_parent_id: 'parent', active: true, verdict: { ...base.verdict, working: false } }),
      liveRow('missing', { title: 'Missing reviewers', reviewer_session_ids: ['r3', 'r4'], verdict: { ...base.verdict, review_working: true } }),
      liveRow('historical', { title: 'Historical reviewers', reviewer_session_ids: ['r5'], verdict: { ...base.verdict, review_working: true } }),
      liveRow('r5', { subagent_parent_id: 'historical', active: true, verdict: { ...base.verdict, working: false } }),
    ]);
    mockFetch({});
    render(<DashboardView />);
    const parent = within(screen.getByTestId('session-card-parent'));
    expect(parent.queryByRole('button', { name: /sub-agent/ })).not.toBeInTheDocument();
    fireEvent.click(parent.getByRole('button', { name: '1 reviewer' }));
    expect(useUiStore.getState().route).toEqual({ name: 'workspace', subagentParentId: 'parent', childKind: 'reviewer' });
    for (const id of ['missing', 'historical']) {
      const card = within(screen.getByTestId(`session-card-${id}`));
      expect(card.getByRole('button', { name: 'Reviewing' })).toBeInTheDocument();
      expect(card.queryByRole('button', { name: /[0-9]+ reviewers?/ })).not.toBeInTheDocument();
      fireEvent.click(card.getByRole('button', { name: /Actions for/ }));
      expect(card.queryByRole('menuitem', { name: 'Reviewers' })).not.toBeInTheDocument();
      fireEvent.click(card.getByRole('button', { name: /Actions for/ }));
    }
  });

  it('opens a stopped sub-agent from its card as a conversation, not a resume', () => {
    const live = liveRow('c1');
    hydrate([
      liveRow('p', { title: 'Parent session', workspace_id: 'w1', subagent_session_ids: ['c1'] }),
      liveRow('c1', {
        title: 'Stopped child',
        workspace_id: 'w1',
        subagent_parent_id: 'p',
        lifecycle: 'suspended',
        capabilities: { ...live.capabilities, open: false, read_history: true, resume: false },
      }),
    ]);
    mockFetch({});
    render(<DashboardView subagentParentId="p" />);

    fireEvent.click(screen.getByRole('button', { name: 'Stopped child' }));
    expect(useUiStore.getState().route).toEqual({ name: 'conversation', sessionId: 'c1' });
  });

  it('shows a parent virtual sub-agent workspace and breadcrumbs back to the parent', () => {
    hydrate([
      liveRow('p', {
        title: 'Parent session',
        workspace_id: 'w1',
        subagent_session_ids: ['c1', 'c2'],
        reviewer_session_ids: ['c2'],
      }),
      liveRow('c1', {
        title: 'Worker child',
        workspace_id: 'w1',
        subagent_parent_id: 'p',
      }),
      liveRow('c2', {
        title: 'Review child',
        workspace_id: 'w1',
        subagent_parent_id: 'p',
      }),
    ]);
    mockFetch({});
    const view = render(<DashboardView subagentParentId="p" />);

    expect(screen.getByTestId('session-card-c1')).toBeInTheDocument();
    expect(screen.queryByTestId('session-card-c2')).not.toBeInTheDocument();
    expect(screen.queryByTestId('session-card-p')).not.toBeInTheDocument();
    expect(screen.getByText('Sub-agents')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /Parent session/ }));
    expect(useUiStore.getState().route).toEqual({ name: 'conversation', sessionId: 'p' });

    view.unmount();
    render(<DashboardView subagentParentId="p" childKind="reviewer" />);
    expect(screen.getByTestId('session-card-c2')).toBeInTheDocument();
    expect(screen.queryByTestId('session-card-c1')).not.toBeInTheDocument();
    expect(screen.getByText('Reviewers')).toBeInTheDocument();
  });

  it('renders only capability-gated quick actions and routes modal and direct actions', async () => {
    const base = liveRow('s1', { title: 'Capability session', workspace_id: 'w1' });
    hydrate([
      liveRow('s1', {
        title: 'Capability session',
        workspace_id: 'w1',
        capabilities: {
          ...base.capabilities,
          open: true,
          move_session: true,
          rename: true,
          restart: true,
          change_workspace: false,
          container_settings: false,
          suspend: false,
          destroy: false,
        },
      }),
    ]);
    const fetchMock = mockFetch({
      '/api/v1/actions': () => new Response(null, { status: 202 }),
    });
    render(<DashboardView workspaceId="w1" />);

    fireEvent.click(screen.getByRole('button', { name: /Actions for Capability session/ }));
    expect(screen.getByRole('menuitem', { name: 'Rename…' })).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: 'Move…' })).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: 'Restart agent' })).toBeInTheDocument();
    expect(screen.queryByRole('menuitem', { name: 'Suspend…' })).not.toBeInTheDocument();
    expect(screen.queryByRole('menuitem', { name: 'Destroy…' })).not.toBeInTheDocument();
    expect(screen.queryByRole('menuitem', { name: 'Change workspace…' })).not.toBeInTheDocument();
    expect(screen.queryByRole('menuitem', { name: 'Container settings…' })).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('menuitem', { name: 'Rename…' }));
    expect(useLifecycleModal.getState().modal).toEqual({ kind: 'rename', sessionId: 's1' });

    fireEvent.click(screen.getByRole('button', { name: /Actions for Capability session/ }));
    fireEvent.click(screen.getByRole('menuitem', { name: 'Restart agent' }));
    await waitFor(() => expect(actionCalls(fetchMock.calls)).toHaveLength(1));
    expect(actionCalls(fetchMock.calls)[0]).toEqual({ action: 'restart', session_id: 's1' });
  });

  it('shows and dismisses runtime notices and launch failures', () => {
    hydrate([liveRow('s1', { title: 'A session', workspace_id: 'w1' })], {
      notices: [{ id: 1, session_id: 's1', text: 'Disk is almost full.' }],
      launch_failures: [
        { id: 'failure-1', workspace_id: 'w1', error: 'Could not clone the repository.' },
      ],
    });
    mockFetch({});
    render(<DashboardView workspaceId="w1" />);

    expect(screen.getByText('Disk is almost full.')).toBeInTheDocument();
    expect(screen.getByText('A session could not be started')).toBeInTheDocument();
    expect(screen.getByText('Could not clone the repository.')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Dismiss notice' }));
    expect(screen.queryByText('Disk is almost full.')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss launch failure' }));
    expect(screen.queryByText('A session could not be started')).not.toBeInTheDocument();
  });
});
