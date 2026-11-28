import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
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
import { jsonResponse, liveRow, mockFetch, snapshot } from '../../../test-utils';
import { formatGiB } from '../../gib';
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
  it('orders and filters bundles and resolves provider auto for the current target', async () => {
    const calls: Record<string, unknown>[] = [];
    const preflights: Record<string, unknown>[] = [];
    let releaseOld: (response: Response) => void = () => {};
    const oldResponse = new Promise<Response>(resolve => { releaseOld = resolve; });
    const options = {
      ...LAUNCH_OPTIONS,
      primary_configured: true,
      profiles: [{ id: 'p1', harness: 'claude' }, { id: 'p2', harness: 'claude' }, { id: 'p3', harness: 'codex' }],
      targets: [...LAUNCH_OPTIONS.targets, { ...LAUNCH_OPTIONS.targets[0], id: 't2' }],
      bundles: [LAUNCH_OPTIONS.bundles[0], { id: 'z-recent', primary_repository: 'needle', repositories: [{ id: 'repo', github: 'owner/thing', destination: '/special' }] }],
    };
    useSessionStore.setState(state => ({ sessionsById: { ...state.sessionsById, recent: { ...liveRow('recent'), bundle_id: 'z-recent', last_activity_at_ms: 123 } } }));
    mockFetch({
      '/api/v1/options': () => jsonResponse(options),
      '/api/v1/profiles/providers': () => jsonResponse({ providers: { p1: 'Anthropic', p2: 'Anthropic', p3: 'OpenAI' } }),
      '/api/v1/projects': () => jsonResponse(PROJECT_CATALOG),
      '/api/v1/profiles/select': (_url, init) => { const body = JSON.parse(String(init?.body)); calls.push(body); return body.target_template_id === 't1' ? oldResponse : jsonResponse('p2'); },
      '/api/v1/preflight/new': (_url, init) => { preflights.push(JSON.parse(String(init?.body))); return jsonResponse(preflight()); },
    });
    render(<NewSessionView workspaceId="w1" />);
    await screen.findByRole('button', { name: 'Auto (OpenAI)' });
    const headings = screen.getAllByRole('heading', { level: 2 }).map(heading => heading.textContent);
    expect(headings.indexOf('1 · Target')).toBeLessThan(headings.indexOf('2 · Project'));
    const list = screen.getByRole('listbox', { name: 'Project bundles' });
    expect(list).toHaveClass('max-h-60', 'overflow-y-auto');
    expect(within(list).getAllByRole('option')[0]).toHaveTextContent('z-recent');
    fireEvent.change(screen.getByLabelText('Search bundles'), { target: { value: '/SPECIAL' } });
    expect(within(list).getAllByRole('option')).toHaveLength(1);
    expect(within(list).getByRole('option')).toHaveTextContent('z-recent');
    expect(screen.queryByLabelText('Repository source')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '+ Create bundle from repository' }));
    expect(screen.getByLabelText('Repository source')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Auto (Anthropic)' }));
    await waitFor(() => expect(calls).toHaveLength(1));
    fireEvent.change(screen.getByLabelText('Target'), { target: { value: 't2' } });
    await waitFor(() => expect(calls).toHaveLength(2));
    await screen.findByRole('button', { name: 'Auto (Anthropic) → p2' });
    releaseOld(jsonResponse('p1'));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Auto (Anthropic) → p2' })).toBeInTheDocument());
    expect(calls[1]).toEqual({ provider: 'Anthropic', profile_id: null, target_template_id: 't2' });
    await waitFor(() => expect(preflights).toContainEqual(expect.objectContaining({ profile_id: 'p2', target_id: 't2' })));
    fireEvent.click(screen.getByRole('button', { name: 'p1 (claude)' }));
    expect(screen.getByRole('button', { name: 'p1 (claude)' })).toHaveAttribute('aria-pressed', 'true');
  });

  it('reports a provider lookup failure, permits manual selection, and retries the lookup', async () => {
    let requests = 0;
    mockFetch({
      '/api/v1/options': () => jsonResponse({ ...LAUNCH_OPTIONS, primary_configured: true, default: { target_id: 't1' } }),
      '/api/v1/projects': () => jsonResponse(PROJECT_CATALOG),
      '/api/v1/profiles/providers': () => ++requests === 1
        ? new Response(JSON.stringify({ error: 'provider service unavailable' }), { status: 503 })
        : jsonResponse({ providers: { p1: 'Anthropic' } }),
      '/api/v1/preflight/new': () => jsonResponse(preflight()),
    });
    render(<NewSessionView workspaceId="w1" />);
    const notice = await screen.findByRole('alert');
    expect(notice).toHaveTextContent('Could not load provider groups: provider service unavailable');
    expect(screen.queryByRole('button', { name: /Auto \(Anthropic\)/ })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'p1 (claude)' }));
    expect(screen.getByRole('button', { name: 'p1 (claude)' })).toHaveAttribute('aria-pressed', 'true');
    fireEvent.click(within(notice).getByRole('button', { name: 'Retry' }));
    await screen.findByRole('button', { name: 'Auto (Anthropic)' });
    expect(requests).toBe(2);
    expect(await screen.findByRole('button', { name: 'p1 (claude)' })).toHaveAttribute('aria-pressed', 'true');
  });

  it('keeps profiles missing from the provider map available for manual selection', async () => {
    mockFetch({
      '/api/v1/options': () => jsonResponse(LAUNCH_OPTIONS),
      '/api/v1/projects': () => jsonResponse(PROJECT_CATALOG),
      '/api/v1/profiles/providers': () => jsonResponse({ providers: {} }),
      '/api/v1/preflight/new': () => jsonResponse(preflight()),
    });
    render(<NewSessionView workspaceId="w1" />);
    expect(await screen.findByRole('alert')).toHaveTextContent('No provider returned for p1');
    expect(screen.getByRole('button', { name: 'p1 (claude)' })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.queryByRole('button', { name: 'Auto (Anthropic)' })).not.toBeInTheDocument();
  });

  it('retries a failed provider Auto selection without switching accounts', async () => {
    let selections = 0;
    mockFetch({
      '/api/v1/options': () => jsonResponse(LAUNCH_OPTIONS),
      '/api/v1/projects': () => jsonResponse(PROJECT_CATALOG),
      '/api/v1/profiles/providers': () => jsonResponse({ providers: { p1: 'Anthropic' } }),
      '/api/v1/profiles/select': () => ++selections === 1
        ? new Response(JSON.stringify({ error: 'quota unavailable' }), { status: 503 })
        : jsonResponse('p1'),
      '/api/v1/preflight/new': () => jsonResponse(preflight()),
    });
    render(<NewSessionView workspaceId="w1" />);
    fireEvent.click(await screen.findByRole('button', { name: 'Auto (Anthropic)' }));
    const notice = await screen.findByRole('alert');
    expect(notice).toHaveTextContent('quota unavailable');
    fireEvent.click(within(notice).getByRole('button', { name: 'Retry' }));
    await screen.findByRole('button', { name: 'Auto (Anthropic) → p1' });
    expect(selections).toBe(2);
    await waitFor(() => expect(launchButton()).toBeEnabled());
  });

  it('formats fractional GiB without gluing the whole and fraction digits together', () => {
    expect([4, 4.5, 0.5, 1.25].map(gib => formatGiB(gib * GIB))).toEqual(['4', '4.5', '0.5', '1.25']);
  });

  it('offers Auto (Primary) when primary models are configured and launches with an empty profile_id', async () => {
    const preflightBodies: Record<string, unknown>[] = [];
    const actions: Record<string, unknown>[] = [];
    mockFetch({
      '/api/v1/options': () =>
        jsonResponse({ ...LAUNCH_OPTIONS, primary_configured: true, default: { target_id: 't1' } }),
      '/api/v1/profiles/providers': () => jsonResponse({ providers: { p1: 'Anthropic' } }),
      '/api/v1/projects': () => jsonResponse(PROJECT_CATALOG),
      '/api/v1/preflight/new': (_url, init) => {
        preflightBodies.push(JSON.parse(String(init?.body)) as Record<string, unknown>);
        return jsonResponse(preflight());
      },
      '/api/v1/actions': (_url, init) => {
        actions.push(JSON.parse(String(init?.body)) as Record<string, unknown>);
        return new Response('', { status: 202 });
      },
    });

    render(<NewSessionView workspaceId="w1" />);
    await waitFor(() => expect(launchButton()).toBeEnabled(), { timeout: 3000 });
    expect(screen.getByRole('button', { name: 'Auto (Primary)' })).toHaveAttribute('aria-pressed', 'true');
    expect(preflightBodies.at(-1)?.profile_id).toBe('');

    fireEvent.click(launchButton());
    await waitFor(() => expect(actions).toHaveLength(1));
    expect(actions[0]).toMatchObject({ action: 'new', profile_id: '', target_id: 't1' });
  });

  it('lazy-loads launch options and projects only when the view mounts', async () => {
    const { urls } = mockFetch({
      '/api/v1/options': () => jsonResponse(LAUNCH_OPTIONS),
      '/api/v1/profiles/providers': () => jsonResponse({ providers: { p1: 'Anthropic' } }),
      '/api/v1/projects': () => jsonResponse(PROJECT_CATALOG),
      '/api/v1/preflight/new': () => jsonResponse(preflight()),
      '/api/v1/actions': () => new Response('', { status: 202 }),
    });
    expect(urls()).toEqual([]);

    render(<NewSessionView workspaceId="w1" />);

    await waitFor(() => expect(launchButton()).toBeEnabled(), { timeout: 3000 });
    expect(urls().filter(url => url === '/api/v1/options')).toHaveLength(1);
    expect(urls().filter(url => url === '/api/v1/projects')).toHaveLength(1);
    expect(screen.getByRole('button', { name: 'p1 (claude)' })).toHaveAttribute('aria-pressed', 'true');
    expect((screen.getByLabelText('Target') as HTMLSelectElement).value).toBe('t1');
    expect(screen.getByText('App')).toBeInTheDocument();
  });

  it('submits the exact AdditionalMount wire payload on the new action', async () => {
    const bodies: Record<string, unknown>[] = [];
    mockFetch({
      '/api/v1/options': () => jsonResponse(LAUNCH_OPTIONS),
      '/api/v1/profiles/providers': () => jsonResponse({ providers: { p1: 'Anthropic' } }),
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
      '/api/v1/profiles/providers': () => jsonResponse({ providers: { p1: 'Anthropic' } }),
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
