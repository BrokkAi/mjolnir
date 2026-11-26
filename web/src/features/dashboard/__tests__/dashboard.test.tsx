import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
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
    ]);
    mockFetch({});
    render(<DashboardView workspaceId="w1" />);

    expect(screen.getByTestId('session-card-a')).toBeInTheDocument();
    expect(screen.queryByTestId('session-card-b')).not.toBeInTheDocument();
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
