import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type {
  LaunchOptions,
  PreflightNew,
  ProjectBundleSnapshot,
  ProjectCatalogView,
  ViewerProfile,
  ViewerTarget,
} from '../../../api/types';
import { useSessionStore } from '../../../stores/useSessionStore';
import { useUiStore } from '../../../stores/useUiStore';
import { jsonResponse, mockFetch, snapshot } from '../../../test-utils';
import NewSessionView from '../NewSessionView';

const GIB = 1024 ** 3;

function viewerProfile(id: string, overrides: Partial<ViewerProfile> = {}): ViewerProfile {
  return {
    id,
    harness_kind: 'claude',
    capabilities_key: `${id}-caps`,
    availability: { status: 'available' },
    ...overrides,
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
    container_host_limits: { cpus: 8, memory_bytes: 32 * GIB },
    default_resource_allocation: { kind: 'container', cpus: 2, memory_bytes: 8 * GIB },
    availability: { status: 'available' },
    recent_project_directories: ['/home/me/app'],
    ...overrides,
  };
}

const PROJECT_BUNDLE: ProjectBundleSnapshot = {
  bundle: {
    primary_repo: 'app',
    repositories: [{ id: 'app', github: 'owner/app', destination: '.' }],
  },
  identities: {},
};

const LAUNCH_OPTIONS: LaunchOptions = {
  revision: 7,
  primary_configured: false,
  profiles: [{ id: 'p1', harness: 'claude' }],
  targets: [
    {
      id: 't1',
      kind: 'local-docker',
      requires_project_directory: false,
      availability: 'ready',
    },
  ],
  bundles: [
    {
      id: 'b1',
      primary_repository: 'app',
      repositories: [{ id: 'app', github: 'owner/app', destination: '.' }],
    },
  ],
  default: { profile_id: 'p1', target_id: 't1' },
};

const PROJECT_CATALOG: ProjectCatalogView = {
  projects: [{ bundle_id: 'b1', name: 'App', project: PROJECT_BUNDLE }],
  locations: [],
  status: { state: 'ready' },
};

function preflight(overrides: Partial<PreflightNew> = {}): PreflightNew {
  return {
    managed_worktree: { available: false, default_create: false },
    remote_repairs: [],
    dirty_repositories: [],
    remote_repositories: [],
    local_changes_excluded: false,
    ...overrides,
  };
}

function seedStore() {
  useSessionStore.getState().applySnapshot(
    snapshot([], {
      meta: {
        profiles: [viewerProfile('p1')],
        targets: [viewerTarget('t1')],
        bundles: [{ id: 'b1', primary_repository: 'app', repositories: [] }],
      },
    }),
  );
}

function launchButton(): HTMLButtonElement {
  return screen.getByRole('button', { name: 'Launch' }) as HTMLButtonElement;
}

beforeEach(() => {
  seedStore();
  location.hash = '';
  useUiStore.setState({ toasts: [], layers: [], drawerOpen: false, route: { name: 'workspace' } });
});

afterEach(() => {
  cleanup();
  useSessionStore.getState().handleSignedOut();
  useUiStore.setState({ toasts: [], layers: [], drawerOpen: false });
  location.hash = '';
  vi.unstubAllGlobals();
});

describe('new session wizard', () => {
  it('lazy-loads launch options and projects only when the view mounts', async () => {
    const { urls } = mockFetch({
      '/api/v1/options': () => jsonResponse(LAUNCH_OPTIONS),
      '/api/v1/projects': () => jsonResponse(PROJECT_CATALOG),
      '/api/v1/preflight/new': () => jsonResponse(preflight()),
      '/api/v1/actions': () => new Response('', { status: 202 }),
    });
    expect(urls()).toEqual([]);

    render(<NewSessionView workspaceId="w1" />);

    await waitFor(() => expect(launchButton()).toBeEnabled(), { timeout: 3000 });
    expect(urls().filter(url => url === '/api/v1/options')).toHaveLength(1);
    expect(urls().filter(url => url === '/api/v1/projects')).toHaveLength(1);
    expect((screen.getByLabelText('Profile') as HTMLSelectElement).value).toBe('p1');
    expect((screen.getByLabelText('Target') as HTMLSelectElement).value).toBe('t1');
    expect(screen.getByText('App')).toBeInTheDocument();
  });

  it('submits the exact AdditionalMount wire payload on the new action', async () => {
    const bodies: Record<string, unknown>[] = [];
    mockFetch({
      '/api/v1/options': () => jsonResponse(LAUNCH_OPTIONS),
      '/api/v1/projects': () => jsonResponse(PROJECT_CATALOG),
      '/api/v1/preflight/new': () => jsonResponse(preflight()),
      '/api/v1/actions': (_url, init) => {
        bodies.push(JSON.parse(String(init?.body)) as Record<string, unknown>);
        return new Response('', { status: 202 });
      },
    });

    render(<NewSessionView workspaceId="w1" />);
    await waitFor(() => expect(launchButton()).toBeEnabled(), { timeout: 3000 });

    fireEvent.click(screen.getByRole('button', { name: 'Add mount' }));
    fireEvent.change(screen.getByLabelText('Mount source 1'), { target: { value: '/host/data' } });
    fireEvent.change(screen.getByLabelText('Mount destination 1'), {
      target: { value: '/workspace/data' },
    });
    await waitFor(() => expect(launchButton()).toBeEnabled(), { timeout: 3000 });

    fireEvent.click(launchButton());
    await waitFor(() => expect(bodies).toHaveLength(1));

    const action = bodies[0];
    expect(action.action).toBe('new');
    expect(action.workspace_id).toBe('w1');
    expect(action.profile_id).toBe('p1');
    expect(action.target_id).toBe('t1');
    expect(action.bundle_id).toBe('b1');
    expect(action.resource_allocation).toEqual({ kind: 'container', cpus: 2, memory_bytes: 8 * GIB });
    expect(action.additional_mounts).toEqual([
      { source: '/host/data', destination: '/workspace/data', read_only: true },
    ]);
    expect(Object.keys((action.additional_mounts as Record<string, unknown>[])[0])).toEqual([
      'source',
      'destination',
      'read_only',
    ]);
  });

  it('gates launching a dirty project on acknowledging each repository', async () => {
    const bodies: Record<string, unknown>[] = [];
    mockFetch({
      '/api/v1/options': () => jsonResponse(LAUNCH_OPTIONS),
      '/api/v1/projects': () => jsonResponse(PROJECT_CATALOG),
      '/api/v1/preflight/new': () =>
        jsonResponse(preflight({ dirty_repositories: ['repo-a', 'repo-b'] })),
      '/api/v1/actions': (_url, init) => {
        bodies.push(JSON.parse(String(init?.body)) as Record<string, unknown>);
        return new Response('', { status: 202 });
      },
    });

    render(<NewSessionView workspaceId="w1" />);
    const repoA = await screen.findByRole('checkbox', { name: /repo-a/ }, { timeout: 3000 });
    const repoB = screen.getByRole('checkbox', { name: /repo-b/ });
    expect(launchButton()).toBeDisabled();

    fireEvent.click(repoA);
    expect(launchButton()).toBeDisabled();

    fireEvent.click(repoB);
    await waitFor(() => expect(launchButton()).toBeEnabled());
    fireEvent.click(launchButton());

    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0].dirty_ack).toEqual(['repo-a', 'repo-b']);
  });
});
