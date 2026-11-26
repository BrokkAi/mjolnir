import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type {
  RawConversionPreview,
  SessionRowResponse,
  ViewerProfile,
  ViewerSession,
  ViewerTarget,
  WikiHitTranscript,
  WikiRow,
  WikiSearchPage,
  WikiSessionInfo,
} from '../../../api/types';
import { useSessionStore } from '../../../stores/useSessionStore';
import { useUiStore } from '../../../stores/useUiStore';
import { jsonResponse, liveRow, mockFetch, snapshot } from '../../../test-utils';
import ResumeView from '../ResumeView';

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

const CONVERSION: RawConversionPreview = {
  checkout: '/home/me/app',
  destination: '/workspace/app',
  branch: 'feature/raw',
  fetch_url: 'https://github.com/owner/app.git',
  push_urls: ['https://github.com/owner/app.git'],
  default_branch: 'main',
  unpushed_commits: 3,
  staged_files: 1,
  unstaged_files: 2,
  untracked_files: 4,
  untracked_bytes: 2048,
  host_checkout_retained: true,
};

function suspendedSummary(): ViewerSession {
  return {
    ...liveRow('s1', {
      title: 'Paused work',
      lifecycle: 'suspended',
      capabilities: { ...liveRow('s1').capabilities, resume: true, open: false },
    }),
    detail: false,
    queued_prompt_count: 0,
    pending_elicitation_count: 0,
    pending_elicitation_ids: [],
  } as unknown as ViewerSession;
}

function suspendedDetail(): ViewerSession {
  return liveRow('s1', {
    title: 'Paused work',
    lifecycle: 'suspended',
    profile_id: 'p1',
    target_id: 't1',
    compatible_resume_targets: ['t2'],
    incompatible_resume_targets: ['t3'],
    capabilities: { ...liveRow('s1').capabilities, resume: true, open: true },
  });
}

const WIKI_ROW: WikiRow = {
  id: 'wiki-1',
  tool: 'claude',
  project: 'app',
  title: 'Archived chat',
  started: '2026-01-01T00:00:00Z',
  msgs: 12,
  preview: 'A previous conversation',
  archived: true,
  native_id: null,
  snippet: 'the needle sits here',
  hel_session_id: null,
  target: 't1',
  profile: 'p1',
};

const WIKI_INFO: WikiSessionInfo = {
  wiki_id: 'wiki-1',
  tool: 'claude',
  path: '/archive/wiki-1',
  status: 'archived',
  mjolnir_session_id: null,
  profile_id: 'p1',
  target_template_id: 't1',
  harness: 'claude',
  title: 'Archived chat',
  project: 'app',
};

const WIKI_HITS: WikiHitTranscript = {
  blocks: [
    {
      role: 'assistant',
      text: 'the needle sits here',
      hits: [[4, 10]],
      omitted_before: 2,
      truncated: false,
    },
  ],
  omitted_after: 1,
};

function seedStore(sessions: ViewerSession[], targets?: ViewerTarget[], profiles?: ViewerProfile[]) {
  useSessionStore.getState().applySnapshot(
    snapshot(sessions, {
      meta: {
        profiles: profiles ?? [viewerProfile('p1')],
        targets: targets ?? [viewerTarget('t1'), viewerTarget('t2'), viewerTarget('t3')],
        bundles: [],
      },
    }),
  );
}

beforeEach(() => {
  location.hash = '';
  useUiStore.setState({
    toasts: [],
    layers: [],
    drawerOpen: false,
    route: { name: 'workspace' },
  });
});

afterEach(() => {
  cleanup();
  useSessionStore.getState().handleSignedOut();
  useUiStore.setState({ toasts: [], layers: [], drawerOpen: false });
  location.hash = '';
  vi.unstubAllGlobals();
});

describe('resume and SessionWiki', () => {
  it('runs the resume preflight, shows a raw-checkout conversion, and acknowledges before resuming', async () => {
    seedStore([suspendedSummary()]);
    const bodies: Record<string, unknown>[] = [];
    const { urls } = mockFetch({
      '/api/v1/sessions/s1/row': () =>
        jsonResponse({ revision: 1, row: suspendedDetail() } as SessionRowResponse),
      '/api/v1/preflight/resume': () =>
        jsonResponse({ kind: 'converting-raw-checkout', preview: CONVERSION }),
      '/api/v1/actions': (_url, init) => {
        bodies.push(JSON.parse(String(init?.body)) as Record<string, unknown>);
        return new Response('', { status: 202 });
      },
    });

    render(<ResumeView workspaceId="w1" sessionId="s1" />);

    expect(await screen.findByRole('button', { name: /Paused work/ })).toBeInTheDocument();
    expect(urls()).toContain('/api/v1/sessions/s1/row');
    expect(await screen.findByText(/Unpushed commits/)).toBeInTheDocument();
    expect(screen.getByText(/Host checkout retained/)).toBeInTheDocument();

    const resumeButton = screen.getByRole('button', { name: 'Resume session' });
    expect(resumeButton).toBeDisabled();

    fireEvent.click(
      screen.getByRole('checkbox', {
        name: /checkout will move into an isolated workspace/i,
      }),
    );
    await waitFor(() => expect(resumeButton).toBeEnabled());
    fireEvent.click(resumeButton);

    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0]).toMatchObject({
      action: 'resume',
      session_id: 's1',
      workspace_id: 'w1',
      profile_id: 'p1',
      target_id: 't2',
      queue: 'start',
    });
    expect(useUiStore.getState().route).toEqual({ name: 'conversation', sessionId: 's1' });
  });

  it('searches the wiki, renders the brief and highlighted hits, and restores a session', async () => {
    seedStore([]);
    const restoreBodies: Record<string, unknown>[] = [];
    const searches: string[] = [];
    mockFetch({
      '/api/v1/wiki/search': url => {
        searches.push(url);
        const page: WikiSearchPage = { rows: [WIKI_ROW], status: { state: 'ready' } };
        return jsonResponse(page);
      },
      '/api/v1/wiki/sessions/wiki-1': () => jsonResponse(WIKI_INFO),
      '/api/v1/wiki/sessions/wiki-1/brief': () =>
        jsonResponse({ markdown: '# Brief\nImportant **summary** of the work.' }),
      '/api/v1/wiki/sessions/wiki-1/hits': () => jsonResponse(WIKI_HITS),
      '/api/v1/wiki/sessions/wiki-1/restore': (_url, init) => {
        restoreBodies.push(JSON.parse(String(init?.body)) as Record<string, unknown>);
        return jsonResponse({ session_id: 'restored-1' });
      },
    });

    render(<ResumeView workspaceId="w1" />);
    fireEvent.click(screen.getByRole('tab', { name: 'Session Wiki' }));

    const result = await screen.findByRole('button', { name: /Archived chat/ }, { timeout: 3000 });
    expect(result).toHaveTextContent('app');
    expect(result).toHaveTextContent('12 messages');

    fireEvent.change(screen.getByLabelText('Search the SessionWiki'), {
      target: { value: 'needle' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    await waitFor(() => expect(searches.some(url => url.includes('q=needle'))).toBe(true));

    fireEvent.click(await screen.findByRole('button', { name: /Archived chat/ }));
    expect(await screen.findByText('summary')).toBeInTheDocument();
    expect(await screen.findByText('needle')).toBeInTheDocument();
    expect(screen.getByText(/2 earlier messages omitted/)).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Restore session' }));
    await waitFor(() => expect(restoreBodies).toHaveLength(1));
    expect(restoreBodies[0]).toEqual({
      workspace_id: 'w1',
      profile_id: 'p1',
      target_id: 't1',
    });
    expect(useUiStore.getState().route).toEqual({ name: 'conversation', sessionId: 'restored-1' });
  });

  it('opens the wiki tab directly when the route carries a wiki id', async () => {
    seedStore([]);
    const { urls } = mockFetch({
      '/api/v1/wiki/sessions/wiki-1': () => jsonResponse(WIKI_INFO),
      '/api/v1/wiki/sessions/wiki-1/brief': () => jsonResponse({ markdown: '# Archived brief' }),
    });

    render(<ResumeView workspaceId="w1" wikiId="wiki-1" />);

    expect(screen.getByRole('tab', { name: 'Session Wiki' })).toHaveAttribute('aria-selected', 'true');
    expect(await screen.findByText('Archived brief')).toBeInTheDocument();
    expect(urls().some(url => url.startsWith('/api/v1/wiki/search'))).toBe(false);
  });
});
