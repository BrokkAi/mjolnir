// Tier 2 views: New Session (lazy catalog fetches, dirty-repo preflight,
// exact mount payloads), Resume & Session Wiki, Move, Targets, Quota,
// Settings with Git identity, and the Suspend/Destroy lifecycle dialogs.

const { test, expect } = require('@playwright/test');
const {
  PHONE,
  startServer,
  installCommonRoutes,
  guardedPage,
  signIn,
  postedActions,
} = require('./spec-helpers');
const { snapshot, liveSession, profile, target } = require('./fixtures');

const GIB = 1024 ** 3;
let server;

async function openDashboard(page) {
  await signIn(page, server);
  await expect(page.getByRole('heading', { name: 'Workspaces' })).toBeVisible();
}

function lastRequest(method, pattern) {
  const matches = server.requests(method, pattern);
  return matches[matches.length - 1];
}

test.beforeAll(async () => {
  server = await startServer();
  installCommonRoutes(server);
});

test.afterAll(async () => {
  await server.close();
});

test.beforeEach(() => {
  server.log.length = 0;
});

test('new session lazily loads catalogs, acknowledges a dirty repo and launches', async ({ browser }) => {
  server.setSnapshot(
    snapshot({
      meta: {
        targets: [
          target('localhost', { requires_project_directory: true, recent_project_directories: ['/proj/app'] }),
        ],
      },
      sessions: [liveSession('s1', { title: 'Existing' })],
    }),
  );
  server.handle('GET', '/api/v1/options', () => ({
    json: {
      revision: 1,
      profiles: [{ id: 'p1', harness: 'fake' }],
      targets: [{ id: 'localhost', kind: 'local', requires_project_directory: true, availability: 'ready' }],
      bundles: [],
      default: { profile_id: 'p1', target_id: 'localhost' },
    },
  }));
  server.handle('GET', '/api/v1/projects', () => ({
    json: { projects: [], locations: [], status: { state: 'ready' } },
  }));
  server.handle('POST', '/api/v1/paths/complete', () => ({ json: { candidates: [], truncated: false } }));
  server.handle('POST', '/api/v1/preflight/new', () => ({
    json: {
      managed_worktree: { available: false, default_create: false },
      remote_repairs: [],
      dirty_repositories: ['/proj/app'],
      remote_repositories: [],
      local_changes_excluded: false,
    },
  }));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openDashboard(page);
    expect(server.requests('GET', '/api/v1/options')).toHaveLength(0);
    await page.getByRole('link', { name: 'New session' }).click();
    await expect(page.getByRole('heading', { name: '1 · Target' })).toBeVisible();
    await expect(page.getByRole('heading', { name: '2 · Project' })).toBeVisible();
    // The catalogs load lazily, only now.
    expect(server.requests('GET', '/api/v1/options')).toHaveLength(1);
    expect(server.requests('GET', '/api/v1/projects')).toHaveLength(1);

    // The directory target pre-fills its most recent project directory.
    await expect(page.getByLabel('Project directory')).toHaveValue('/proj/app');
    // Preflight reports uncommitted changes; the launch stays locked until acknowledged.
    await expect(page.getByText('Uncommitted changes found')).toBeVisible();
    const launch = page.getByRole('button', { name: 'Launch' });
    await expect(launch).toBeDisabled();
    await page.getByRole('checkbox', { name: /uncommitted changes/ }).check();
    await expect(launch).toBeEnabled();
    await launch.click();

    await expect.poll(() => postedActions(server).filter(body => body.action === 'new').length).toBe(1);
    const action = postedActions(server).find(body => body.action === 'new');
    expect(action).toMatchObject({
      action: 'new',
      workspace_id: 'w1',
      profile_id: 'p1',
      target_id: 'localhost',
      project_directory: '/proj/app',
      dirty_ack: ['/proj/app'],
    });
    const preflights = server.requests('POST', '/api/v1/preflight/new');
    expect(preflights.length).toBeGreaterThan(0);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('additional mounts serialize with the exact read_only wire shape', async ({ browser }) => {
  server.setSnapshot(
    snapshot({
      meta: {
        targets: [
          target('localhost'),
          target('pod', {
            kind: 'container',
            resource_allocation_kind: 'container',
            default_resource_allocation: { kind: 'container', cpus: 4, memory_bytes: 8 * GIB },
            container_host_limits: { cpus: 16, memory_bytes: 32 * GIB },
          }),
        ],
      },
      sessions: [liveSession('s1', { title: 'Existing' })],
    }),
  );
  server.handle('GET', '/api/v1/options', () => ({
    json: {
      revision: 1,
      profiles: [{ id: 'p1', harness: 'fake' }],
      targets: [
        { id: 'localhost', kind: 'local', requires_project_directory: false, availability: 'ready' },
        { id: 'pod', kind: 'container', requires_project_directory: false, availability: 'ready' },
      ],
      bundles: [{ id: 'b1', primary_repository: 'r1', repositories: [{ id: 'r1', destination: '/dest' }] }],
      default: { profile_id: 'p1', target_id: 'pod' },
    },
  }));
  server.handle('GET', '/api/v1/projects', () => ({
    json: { projects: [], locations: [], status: { state: 'ready' } },
  }));
  server.handle('POST', '/api/v1/preflight/new', () => ({
    json: {
      managed_worktree: { available: false, default_create: false },
      remote_repairs: [],
      dirty_repositories: [],
      remote_repositories: [],
      local_changes_excluded: false,
    },
  }));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openDashboard(page);
    await page.getByRole('link', { name: 'New session' }).click();
    await expect(page.getByRole('heading', { name: '2 · Project' })).toBeVisible();
    // The default target is the container target, which takes bundles and mounts.
    await expect(page.getByLabel('Target')).toHaveValue('pod');
    const bundles = page.getByRole('listbox', { name: 'Project bundles' });
    await expect(bundles).toHaveClass(/max-h-60/);
    await page.getByLabel('Search bundles').fill('/DEST');
    await expect(bundles.getByRole('option')).toHaveCount(1);
    await page.getByLabel('Search bundles').clear();
    await expect(page.getByRole('button', { name: '+ Create bundle from repository' })).toHaveAttribute('aria-expanded', 'false');
    await expect(page.getByText('Preflight passed.')).toBeVisible();

    await page.getByRole('button', { name: 'Add mount' }).click();
    await page.getByLabel('Mount source 1').fill('/host/data');
    await page.getByLabel('Mount destination 1').fill('/workspace/data');
    await expect(page.getByLabel('Mount access 1')).toHaveValue('ro');

    await expect.poll(() => {
      const request = lastRequest('POST', '/api/v1/preflight/new');
      return request && request.body && Array.isArray(request.body.additional_mounts)
        ? request.body.additional_mounts.length
        : 0;
    }).toBe(1);
    expect(lastRequest('POST', '/api/v1/preflight/new').body.additional_mounts).toEqual([
      { source: '/host/data', destination: '/workspace/data', read_only: true },
    ]);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('resume confirms a raw-checkout conversion and wiki restores an archive', async ({ browser }) => {
  server.setSnapshot(
    snapshot({
      sessions: [
        liveSession('r1', {
          title: 'Suspended one',
          lifecycle: 'suspended',
          active: false,
          chat_phase: 'closed',
          conversation_available: false,
          compatible_resume_targets: ['localhost'],
          capabilities: { open: false, prompt: false, run_shell: false },
          verdict: { resume: 'ready' },
        }),
      ],
    }),
  );
  server.handle('POST', '/api/v1/preflight/resume', () => ({
    json: {
      kind: 'converting-raw-checkout',
      preview: {
        checkout: '/proj/app',
        destination: '/dest/app',
        branch: 'main',
        fetch_url: 'https://example.invalid/app.git',
        push_urls: [],
        default_branch: 'main',
        unpushed_commits: 2,
        staged_files: 0,
        unstaged_files: 1,
        untracked_files: 3,
        untracked_bytes: 1024,
        host_checkout_retained: true,
      },
    },
  }));
  server.handle('GET', '/api/v1/wiki/search', () => ({
    json: {
      rows: [
        {
          id: 'wk1',
          tool: 'claude',
          project: 'app',
          title: 'Archived debugging chat',
          started: null,
          msgs: 12,
          preview: 'how the bug was fixed',
          archived: true,
          native_id: null,
          snippet: null,
          hel_session_id: null,
        },
      ],
      status: { state: 'ready' },
    },
  }));
  server.handle('GET', '/api/v1/wiki/sessions/wk1', () => ({
    json: {
      wiki_id: 'wk1',
      tool: 'claude',
      path: '/archive/wk1',
      status: 'archived',
      profile_id: 'p1',
      target_template_id: 'localhost',
      title: 'Archived debugging chat',
      project: 'app',
    },
  }));
  server.handle('GET', '/api/v1/wiki/sessions/wk1/brief', () => ({
    json: { markdown: '## Brief\n\nThe bug was a stale cache.' },
  }));
  server.handle('POST', '/api/v1/wiki/sessions/wk1/restore', () => ({
    json: { session_id: 'restored-1' },
  }));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openDashboard(page);
    await page.getByRole('link', { name: 'Resume' }).click();
    await expect(page.getByRole('heading', { name: 'Resume and SessionWiki' })).toBeVisible();

    // Suspended tab: pick the session, run the preflight, acknowledge, resume.
    await page.getByRole('button', { name: /Suspended one/ }).click();
    await expect(page.getByText(/converting the checkout|isolated workspace/i).first()).toBeVisible();
    const resumeButton = page.getByRole('button', { name: 'Resume session' });
    await expect(resumeButton).toBeDisabled();
    await page.getByRole('checkbox', { name: /isolated workspace/ }).check();
    await expect(resumeButton).toBeEnabled();
    expect(lastRequest('POST', '/api/v1/preflight/resume').body).toEqual({
      session_id: 'r1',
      target_id: 'localhost',
    });
    await resumeButton.click();
    await expect.poll(() => postedActions(server).filter(body => body.action === 'resume').length).toBe(1);
    expect(postedActions(server).find(body => body.action === 'resume')).toMatchObject({
      action: 'resume',
      session_id: 'r1',
      profile_id: 'p1',
      target_id: 'localhost',
      queue: 'start',
    });

    // A successful resume opens the conversation; wait for it so the app's own
    // navigation cannot land after the next step's.
    await expect(page).toHaveURL(/#conversation\/r1$/);

    // Wiki tab: search, open the hit, read the brief, restore.
    await page.goto(`${server.url}/#workspace/w1/resume`);
    await page.getByRole('tab', { name: 'Session Wiki' }).click();
    await expect(page.getByRole('button', { name: /Archived debugging chat/ })).toBeVisible();
    await page.getByRole('button', { name: /Archived debugging chat/ }).click();
    await expect(page.getByRole('heading', { name: 'Brief', level: 3 })).toBeVisible();
    await expect(page.getByText('The bug was a stale cache.')).toBeVisible();
    await page.getByRole('button', { name: 'Restore session' }).click();
    await expect.poll(() => server.requests('POST', '/api/v1/wiki/sessions/wk1/restore').length).toBe(1);
    expect(lastRequest('POST', '/api/v1/wiki/sessions/wk1/restore').body).toMatchObject({
      workspace_id: 'w1',
      profile_id: 'p1',
      target_id: 'localhost',
    });
    await expect(page).toHaveURL(/#conversation\/restored-1$/);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('move prepares against a destination and confirms with the preparation', async ({ browser }) => {
  server.setSnapshot(
    snapshot({
      meta: { targets: [target('localhost'), target('pod', { kind: 'container' })] },
      sessions: [
        liveSession('s1', {
          title: 'Movable session',
          compatible_resume_targets: ['localhost', 'pod'],
        }),
      ],
    }),
  );
  server.handle('POST', '/api/v1/moves/prepare', ({ entry }) => ({
    json: {
      destination_checks: 'checked',
      route_preview: 'same_host',
      selection: entry.body,
      source_profile_id: 'p1',
      source_target_template_id: 'localhost',
      cross_harness: false,
      active: false,
      queued_commands: [],
      fingerprint: 'fp-1',
      operation_id: 'op-1',
    },
  }));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openDashboard(page);
    await page.getByTestId('session-card-s1').getByRole('button', { name: 'Actions for Movable session', exact: true }).click();
    await page.getByRole('menuitem', { name: 'Move…' }).click();
    await expect(page.getByRole('heading', { name: 'Move session' }).first()).toBeVisible();

    await page.getByLabel('Target').selectOption('pod');
    await page.getByRole('button', { name: 'Prepare move' }).click();
    await expect(page.getByText('Checked before the move')).toBeVisible();
    await expect(page.getByText('Same host — no network transfer')).toBeVisible();
    const prepare = lastRequest('POST', '/api/v1/moves/prepare').body;
    expect(prepare).toMatchObject({
      session_id: 's1',
      profile_id: 'p1',
      target_template_id: 'pod',
      subagents: { mode: 'native' },
    });

    await page.getByRole('button', { name: 'Confirm move' }).click();
    await expect.poll(() => postedActions(server).filter(body => body.action === 'move').length).toBe(1);
    const move = postedActions(server).find(body => body.action === 'move');
    expect(move.request.preparation.fingerprint).toBe('fp-1');
    expect(move.request.acknowledge_interruption).toBe(false);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('targets and quota render readings and post their refresh actions', async ({ browser }) => {
  const quota = {
    summary: 'fake subscription quota',
    stale: false,
    refreshed_at_epoch_seconds: 1_764_000_000,
    has_error: false,
    windows: [
      {
        label: 'Weekly window',
        reset_countdown_style: 'long',
        percent_used: 55,
        resets_at_epoch_seconds: 1_764_000_000 + 7200,
        projects_exhaustion_before_reset: false,
      },
    ],
  };
  server.setSnapshot(
    snapshot({
      meta: {
        profiles: [profile('p1', { quota })],
        targets: [
          target('localhost'),
          target('broken', { availability: { status: 'missing', reason: 'podman is not installed' } }),
        ],
        capacity: [
          {
            id: 'cap1',
            label: 'Dev host',
            target_ids: ['localhost'],
            cpu_percent: 42,
            refreshing: false,
            stale: false,
            has_error: false,
          },
        ],
      },
      sessions: [liveSession('s1', { title: 'Existing' })],
    }),
  );
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openDashboard(page);
    await page.goto(`${server.url}/#targets`);
    await expect(page.getByRole('heading', { name: 'Targets' }).first()).toBeVisible();
    await expect(page.getByText('Dev host')).toBeVisible();
    await expect(page.getByText('42%')).toBeVisible();
    await expect(page.getByText('podman is not installed')).toBeVisible();
    await page.getByRole('button', { name: 'Check availability' }).click();
    await expect.poll(() =>
      postedActions(server).filter(body => body.action === 'check-availability').length,
    ).toBe(1);
    await page.getByRole('button', { name: 'Refresh capacity for localhost' }).click();
    await expect.poll(() =>
      postedActions(server).filter(body => body.action === 'refresh-capacity').length,
    ).toBe(1);
    expect(postedActions(server).find(body => body.action === 'refresh-capacity')).toEqual({
      action: 'refresh-capacity',
      target_id: 'localhost',
    });

    await page.goto(`${server.url}/#quota`);
    await expect(page.getByRole('heading', { name: 'Quota' }).first()).toBeVisible();
    await expect(page.getByText('fake subscription quota')).toBeVisible();
    await expect(page.getByText('55% used')).toBeVisible();
    await expect(page.getByRole('progressbar', { name: 'Weekly window usage' })).toHaveAttribute(
      'aria-valuenow',
      '55',
    );
    await page.getByRole('button', { name: 'Refresh quota for p1' }).click();
    await expect.poll(() =>
      postedActions(server).filter(body => body.action === 'refresh-quota').length,
    ).toBe(1);
    expect(postedActions(server).find(body => body.action === 'refresh-quota')).toEqual({
      action: 'refresh-quota',
      profile_id: 'p1',
    });
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('settings edits save and the git identity card round-trips', async ({ browser }) => {
  const settingsPage = checked => ({
    path: [],
    breadcrumb: ['Settings'],
    help: '',
    groups: [
      {
        heading: 'General',
        rows: [
          { path: ['tips'], label: 'Show tips', control: 'checkbox', checked },
          { path: ['model'], label: 'A very long model selection label that wraps on a narrow screen', control: 'choice', value: 'very-long-model-name-without-breaks-123456789012345678901234567890', choices: ['very-long-model-name-without-breaks-123456789012345678901234567890', 'b', 'c', 'd'].map(value => ({ value, label: value })) },
          { path: ['instruction'], label: 'A very long text setting label that also wraps on a narrow screen', control: 'text', value: '', placeholder: '', number: false },
        ],
      },
    ],
    removable: false,
  });
  server.setSnapshot(snapshot({ sessions: [liveSession('s1', { title: 'Existing' })] }));
  server.handle('GET', '/api/v1/settings', () => ({
    json: { original: {}, draft: {}, page: settingsPage(true) },
  }));
  server.handle('POST', '/api/v1/settings/page', () => ({ json: { page: settingsPage(true) } }));
  server.handle('POST', '/api/v1/settings/edit', () => ({
    json: { draft: { tips: false }, page: settingsPage(false) },
  }));
  server.handle('POST', '/api/v1/settings/save', () => ({
    json: { original: { tips: false }, draft: { tips: false }, page: settingsPage(false) },
  }));
  server.handle('GET', '/api/v1/git-identity', () => ({
    json: { name: 'Test User', email: 'test@example.com', per_directory: false },
  }));
  server.handle('POST', '/api/v1/git-identity', ({ entry }) => ({
    json: { name: entry.body.name, email: entry.body.email, per_directory: false },
  }));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openDashboard(page);
    await page.goto(`${server.url}/#settings`);
    const toggle = page.getByRole('switch', { name: 'Show tips' });
    await expect(toggle).toBeVisible();
    await expect(page.getByRole('combobox', { name: /A very long model selection label/ })).toBeVisible();
    await expect(page.getByRole('textbox', { name: /A very long text setting label/ })).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
    await toggle.click();
    await expect.poll(() => server.requests('POST', '/api/v1/settings/edit').length).toBe(1);
    expect(lastRequest('POST', '/api/v1/settings/edit').body.edit).toEqual({
      kind: 'toggle',
      path: ['tips'],
    });

    // The edit left the document dirty; saving clears the footer.
    await expect(page.getByText('Unsaved changes')).toBeVisible();
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByRole('main').getByText('Settings saved.')).toBeVisible();
    expect(lastRequest('POST', '/api/v1/settings/save').body).toMatchObject({
      original: {},
      draft: { tips: false },
    });

    // Git identity: load, edit, save.
    await expect(page.getByLabel('Name')).toHaveValue('Test User');
    await page.getByLabel('Name').fill('New Name');
    await page.getByLabel('Email').fill('new@example.com');
    await page.getByRole('button', { name: 'Save Git identity' }).click();
    await expect(page.getByText('Git identity saved.')).toBeVisible();
    expect(lastRequest('POST', '/api/v1/git-identity').body).toEqual({
      name: 'New Name',
      email: 'new@example.com',
    });
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('suspend and destroy run through their confirmation dialogs', async ({ browser }) => {
  server.setSnapshot(snapshot({ sessions: [liveSession('s1', { title: 'Lifecycle session' })] }));
  server.handle('GET', '/api/v1/sessions/s1/suspend-preflight', () => ({
    json: {
      session_id: 's1',
      untracked: [
        {
          checkout: { repository_id: 'r1', checkout_path: '/proj/app' },
          path: 'tmp/output.txt',
          bytes: 42,
          identity: 'ident-1',
        },
      ],
      nested_repositories: [],
      untracked_stashes: [],
      keep_dir_entries: [],
      keep_dir_bytes: 0,
      operations: [],
      raw: false,
      interrupting: false,
      children_not_handed_back: [],
      blockers: [],
    },
  }));
  server.handle('POST', '/api/v1/sessions/s1/destroy', () => ({}));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openDashboard(page);
    await page.getByTestId('session-card-s1').getByRole('button', { name: 'Lifecycle session', exact: true }).click();
    await expect(page.getByTestId('conversation-view')).toBeVisible();

    // Suspend: the preflight lists the untracked file and gates on the acknowledgement.
    await page.getByRole('button', { name: 'More actions' }).click();
    await page.getByRole('menuitem', { name: 'Suspend', exact: true }).click();
    const suspendDialog = page.getByRole('dialog');
    await expect(suspendDialog).toContainText('1 untracked item will not be kept');
    await expect(suspendDialog).toContainText('tmp/output.txt');
    const suspendButton = suspendDialog.getByRole('button', { name: 'Suspend session' });
    await expect(suspendButton).toBeDisabled();
    await suspendDialog.getByRole('checkbox', { name: /will be discarded/ }).check();
    await expect(suspendButton).toBeEnabled();
    await suspendButton.click();
    await expect.poll(() => postedActions(server).filter(body => body.action === 'suspend').length).toBe(1);
    expect(postedActions(server).find(body => body.action === 'suspend')).toEqual({
      action: 'suspend',
      session_id: 's1',
      discard_untracked: {
        Shown: [
          {
            Path: {
              checkout: { repository_id: 'r1', checkout_path: '/proj/app' },
              path: 'tmp/output.txt',
              identity: 'ident-1',
            },
          },
        ],
      },
    });
    await expect(suspendDialog).toBeHidden();

    // Destroy: its own POST route, after the explicit confirmation.
    await page.getByRole('button', { name: 'More actions' }).click();
    await page.getByRole('menuitem', { name: 'Destroy' }).click();
    const destroyDialog = page.getByRole('dialog');
    await expect(destroyDialog).toContainText('Permanently remove this session');
    await destroyDialog.getByRole('button', { name: 'Destroy session' }).click();
    await expect.poll(() => server.requests('POST', '/api/v1/sessions/s1/destroy').length).toBe(1);
    await expect(page.getByRole('heading', { name: 'Workspaces' })).toBeVisible();
    await guards.assertClean();
  } finally {
    await context.close();
  }
});
