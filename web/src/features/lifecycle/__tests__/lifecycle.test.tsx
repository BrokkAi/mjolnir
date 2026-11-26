import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type {
  ContainerUpgradePreview,
  ControllerAction,
  SuspendPreflight,
  ViewerSession,
  ViewerTarget,
} from '../../../api/types';
import { useSessionStore } from '../../../stores/useSessionStore';
import { useUiStore } from '../../../stores/useUiStore';
import { jsonResponse, liveRow, metadata, mockFetch } from '../../../test-utils';
import { SessionActionModal } from '../SessionActionModal';
import { useLifecycleModal } from '../useLifecycleModal';

const GiB = 1024 ** 3;

const session = (overrides: Partial<ViewerSession> = {}): ViewerSession =>
  liveRow('s1', {
    title: 'Fix the build',
    given_title: 'Fix the build',
    workspace_id: 'w1',
    target_id: 't1',
    additional_mounts: [{ source: '/src', destination: '/work', read_only: true }],
    resource_allocation: { kind: 'container', cpus: 2, memory_bytes: 4 * GiB },
    container_upgrade_supported: true,
    ...overrides,
  });

const target: ViewerTarget = {
  id: 't1',
  kind: 'container',
  permission_mode: 'default',
  yolo: false,
  subagents_configured: false,
  oracle_configured: false,
  resource_allocation_kind: 'container',
  requires_project_directory: true,
  container_host_limits: { cpus: 8, memory_bytes: 16 * GiB },
  availability: { status: 'available' },
  recent_project_directories: [],
};

const preflight: SuspendPreflight = {
  session_id: 's1',
  untracked: [
    {
      checkout: { repository_id: 'r1', checkout_path: '/repo' },
      path: 'scratch.txt',
      bytes: 12,
      identity: 'sha256:abc',
    },
  ],
  nested_repositories: [],
  untracked_stashes: [],
  keep_dir_entries: [],
  keep_dir_bytes: 0,
  operations: [
    {
      checkout: { repository_id: 'r1', checkout_path: '/repo' },
      kind: 'Rebase',
    },
  ],
  raw: false,
  interrupting: true,
  children_not_handed_back: ['child-1'],
  owning_children_to_suspend: 1,
  blockers: [],
  existing_checkpoint: { verified: true, age_seconds: 120 },
};

const preview: ContainerUpgradePreview = {
  session_id: 's1',
  source_container_id: 'container-1',
  current_image: 'mjolnir:one',
  current_image_id: 'sha256:one',
  target_image: 'mjolnir:two',
  target_image_id: null,
  available: true,
  retained_mounts: ['/work'],
  warning:
    "State in the container's writable layer, including manually installed packages, may be lost.",
  settings: {
    resource_allocation: { kind: 'container', cpus: 4, memory_bytes: 8 * GiB },
    mounts: [{ source: '/src', destination: '/work', read_only: true }],
  },
};

function openModal(kind: 'suspend' | 'container-settings' | 'destroy') {
  act(() => {
    useLifecycleModal.getState().openModal({ kind, sessionId: 's1' });
  });
}

beforeEach(() => {
  useSessionStore.setState({
    sessionsById: { s1: session() },
    meta: metadata({
      workspaces: [{ id: 'w1', name: 'Main', last_opened_at: '2026-01-01T00:00:00Z' }],
      targets: [target],
    }),
    detailLoadingIds: {},
    detailErrors: {},
  });
  useUiStore.setState({ route: { name: 'conversation', sessionId: 's1' }, layers: [] });
  location.hash = '#conversation/s1';
});

afterEach(() => {
  act(() => useLifecycleModal.getState().closeModal());
  cleanup();
  useSessionStore.setState({ sessionsById: {}, meta: null, detailLoadingIds: {}, detailErrors: {} });
  useUiStore.setState({ route: { name: 'workspace' }, layers: [], toasts: [] });
  vi.unstubAllGlobals();
});

describe('Session lifecycle dialogs', () => {
  it('runs suspend preflight and sends PascalCase shown consent', async () => {
    const actions: ControllerAction[] = [];
    const fetchMock = mockFetch({
      '/api/v1/sessions/s1/suspend-preflight': () => jsonResponse(preflight),
      '/api/v1/actions': (_url, init) => {
        actions.push(JSON.parse(String(init?.body)) as ControllerAction);
        return new Response('', { status: 202 });
      },
    });

    render(<SessionActionModal />);
    openModal('suspend');

    expect(await screen.findByText('scratch.txt')).toBeInTheDocument();
    expect(screen.getByText(/Rebase: conflict resolution is discarded/)).toBeInTheDocument();
    expect(screen.getByText(/1 sub-agent has not handed back/)).toBeInTheDocument();
    expect(screen.getByText(/Existing checkpoint/)).toBeInTheDocument();
    expect(
      screen.getByText(/Suspending also suspends 1 child with their own environment/),
    ).toBeInTheDocument();

    // The preflight is asked for with the currently selected options.
    const preflightCall = fetchMock.calls.find(call =>
      call.url.includes('/suspend-preflight'),
    );
    expect(preflightCall?.init?.method ?? 'GET').toBe('GET');
    expect(preflightCall?.url).toContain('without_checkpoint=false');
    expect(preflightCall?.url).toContain('history_only=false');

    fireEvent.click(
      screen.getByRole('checkbox', {
        name: /untracked files, nested repositories, stashes and keep files will be discarded/,
      }),
    );
    fireEvent.click(
      screen.getByRole('checkbox', { name: /these Git operations will be aborted/ }),
    );
    fireEvent.click(screen.getByRole('button', { name: 'Suspend session' }));

    await waitFor(() => expect(actions).toHaveLength(1));
    expect(actions[0]).toEqual({
      action: 'suspend',
      session_id: 's1',
      discard_untracked: {
        Shown: [
          {
            Path: {
              checkout: { repository_id: 'r1', checkout_path: '/repo' },
              path: 'scratch.txt',
              identity: 'sha256:abc',
            },
          },
        ],
      },
      abort_operations: {
        Shown: [
          {
            checkout: { repository_id: 'r1', checkout_path: '/repo' },
            kind: 'Rebase',
          },
        ],
      },
    });
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
  });

  it('serializes mounts, previews a container upgrade, then confirms writable-layer loss', async () => {
    const actions: ControllerAction[] = [];
    mockFetch({
      '/api/v1/actions': (_url, init) => {
        const action = JSON.parse(String(init?.body)) as ControllerAction;
        actions.push(action);
        if (action.action === 'set-container-settings') return jsonResponse(preview);
        return new Response('', { status: 202 });
      },
    });

    render(<SessionActionModal />);
    openModal('container-settings');

    const cpus = await screen.findByLabelText('CPUs');
    const memory = screen.getByLabelText('Memory (GiB)');
    expect(cpus).toHaveValue(2);
    expect(memory).toHaveValue(4);
    expect(screen.getByLabelText('Mount 1 source')).toHaveValue('/src');
    expect(screen.getByLabelText('Mount 1 destination')).toHaveValue('/work');
    expect(screen.getByLabelText('Mount 1 access')).toHaveValue('ro');

    fireEvent.change(cpus, { target: { value: '4' } });
    fireEvent.change(memory, { target: { value: '8' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save container settings' }));

    expect(await screen.findByText('mjolnir:two')).toBeInTheDocument();
    expect(screen.getAllByText(/writable layer, including manually installed packages/).length).toBeGreaterThan(0);
    expect(actions).toHaveLength(1);
    expect(actions[0]).toEqual({
      action: 'set-container-settings',
      session_id: 's1',
      resource_allocation: { kind: 'container', cpus: 4, memory_bytes: 8 * GiB },
      mounts: [{ source: '/src', destination: '/work', read_only: true }],
    });

    fireEvent.click(
      screen.getByRole('checkbox', {
        name: /I understand that state in the container's writable layer/,
      }),
    );
    fireEvent.click(screen.getByRole('button', { name: 'Upgrade container' }));

    await waitFor(() => expect(actions).toHaveLength(2));
    expect(actions[1]).toEqual({
      action: 'upgrade-container',
      request: { selection: preview, acknowledge_writable_layer_loss: true },
    });
  });

  it('destroys through the dedicated route and navigates away from the session', async () => {
    const fetchMock = mockFetch({
      '/api/v1/sessions/s1/destroy': () => new Response('', { status: 202 }),
    });

    render(<SessionActionModal />);
    openModal('destroy');

    expect(
      await screen.findByText(/Permanently remove this session, its environment/),
    ).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Destroy session' }));

    await waitFor(() => expect(fetchMock.calls).toHaveLength(1));
    expect(fetchMock.calls[0]?.url).toBe('/api/v1/sessions/s1/destroy');
    expect(fetchMock.calls[0]?.init?.method).toBe('POST');
    expect(fetchMock.calls.some(call => call.url.includes('/api/v1/actions'))).toBe(false);

    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(useUiStore.getState().route).toEqual({ name: 'workspace', workspaceId: 'w1' });
  });
});
