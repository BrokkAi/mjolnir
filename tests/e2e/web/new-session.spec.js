const { test, expect } = require('@playwright/test');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, serviceWorkers: 'block' });

async function mount(page, { bundles = [{ id: 'existing', repositories: [] }] } = {}) {
  const state = {
    snapshot: {
      revision: 1, workspaces: [{ id: 'test', name: 'Test' }, { id: 'other', name: 'Other' }], sessions: [],
      subagents_enabled: true,
      profiles: [{ id: 'alpha', harness_kind: 'codex' }, { id: 'beta', harness_kind: 'claude' }, { id: 'gamma', harness_kind: 'grok' }],
      targets: [
        { id: 'container', kind: 'podman', requires_project_directory: false, recent_project_directories: [] },
        { id: 'local', kind: 'local', requires_project_directory: true, recent_project_directories: ['/work/recent', '/work/older'] },
        { id: 'remote', kind: 'ssh', requires_project_directory: true, recent_project_directories: ['/remote/recent'] },
      ], bundles, capacity: [], launch_failures: [],
    },
    snapshots: 0, preflights: [], preflightFailures: 0, actions: [], creates: [], rejectCreate: false,
    completions: [], discoveries: [], discoveryFailures: 0,
    discover: body => body.kind === 'github' ? {
      entries: [{ name: 'example/app', source: 'https://github.com/example/app', description: 'A useful project', kind: 'repository' }],
      directory: null, parent: null, truncated: false,
    } : {
      entries: body.path === '/home/controller/code' ? [
        { name: 'Use code', source: '/home/controller/code', description: 'Repository in this folder', kind: 'repository' },
      ] : [
        { name: 'code', source: '/home/controller/code', description: 'Folder', kind: 'directory' },
      ], directory: body.path || '/home/controller', parent: body.path ? '/home/controller' : '/', truncated: false,
    },
    complete: () => ({ candidates: ['/work/recent/', '/work/repos/'], insert: '/work/re', truncated: false }),
    holdCreate: null, holdLaunch: null,
    holdPreflight: null, preflightError: null, remoteRepairs: [], resolvedDirectory: null,
    worktreeOptions: { available: true, default_create: true },
  };
  const webRoot = path.resolve(__dirname, '../../../mj-controller/src/web');
  await page.addInitScript(() => {
    window.EventSource = class extends EventTarget {
      constructor() {
        super();
        window.fixtureEvents = this;
        queueMicrotask(() => this.dispatchEvent(new Event('open')));
      }
      close() {}
    };
  });
  page.on('requestfailed', request => {
    if (request.url().endsWith('/api/preflight/new')) state.preflightFailures++;
    if (request.url().endsWith('/api/projects/discover')) state.discoveryFailures++;
  });
  await page.route('**/*', async route => {
    const pathname = new URL(route.request().url()).pathname;
    const json = value => route.fulfill({ contentType: 'application/json', body: JSON.stringify(value) });
    if (pathname === '/api/snapshot') { state.snapshots++; return json(state.snapshot); }
    if (pathname === '/api/events') return route.fulfill({ contentType: 'text/event-stream', body: ': fixture\n\n' });
    if (pathname === '/api/preflight/new') {
      const request = route.request().postDataJSON();
      state.preflights.push(request);
      const bare = state.snapshot.targets.find(target => target.id === request.target_id)?.requires_project_directory === true;
      if (state.holdPreflight) await state.holdPreflight;
      if (state.preflightError) return route.fulfill({ status: 400, contentType: 'application/json', body: JSON.stringify({ error: state.preflightError }) });
      return json({
        remote_repairs: state.remoteRepairs,
        project_directory: bare ? state.resolvedDirectory : null,
        remote_repositories: bare ? [] : [{ id: request.bundle_id, fetch_url: 'https://github.com/example/repo.git', default_branch: 'main', push_urls: ['https://github.com/example/repo.git'] }],
        local_changes_excluded: !bare,
        managed_worktree: bare ? state.worktreeOptions : { available: false, default_create: false },
      });
    }
    if (pathname === '/api/paths/complete') {
      const body = route.request().postDataJSON();
      state.completions.push(body);
      return json(await state.complete(body));
    }
    if (pathname === '/api/projects/discover') {
      const body = route.request().postDataJSON();
      state.discoveries.push(body);
      const result = await state.discover(body);
      if (result.error) return route.fulfill({ status: 400, contentType: 'application/json', body: JSON.stringify(result) });
      return json(result);
    }
    if (pathname === '/api/bundles') {
      state.creates.push(route.request().postDataJSON());
      if (state.holdCreate) await state.holdCreate;
      if (state.rejectCreate) return route.fulfill({ status: 400, contentType: 'application/json', body: JSON.stringify({ error: 'Repository source is invalid' }) });
      state.snapshot.bundles.push({ id: 'created', repositories: [] });
      return json({ bundle_id: 'created' });
    }
    if (pathname === '/api/actions') {
      state.actions.push(route.request().postDataJSON());
      if (state.holdLaunch) await state.holdLaunch;
      return route.fulfill({ status: 202, body: '' });
    }
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    if (['viewer.html', 'viewer.js', 'viewer.css', 'markdown.js', 'tool-output.js', 'manifest.webmanifest', 'icon.svg'].includes(file))
      return route.fulfill({ path: path.join(webRoot, file === 'icon.svg' ? '../icons/icon.svg' : file) });
    return route.fulfill({ status: 404, body: '' });
  });
  await page.goto('https://viewer.test/#workspace/test/new');
  await expect(page.locator('#new-page')).toBeVisible();
  return state;
}

async function refresh(page, state) {
  const previous = state.snapshots;
  state.snapshot.revision++;
  await page.evaluate(() => window.fixtureEvents.dispatchEvent(new Event('revision')));
  await expect.poll(() => state.snapshots).toBeGreaterThan(previous);
  // Wait for the render associated with the completed response, not just its request.
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
}

async function projectStep(page, target = 'container') {
  await page.locator('#new-next').click();
  await page.locator('#new-target').getByRole('radio', { name: new RegExp(`^${target}`) }).check();
  await page.locator('#new-next').click();
}

test('whole-row taps and in-progress gestures survive unrelated live updates', async ({ page }) => {
  const state = await mount(page);
  const beta = page.locator('#new-profile').getByRole('radio', { name: /^beta/ });
  const row = beta.locator('..');
  const original = await beta.elementHandle();
  const box = await row.boundingBox();
  expect(box.height).toBeGreaterThanOrEqual(44);
  await page.mouse.move(box.x + box.width - 8, box.y + box.height / 2);
  await page.mouse.down();
  state.snapshot.profiles[0].quota = { summary: 'new live reading' };
  await refresh(page, state);
  expect(await original.evaluate(node => node.isConnected)).toBe(true);
  await page.mouse.up();
  await expect(beta).toBeChecked();
  await refresh(page, state);
  await expect(beta).toBeChecked();
  await projectStep(page, 'local');
  const directory = page.locator('#new-project-directory');
  await directory.fill('/work/typed');
  await refresh(page, state);
  await expect(directory).toBeFocused();
  await expect(directory).toHaveValue('/work/typed');
});

test('raw projects use host-specific recents and preserve edited paths across Back', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page, 'local');
  const directory = page.locator('#new-project-directory');
  await expect(directory).toHaveValue('/work/recent');
  await page.getByRole('button', { name: '/work/older', exact: true }).tap();
  await expect(directory).toHaveValue('/work/older');
  await directory.fill('/work/custom');
  await page.locator('#new-back').click();
  await page.locator('#new-target').getByRole('radio', { name: /^remote/ }).check();
  await page.locator('#new-next').click();
  await expect(directory).toHaveValue('/remote/recent');
  await expect(page.getByRole('button', { name: '/work/older', exact: true })).toHaveCount(0);
  await page.locator('#new-back').click();
  await page.locator('#new-target').getByRole('radio', { name: /^local/ }).check();
  await page.locator('#new-next').click();
  await expect(directory).toHaveValue('/work/custom');
  await directory.fill('');
  await page.locator('#new-next').click();
  await expect(page.locator('#new-error')).toContainText('Name the project directory');
  expect(state.preflights).toHaveLength(0);
  await directory.fill('/work/custom');
  await page.locator('#new-next').click();
  await expect(page.locator('#new-step')).toContainText('/work/custom');
  expect(state.preflights[0]).toMatchObject({ target_id: 'local', project_directory: '/work/custom' });
});

// Suggestions answer the machine that owns the path and never rewrite what
// is being typed; pasting a URL never searches a filesystem.
test('a project directory suggests paths on its own host and a URL never searches folders', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page, 'local');
  const directory = page.locator('#new-project-directory');
  await directory.fill('/work/re');
  await expect.poll(() => state.completions.length).toBe(1);
  expect(state.completions[0]).toEqual({ target_id: 'local', prefix: '/work/re', kind: 'directories' });
  const rows = page.locator('.field-suggestions .palette-row[role="option"]');
  await expect(rows).toHaveCount(2);
  await expect(rows.first()).toBeVisible();
  await expect(directory).toHaveValue('/work/re');

  await directory.press('ArrowDown');
  await directory.press('Enter');
  await expect(directory).toHaveValue('/work/repos/');
  // Accepting a directory asks for its children, on the same host.
  await expect.poll(() => state.completions.length).toBe(2);
  expect(state.completions[1]).toEqual({ target_id: 'local', prefix: '/work/repos/', kind: 'directories' });

  await page.locator('#new-back').click();
  await page.locator('#new-target').getByRole('radio', { name: /^container/ }).check();
  await page.locator('#new-next').click();
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await page.locator('#new-project-source').fill('owner/repo');
  await page.waitForTimeout(400);
  expect(state.completions).toHaveLength(2);
});

test('empty projects show choices and a pasted source retries then continues directly to review', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await expect(page.locator('#new-next')).toBeHidden();
  await expect(page.locator('#new-title')).toHaveCount(0);
  await expect(page.locator('#new-project-source')).toHaveCount(0);
  await expect(page.locator('#new-step')).not.toContainText(/bundle/i);
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await page.locator('#new-project-source').fill('example/created');
  state.rejectCreate = true;
  await page.getByRole('button', { name: 'Use repository', exact: true }).click();
  await expect(page.locator('#new-error')).toContainText('Repository source is invalid');
  await expect(page.locator('#new-project-source')).toHaveValue('example/created');
  state.rejectCreate = false;
  await page.locator('#new-project-source').press('Enter');
  await expect(page.locator('#new-progress')).toContainText('Review');
  await expect(page.locator('#new-step')).toContainText('Project');
  await expect(page.locator('#new-step')).not.toContainText(/bundle/i);
  expect(state.creates).toEqual([{ sources: ['example/created'] }, { sources: ['example/created'] }]);
  await expect(page.locator('#new-step')).toContainText('Local changes');
  await page.locator('#new-next').click();
  await expect(page).toHaveURL(/#workspace\/test$/);
  expect(state.actions).toHaveLength(1);
  expect(state.actions[0]).toMatchObject({ action: 'new', workspace_id: 'test', bundle_id: 'created' });
  expect(state.actions[0]).not.toHaveProperty('dirty_ack');
});

test('late launch completion cannot replace another workspace wizard', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page);
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  let release;
  state.holdLaunch = new Promise(resolve => { release = resolve; });
  await page.locator('#new-next').click();
  await expect.poll(() => state.actions.length).toBe(1);
  await expect(page.locator('#new-next')).toBeDisabled();
  await page.evaluate(() => { location.hash = '#workspace/other/new'; });
  await expect(page.locator('#new-profile')).toBeVisible();
  release();
  await refresh(page, state);
  await expect(page).toHaveURL(/#workspace\/other\/new$/);
  await expect(page.locator('#new-profile')).toBeVisible();
  expect(state.actions[0].workspace_id).toBe('test');
});

test('leaving a new wizard aborts its stale preflight request', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page);
  let release;
  state.holdPreflight = new Promise(resolve => { release = resolve; });
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect.poll(() => state.preflights.length).toBe(1);
  await expect(page.locator('#new-next')).toHaveText('Checking…');
  await page.evaluate(() => { location.hash = '#workspace/other/new'; });
  await expect(page.locator('#new-profile')).toBeVisible();
  release();
  await expect.poll(() => state.preflightFailures).toBeGreaterThan(0);
  await expect(page.locator('#new-step')).toContainText('Account');
  await expect(page.locator('#new-step')).not.toContainText('Local changes');
});

test('Review is usable during preflight, survives refresh, and gates submission', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page, 'local');
  let release;
  state.holdPreflight = new Promise(resolve => { release = resolve; });
  state.resolvedDirectory = '/resolved/project';
  await page.locator('#new-next').click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  await expect(page.locator('#new-step')).toContainText('Checking project…');
  await expect(page.locator('#new-next')).toBeDisabled();
  await expect(page.locator('#new-back')).toBeEnabled();
  const worktree = page.getByRole('checkbox', { name: 'Create isolated checkout' });
  await expect(worktree).toBeDisabled();
  const subagents = page.getByRole('checkbox', { name: 'Use Mjolnir sub-agents' });
  await subagents.uncheck();
  await page.locator('#new-form').evaluate(form => form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })));
  expect(state.actions).toHaveLength(0);
  await refresh(page, state);
  await expect(subagents).not.toBeChecked();
  await expect(page.locator('#new-step')).toContainText('Checking project…');
  expect(state.preflights).toHaveLength(1);
  release();
  await expect(page.locator('#new-next')).toHaveText('Start');
  await expect(page.locator('#new-next')).toBeEnabled();
  await expect(worktree).toBeChecked();
  await expect(page.locator('#new-step')).toContainText('/resolved/project');
  await page.locator('#new-next').click();
  expect(state.actions[0]).toMatchObject({ project_directory: '/resolved/project', mjolnir_subagents: false });
});

test('failed Review retries without launching and Back abandons a pending check', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page);
  state.preflightError = 'Remote unavailable';
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  await expect(page.locator('#new-step')).toContainText('Remote unavailable');
  await expect(page.locator('#new-next')).toHaveText('Retry');
  state.preflightError = null;
  let release;
  state.holdPreflight = new Promise(resolve => { release = resolve; });
  await page.locator('#new-next').click();
  await expect.poll(() => state.preflights.length).toBe(2);
  expect(state.actions).toHaveLength(0);
  await page.locator('#new-back').click();
  await expect(page.locator('#new-progress')).toContainText('Project');
  state.holdPreflight = null;
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(page.locator('#new-next')).toHaveText('Start');
  await expect.poll(() => state.preflights.length).toBe(3);
  release();
  await expect.poll(() => state.preflightFailures).toBeGreaterThan(0);
  await expect(page.locator('#new-progress')).toContainText('Review');
  await expect(page.locator('#new-next')).toBeEnabled();
  expect(state.actions).toHaveLength(0);
});

test('declining repair keeps Review unready and allows a fresh retry', async ({ page }) => {
  const state = await mount(page);
  state.remoteRepairs = [{ path: '/project', branch: 'main', missing_remote: 'old', replacement_remote: 'origin', fetch_url: 'https://example.com/project.git', push_urls: [] }];
  page.on('dialog', dialog => dialog.dismiss());
  await projectStep(page);
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(page.locator('#new-step')).toContainText('repair was declined');
  await expect(page.locator('#new-next')).toHaveText('Retry');
  expect(state.preflights).toHaveLength(1);
  expect(state.actions).toHaveLength(0);
  state.remoteRepairs = [];
  await page.locator('#new-next').click();
  await expect(page.locator('#new-next')).toHaveText('Start');
  expect(state.preflights).toHaveLength(2);
  expect(state.actions).toHaveLength(0);
});

test('a pending client does not prevent another client from completing preflight', async ({ page, context }) => {
  const first = await mount(page);
  await projectStep(page);
  let release;
  first.holdPreflight = new Promise(resolve => { release = resolve; });
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(page.locator('#new-next')).toBeDisabled();
  const other = await context.newPage();
  const second = await mount(other);
  await projectStep(other);
  await other.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(other.locator('#new-next')).toHaveText('Start');
  await expect(other.locator('#new-next')).toBeEnabled();
  expect(second.preflights).toHaveLength(1);
  await expect(page.locator('#new-next')).toBeDisabled();
  release();
  await expect(page.locator('#new-next')).toBeEnabled();
});

test('project creation stays single-flight and a late result cannot alter a replacement wizard', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  let release;
  state.holdCreate = new Promise(resolve => { release = resolve; });
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await page.locator('#new-project-source').fill('example/created');
  await page.getByRole('button', { name: 'Use repository', exact: true }).click();
  await expect.poll(() => state.creates.length).toBe(1);
  await expect(page.getByRole('button', { name: 'Opening project…', exact: true })).toBeDisabled();
  await expect(page.locator('#new-next')).toBeDisabled();
  await page.evaluate(() => { location.hash = '#workspace/other/new'; });
  await expect(page.locator('#new-profile')).toBeVisible();
  release();
  await refresh(page, state);
  await expect(page).toHaveURL(/#workspace\/other\/new$/);
  await expect(page.locator('#new-profile')).toBeVisible();
  expect(state.creates).toHaveLength(1);
});

test('resume choices stay selected through revisions and are used by Resume', async ({ page }) => {
  const state = await mount(page);
  state.snapshot.sessions.push({
    id: 'suspended', title: 'Suspended test', state: 'suspended', lifecycle: 'suspended',
    workspace_id: 'test', profile_id: 'alpha', target_id: 'local',
    capabilities: { resume: true }, compatible_resume_targets: ['local', 'remote'],
    queued_prompts: [{ id: 'queued', text: 'queued work' }],
  });
  await refresh(page, state);
  await page.evaluate(() => { location.hash = '#workspace/test/resume'; });
  await expect(page.locator('#resume-list-view')).toBeVisible();
  await expect(page.locator('#resume-detail-view')).toBeHidden();
  await page.locator('#resumable [data-session-id="suspended"]').click();
  await expect(page).toHaveURL(/\/resume\/suspended$/);
  const detail = page.locator('#resume-detail');
  await detail.locator('[data-role="resume-profile"] select').selectOption('beta');
  await detail.locator('[data-role="resume-target"] select').selectOption('remote');
  await detail.locator('[data-role="resume-queue"] select').selectOption('discard');
  await refresh(page, state);
  await expect(detail.locator('[data-role="resume-profile"] select')).toHaveValue('beta');
  await expect(detail.locator('[data-role="resume-queue"] select')).toHaveValue('discard');
  await detail.getByRole('button', { name: 'Resume', exact: true }).click();
  await expect.poll(() => state.actions.length).toBe(1);
  await expect(page).toHaveURL(/#workspace\/test$/);
  expect(state.actions[0]).toEqual({ action: 'resume', session_id: 'suspended', workspace_id: 'test', profile_id: 'beta', target_id: 'remote', queue: 'discard' });
});


test('managed worktree defaults can be overridden and survive Back and live refresh', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page, 'local');
  await page.locator('#new-next').click();
  const checkbox = page.getByRole('checkbox', { name: 'Create isolated checkout' });
  await expect(checkbox).toBeChecked();
  await checkbox.uncheck();
  await expect(page.locator("#new-step")).toContainText("Use the selected directory directly.");
  await refresh(page, state);
  await expect(checkbox).not.toBeChecked();
  await page.locator('#new-back').click();
  await page.locator('#new-next').click();
  await expect(checkbox).not.toBeChecked();
  await expect(page.locator('#new-step')).toContainText('Use the selected directory directly.');
  await page.locator('#new-next').click();
  expect(state.actions.at(-1)).toMatchObject({ create_managed_worktree: false, project_directory: '/work/recent' });
});

test('an existing linked checkout can explicitly create a managed worktree', async ({ page }) => {
  const state = await mount(page);
  state.worktreeOptions = { available: true, default_create: false };
  await projectStep(page, 'local');
  await page.locator('#new-project-directory').fill('/work/linked');
  await page.locator('#new-next').click();
  const checkbox = page.getByRole('checkbox', { name: 'Create isolated checkout' });
  await expect(checkbox).not.toBeChecked();
  await expect(checkbox).toBeEnabled();
  await checkbox.focus();
  await page.keyboard.press('Space');
  await expect(checkbox).toBeChecked();
  await expect(checkbox).toBeFocused();
  await page.locator('#new-next').click();
  expect(state.actions.at(-1)).toMatchObject({ create_managed_worktree: true, project_directory: '/work/linked' });
});

test('the sub-agent checkbox appears only for Claude and Codex and sends its choice', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page, 'container');
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  const checkbox = page.getByRole('checkbox', { name: 'Use Mjolnir sub-agents' });
  await expect(checkbox).toBeChecked();
  await checkbox.uncheck();
  await expect(page.locator('#new-step')).toContainText('keeps the harness');
  await refresh(page, state);
  await expect(checkbox).not.toBeChecked();
  await page.locator('#new-next').click();
  expect(state.actions.at(-1)).toMatchObject({ mjolnir_subagents: false });
});

test('a harness that cannot receive Mjolnir sub-agents shows no checkbox and sends no choice', async ({ page }) => {
  const state = await mount(page);
  await page.locator('#new-profile').getByRole('radio', { name: /^gamma/ }).check();
  await projectStep(page, 'container');
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(page.getByRole('checkbox', { name: 'Use Mjolnir sub-agents' })).toHaveCount(0);
  await page.locator('#new-next').click();
  expect(state.actions.at(-1).mjolnir_subagents).toBe(null);
});

test('changing the directory resets the worktree choice to its inspected default', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page, 'local');
  await page.locator('#new-next').click();
  const checkbox = page.getByRole('checkbox', { name: 'Create isolated checkout' });
  await checkbox.uncheck();
  await page.locator('#new-back').click();
  await page.locator('#new-project-directory').fill('/work/another');
  await page.locator('#new-next').click();
  await expect(checkbox).toBeChecked();
});

for (const target of ['container', 'local', 'remote']) {
  test(`unsupported worktree creation stays disabled for ${target}`, async ({ page }) => {
    const state = await mount(page);
    state.worktreeOptions = { available: false, default_create: false };
    await projectStep(page, target);
    if (target === 'container') await page.getByRole('button', { name: 'existing', exact: true }).click();
    else await page.locator('#new-next').click();
    const checkbox = page.getByRole('checkbox', { name: 'Create isolated checkout' });
    await expect(checkbox).not.toBeChecked();
    await expect(checkbox).toBeDisabled();
    await refresh(page, state);
    await expect(checkbox).toBeDisabled();
  });
}

test('saved multi-repository projects and recent projects open without creating a new configuration', async ({ page }) => {
  const state = await mount(page, { bundles: [
    { id: 'existing', repositories: [{ id: 'frontend', github: 'example/frontend' }, { id: 'api', github: 'example/api' }] },
    { id: 'saved', repositories: [] },
  ] });
  state.snapshot.sessions.push({ id: 'recent', workspace_id: 'test', bundle_id: 'existing', capabilities: {} });
  await refresh(page, state);
  await expect(page.locator('#new-progress')).toContainText('Account');
  await projectStep(page);
  await expect(page.getByRole('heading', { name: 'Recent projects' })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Saved projects' })).toBeVisible();
  await page.getByRole('button', { name: 'existing example/frontend · example/api', exact: true }).tap();
  await expect(page.locator('#new-progress')).toContainText('Review');
  await expect(page.locator('#new-step')).toContainText('Where to run');
  expect(state.creates).toHaveLength(0);
  expect(state.preflights[0].bundle_id).toBe('existing');
});

test('GitHub lists accessible repositories, preserves focus across snapshots and searches with the keyboard', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await page.getByRole('button', { name: /^GitHub/ }).tap();
  await expect(page.getByRole('button', { name: /^example\/app/ })).toBeVisible();
  expect(state.discoveries).toEqual([{ kind: 'github', query: '' }]);
  const search = page.getByRole('textbox', { name: 'Search GitHub repositories' });
  await search.fill('example/api');
  await expect.poll(() => state.discoveries.length).toBe(2);
  expect(state.discoveries[1]).toEqual({ kind: 'github', query: 'example/api' });
  await expect(search).toBeFocused();
  await expect(page.getByRole('button', { name: /^example\/app/ })).toBeVisible();
  const row = page.getByRole('button', { name: /^example\/app/ });
  const original = await row.elementHandle();
  state.snapshot.bundles.push({ id: 'another-clients-project', repositories: [] });
  await refresh(page, state);
  await expect(search).toBeFocused();
  expect(await original.evaluate(node => node.isConnected)).toBe(true);
  await page.getByRole('button', { name: 'Clear search', exact: true }).click();
  await expect(search).toHaveValue('');
  await expect(search).toBeFocused();
  await expect.poll(() => state.discoveries.length).toBe(3);
  await row.focus();
  await page.keyboard.press('Enter');
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.creates).toEqual([{ sources: ['https://github.com/example/app'] }]);
  expect(state.preflights[0].bundle_id).toBe('created');
});

test('GitHub authentication errors keep their sign-in instructions and folders remain usable', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  const discover = state.discover;
  state.discover = body => body.kind === 'github' ? { error: 'GitHub authentication required. Sign in on the computer running Mjolnir, then retry.' } : discover(body);
  await projectStep(page);
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await expect(page.locator('.project-browser').getByRole('alert')).toContainText('GitHub authentication required');
  await expect(page.locator('.project-browser')).toContainText('Sign in on the computer running Mjolnir');
  await page.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect.poll(() => state.discoveries.length).toBe(2);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).tap();
  await expect(page.locator('.project-browser')).toContainText('Folders on the computer running Mjolnir.');
  await expect(page.getByRole('button', { name: /^code Folder/ })).toBeVisible();
  expect(state.discoveries.at(-1)).toEqual({ kind: 'directory', path: '' });
  await page.getByRole('button', { name: /^code Folder/ }).tap();
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
  await page.getByRole('button', { name: 'Up', exact: true }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/home/controller');
  await page.getByRole('button', { name: /^code Folder/ }).click();
  await page.getByRole('button', { name: /^Use code/ }).click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.creates).toEqual([{ sources: ['/home/controller/code'] }]);
  expect(state.completions).toHaveLength(0);
});

test('empty and truncated GitHub results explain next steps without preventing URL entry', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  state.discover = () => ({ entries: [], directory: null, parent: null, truncated: true });
  await projectStep(page);
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await expect(page.locator('.project-browser')).toContainText('No repositories found');
  await expect(page.locator('.project-browser')).not.toContainText(/Sign in|gh auth login/);
  await expect(page.locator('.project-browser')).toContainText('Narrow your search');
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await expect(page.getByRole('textbox', { name: 'GitHub URL or owner/repository' })).toBeFocused();
});

test('superseded discovery and cancelled wizard requests cannot insert old results', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  const discover = state.discover;
  let release;
  state.discover = body => body.kind === 'github' ? new Promise(resolve => {
    release = () => resolve(discover(body));
  }) : discover(body);
  await projectStep(page);
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await expect(page.locator('.project-browser').getByRole('status')).toContainText('Loading repositories');
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await expect(page.getByRole('button', { name: /^code Folder/ })).toBeVisible();
  release();
  await expect.poll(() => state.discoveryFailures).toBe(1);
  await expect(page.getByRole('button', { name: /^example\/app/ })).toHaveCount(0);
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await expect(page.locator('.project-browser').getByRole('status')).toContainText('Loading repositories');
  await page.evaluate(() => { location.hash = '#workspace/other/new'; });
  await expect(page.locator('#new-profile')).toBeVisible();
  release();
  await expect.poll(() => state.discoveryFailures).toBe(2);
  await projectStep(page);
  await expect(page.getByRole('heading', { name: 'Choose a project' })).toBeVisible();
  await expect(page.locator('.project-browser')).toHaveCount(0);
});

test('remote folder browsing stays on the selected host and opens its current folder', async ({ page }) => {
  const state = await mount(page);
  state.complete = body => ({ candidates: [`${body.prefix}child/`], truncated: false });
  await projectStep(page, 'remote');
  await page.getByRole('button', { name: 'Browse folders', exact: true }).click();
  await expect(page.locator('.project-browser')).toContainText('Browsing folders on remote');
  await page.getByRole('button', { name: /^\/remote\/recent\/child\// }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/remote/recent/child/');
  await page.getByRole('button', { name: 'Up', exact: true }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/remote/recent/');
  await page.getByRole('button', { name: 'Home', exact: true }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('~/');
  await page.getByRole('button', { name: 'Use this folder', exact: true }).click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.completions).toEqual([
    { target_id: 'remote', prefix: '/remote/recent/', kind: 'directories' },
    { target_id: 'remote', prefix: '/remote/recent/child/', kind: 'directories' },
    { target_id: 'remote', prefix: '/remote/recent/', kind: 'directories' },
    { target_id: 'remote', prefix: '~/', kind: 'directories' },
  ]);
  expect(state.discoveries).toHaveLength(0);
  expect(state.creates).toHaveLength(0);
  expect(state.preflights[0]).toMatchObject({ target_id: 'remote', project_directory: '~/' });
});

test('a discovery failure can retry and a phone chooser stays within the viewport', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  const discover = state.discover;
  state.discover = () => ({ error: 'Folder is unavailable' });
  await page.setViewportSize({ width: 320, height: 700 });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).tap();
  await expect(page.locator('.project-browser').getByRole('alert')).toContainText('Folder is unavailable');
  state.discover = discover;
  await page.getByRole('button', { name: 'Retry', exact: true }).tap();
  const folder = page.getByRole('button', { name: /^code Folder/ });
  await expect(folder).toBeVisible();
  expect((await folder.boundingBox()).height).toBeGreaterThanOrEqual(44);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(320);
  await page.locator('#new-project-location-toggle').click();
  await page.getByRole('textbox', { name: 'Folder path' }).fill('/home/controller/code');
  await page.getByRole('textbox', { name: 'Folder path' }).press('Enter');
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
});

test('search, folder location, and pasted text survive switching source choices', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await page.locator('#new-project-source').fill('example/draft');
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await page.getByRole('textbox', { name: 'Search GitHub repositories' }).fill('keep this query');
  await expect.poll(() => state.discoveries.length).toBe(2);
  await expect(page.getByRole('button', { name: /^example\/app/ })).toBeVisible();
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await page.getByRole('button', { name: /^code Folder/ }).click();
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await expect(page.getByRole('textbox', { name: 'Search GitHub repositories' })).toHaveValue('keep this query');
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await expect(page.locator('#new-project-source')).toHaveValue('example/draft');
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/home/controller/code');
  expect(state.discoveries).toHaveLength(4);
  expect(state.creates).toHaveLength(0);
});

test('folder filtering finds omitted repositories and Open folder browses without selecting', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  state.discover = body => ({
    entries: body.filter === 'omitted' ? [
      { name: 'omitted', source: '/home/controller/omitted', kind: 'repository' },
    ] : [{ name: 'first', source: '/home/controller/first', kind: 'directory' }],
    directory: body.path || '/home/controller', parent: '/', truncated: !body.filter,
  });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await expect(page.locator('.project-browser')).toContainText('Filter by folder name');
  await page.getByRole('textbox', { name: 'Filter folder names' }).fill('omitted');
  await expect(page.getByRole('button', { name: /^omitted / })).toBeVisible();
  expect(state.discoveries.at(-1)).toEqual({ kind: 'directory', path: '/home/controller', filter: 'omitted' });
  await page.getByRole('button', { name: 'Open folder omitted', exact: true }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/home/controller/omitted');
  await expect(page.getByRole('textbox', { name: 'Filter folder names' })).toHaveValue('');
  expect(state.discoveries.at(-1)).toEqual({ kind: 'directory', path: '/home/controller/omitted' });
  expect(state.creates).toHaveLength(0);
  await expect(page.locator('#new-progress')).toContainText('Project');
});

test('cancelled folder loading offers a retry without leaving the chooser', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  const discover = state.discover;
  let release;
  state.discover = body => new Promise(resolve => { release = () => resolve(discover(body)); });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await page.getByRole('button', { name: 'Cancel loading', exact: true }).click();
  await expect(page.locator('.project-browser')).toContainText('Loading cancelled');
  release();
  state.discover = discover;
  await page.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect(page.getByRole('button', { name: /^code Folder/ })).toBeVisible();
  expect(state.creates).toHaveLength(0);
});

test('typing a folder path cancels its pending listing without replacing the edited path', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  const discover = state.discover;
  let release;
  state.discover = body => new Promise(resolve => { release = () => resolve(discover(body)); });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await page.locator('#new-project-location-toggle').click();
  const location = page.getByRole('textbox', { name: 'Folder path' });
  await location.fill('/home/controller/code');
  release();
  await expect.poll(() => state.discoveryFailures).toBe(1);
  await expect(location).toHaveValue('/home/controller/code');
  await expect(location).toBeFocused();
  await expect(page.locator('.project-browser')).toContainText('Choose Go');
  await expect(page.locator('.project-result-row')).toHaveCount(0);
  state.discover = discover;
  await location.press('Enter');
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
});

test('managed projects require an explicit choice and Title can be edited during Review preflight', async ({ page }) => {
  const state = await mount(page, { bundles: [
    { id: 'first', repositories: [] }, { id: 'chosen', repositories: [] },
  ] });
  await projectStep(page);
  await expect(page.locator('#new-next')).toBeHidden();
  await expect(page.locator('#new-title')).toHaveCount(0);
  await expect(page.locator('#new-step')).not.toContainText('unpublished');
  await page.locator('#new-form').evaluate(form => form.requestSubmit());
  expect(state.preflights).toHaveLength(0);
  await expect(page.locator('#new-progress')).toContainText('Project');

  let release;
  state.holdPreflight = new Promise(resolve => { release = resolve; });
  await page.getByRole('button', { name: 'chosen', exact: true }).click();
  const title = page.getByRole('textbox', { name: 'Title (optional)', exact: true });
  await expect(title).toHaveAttribute('placeholder', 'chosen via alpha');
  await title.fill('Investigate startup');
  await title.press('Enter');
  expect(state.actions).toHaveLength(0);
  await refresh(page, state);
  await expect(title).toBeFocused();
  await expect(title).toHaveValue('Investigate startup');
  release();
  await expect(page.locator('#new-next')).toBeEnabled();
  await expect(title).toBeFocused();
  await expect(title).toHaveValue('Investigate startup');
  await page.locator('#new-back').click();
  await expect(page.locator('#new-title')).toHaveCount(0);
  await expect(page.locator('#new-next')).toBeHidden();
  await page.getByRole('button', { name: 'chosen', exact: true }).click();
  await expect(title).toHaveValue('Investigate startup');
  await expect(page.locator('#new-next')).toBeEnabled();
  await title.press('Enter');
  expect(state.actions[0]).toMatchObject({ bundle_id: 'chosen', title: 'Investigate startup' });
});

test('folder path disclosure is optional, keyboard accessible, and survives updates while editing', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/home/controller');
  await expect(page.locator('#new-project-location')).toBeHidden();
  await expect(page.getByRole('button', { name: 'Home', exact: true })).toBeVisible();
  const disclosure = page.locator('#new-project-location-toggle');
  await disclosure.focus();
  await disclosure.press('Enter');
  const location = page.getByRole('textbox', { name: 'Folder path' });
  await location.fill('/home/controller/code');
  await refresh(page, state);
  await expect(location).toBeFocused();
  await expect(location).toHaveValue('/home/controller/code');
  await location.press('Enter');
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
  await expect(page.locator('#new-project-location')).toBeHidden();
  await expect(page.locator('#new-project-current-folder')).toBeFocused();
  expect(state.creates).toHaveLength(0);
});

test('a GitHub service failure reports its actionable error without suggesting another sign-in', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  state.discover = () => ({ error: 'GitHub is temporarily unavailable. Retry later.' });
  await projectStep(page);
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await expect(page.locator('.project-browser').getByRole('alert')).toHaveText('GitHub is temporarily unavailable. Retry later.');
  await expect(page.locator('.project-browser')).not.toContainText(/Sign in|gh auth login/);
  await expect(page.getByRole('button', { name: 'Retry', exact: true })).toBeVisible();
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await expect(page.locator('#new-project-source')).toBeVisible();
});

test('compact source navigation leaves the first folder and repository visible on a phone', async ({ page }) => {
  await mount(page, { bundles: [] });
  await projectStep(page);
  await page.getByRole('button', { name: /^Browse folders/ }).click();
  const sources = page.getByRole('group', { name: 'Find a project' });
  await expect(sources.getByRole('button', { name: 'Folders', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('#new-step')).not.toContainText('Pick a saved project');
  for (const label of ['GitHub', 'Folders', 'URL']) {
    const box = await sources.getByRole('button', { name: label, exact: true }).boundingBox();
    expect(box.height).toBeGreaterThanOrEqual(44);
    expect(box.height).toBeLessThanOrEqual(46);
  }
  const assertVisibleWithoutScrolling = async row => {
    await expect(row).toBeVisible();
    await page.evaluate(() => scrollTo(0, 0));
    const box = await row.boundingBox();
    expect(box.y).toBeGreaterThanOrEqual(0);
    expect(box.y + box.height).toBeLessThanOrEqual(844);
  };
  const folder = page.getByRole('button', { name: /^code Folder/ });
  await assertVisibleWithoutScrolling(folder);
  await folder.click();
  await assertVisibleWithoutScrolling(page.getByRole('button', { name: /^Use code/ }));
  await expect(page.getByRole('button', { name: 'Recent & saved projects', exact: true })).toBeVisible();
  await sources.getByRole('button', { name: 'URL', exact: true }).click();
  await expect(page.locator('#new-project-source')).toBeVisible();
  await page.getByRole('button', { name: 'Recent & saved projects', exact: true }).click();
  await expect(page.getByRole('button', { name: /^Browse folders/ })).toContainText('On your Mjolnir computer');
});
