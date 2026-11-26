import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type {
  MovePreparation,
  MoveSelection,
  ViewerProfile,
  ViewerTarget,
} from '../../../api/types';
import { useSessionStore } from '../../../stores/useSessionStore';
import { useUiStore } from '../../../stores/useUiStore';
import { jsonResponse, liveRow, mockFetch, snapshot } from '../../../test-utils';
import MoveView from '../MoveView';

const GIB = 1024 ** 3;

function viewerProfile(id: string): ViewerProfile {
  return {
    id,
    harness_kind: 'claude',
    capabilities_key: `${id}-caps`,
    availability: { status: 'available' },
  };
}

function viewerTarget(id: string, overrides: Partial<ViewerTarget> = {}): ViewerTarget {
  return {
    id,
    kind: 'local-docker',
    yolo: false,
    subagents_configured: true,
    oracle_configured: true,
    resource_allocation_kind: 'container',
    requires_project_directory: false,
    remembered_container_size: { cpus: 2, memory_bytes: 8 * GIB },
    default_resource_allocation: { kind: 'container', cpus: 2, memory_bytes: 8 * GIB },
    availability: { status: 'available' },
    recent_project_directories: [],
    ...overrides,
  };
}

const SESSION = liveRow('s1', {
  title: 'Move me',
  profile_id: 'p1',
  target_id: 't1',
  compatible_resume_targets: ['t2'],
  incompatible_resume_targets: [],
  resource_allocation: { kind: 'container', cpus: 2, memory_bytes: 8 * GIB },
  additional_mounts: [{ source: '/host/x', destination: '/work/x', read_only: true }],
  subagents: { mode: 'native' },
  oracle: false,
});

function baseSelection(): MoveSelection {
  return {
    session_id: 's1',
    profile_id: 'p1',
    target_template_id: 't2',
    clear_resource_allocation: false,
    additional_mounts: [{ source: '/host/x', destination: '/work/x', read_only: true }],
    resource_allocation: { kind: 'container', cpus: 2, memory_bytes: 8 * GIB },
    subagents: { mode: 'native' },
    oracle: false,
    oracle_choice: null,
    skip_keep_paths: [],
  };
}

function preparation(skip: string[]): MovePreparation {
  return {
    destination_checks: 'checked',
    route_preview: 'direct_or_daemon_stream',
    in_place: false,
    cross_harness: true,
    stops_subagents: true,
    active: true,
    queued_commands: [
      {
        command_id: 'c1',
        content: [{ type: 'text', text: 'do the thing' }],
        queued_at_ms: 1,
      },
    ],
    skip_keep_sizes: [['cache/big.bin', 2048]],
    suspend_preflight: {
      session_id: 's1',
      untracked: [],
      nested_repositories: [],
      untracked_stashes: [],
      keep_dir_entries: [],
      keep_dir_bytes: 1234,
      operations: [],
      raw: false,
      interrupting: true,
      children_not_handed_back: [],
      blockers: [],
      dry_run: { transfer_route: 'direct_push', keep_dir_bytes: 1234 },
    },
    selection: { ...baseSelection(), skip_keep_paths: skip },
    source_profile_id: 'p1',
    source_target_template_id: 't1',
    fingerprint: 'fingerprint-1',
    operation_id: 'operation-1',
  };
}

beforeEach(() => {
  location.hash = '';
  useUiStore.setState({
    toasts: [],
    layers: [],
    drawerOpen: false,
    route: { name: 'workspace' },
  });
  useSessionStore.getState().applySnapshot(
    snapshot([SESSION], {
      meta: {
        profiles: [viewerProfile('p1')],
        targets: [viewerTarget('t1'), viewerTarget('t2')],
        bundles: [],
      },
    }),
  );
});

afterEach(() => {
  cleanup();
  useSessionStore.getState().handleSignedOut();
  useUiStore.setState({ toasts: [], layers: [], drawerOpen: false });
  location.hash = '';
  vi.unstubAllGlobals();
});

describe('move session', () => {
  it('prepares a move, renders destination checks and previews, and dispatches the confirmed move', async () => {
    const prepareBodies: MoveSelection[] = [];
    const actionBodies: Record<string, unknown>[] = [];
    mockFetch({
      '/api/v1/moves/prepare': (_url, init) => {
        const selection = JSON.parse(String(init?.body)) as MoveSelection;
        prepareBodies.push(selection);
        return jsonResponse(preparation(selection.skip_keep_paths ?? []));
      },
      '/api/v1/actions': (_url, init) => {
        actionBodies.push(JSON.parse(String(init?.body)) as Record<string, unknown>);
        return new Response('', { status: 202 });
      },
    });

    render(<MoveView workspaceId="w1" sessionId="s1" />);
    expect(screen.getByText('Move me')).toBeInTheDocument();
    expect(screen.getByText(/2 CPUs · 8 GiB RAM/)).toBeInTheDocument();

    const prepare = screen.getByRole('button', { name: 'Prepare move' });
    expect(prepare).toBeEnabled();
    fireEvent.click(prepare);

    expect(await screen.findByRole('heading', { name: 'Preparation' })).toBeInTheDocument();
    expect(prepareBodies).toHaveLength(1);
    expect(prepareBodies[0]).toMatchObject({
      session_id: 's1',
      profile_id: 'p1',
      target_template_id: 't2',
      clear_resource_allocation: false,
      additional_mounts: [{ source: '/host/x', destination: '/work/x', read_only: true }],
      resource_allocation: { kind: 'container', cpus: 2, memory_bytes: 8 * GIB },
    });

    expect(screen.getByText('Direct push to the destination')).toBeInTheDocument();
    expect(screen.getByText(/different harness or model service/)).toBeInTheDocument();
    expect(screen.getByText(/Live sub-agents are stopped/)).toBeInTheDocument();
    expect(screen.getByText(/interrupts the active turn/)).toBeInTheDocument();
    expect(screen.getByText(/1 queued command/)).toBeInTheDocument();
    expect(screen.getByText(/do the thing/)).toBeInTheDocument();

    const skip = screen.getByRole('checkbox', { name: /cache\/big\.bin/ });
    fireEvent.click(skip);
    await waitFor(() => expect(prepareBodies).toHaveLength(2));
    expect(prepareBodies[1].skip_keep_paths).toEqual(['cache/big.bin']);
    await waitFor(() =>
      expect(screen.getByRole('checkbox', { name: /cache\/big\.bin/ })).toBeChecked(),
    );

    const confirm = screen.getByRole('button', { name: 'Confirm move' });
    expect(confirm).toBeDisabled();

    fireEvent.click(
      screen.getByRole('checkbox', {
        name: /active turn is interrupted and the session is checkpointed/i,
      }),
    );
    fireEvent.click(
      screen.getByRole('radio', { name: /Run queued work after the destination is ready/ }),
    );
    await waitFor(() => expect(confirm).toBeEnabled());
    fireEvent.click(confirm);

    await waitFor(() => expect(actionBodies).toHaveLength(1));
    const body = actionBodies[0] as {
      action: string;
      request: {
        preparation: MovePreparation;
        queue: string;
        acknowledge_interruption: boolean;
      };
    };
    expect(body.action).toBe('move');
    expect(body.request.acknowledge_interruption).toBe(true);
    expect(body.request.queue).toBe('start');
    expect(body.request.preparation.fingerprint).toBe('fingerprint-1');
    expect(body.request.preparation.selection.skip_keep_paths).toEqual(['cache/big.bin']);
    expect(useUiStore.getState().route).toEqual({ name: 'conversation', sessionId: 's1' });
  });
});
