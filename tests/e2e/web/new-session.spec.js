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
    completions: [], directoryEntries: null,
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
      const repositories = state.snapshot.bundles.find(bundle => bundle.id === request.bundle_id)?.repositories || [];
      if (state.holdPreflight) await state.holdPreflight;
      if (state.preflightError) return route.fulfill({ status: 400, contentType: 'application/json', body: JSON.stringify({ error: state.preflightError }) });
      return json({
        remote_repairs: state.remoteRepairs,
        project_directory: bare ? state.resolvedDirectory : null,
        remote_repositories: bare ? [] : repositories.length ? repositories.map(repository => ({
          id: repository.id, fetch_url: `https://github.com/${repository.github}.git`, default_branch: 'main', push_urls: [],
        })) : [{ id: request.bundle_id, fetch_url: 'https://github.com/example/repo.git', default_branch: 'main', push_urls: ['https://github.com/example/repo.git'] }],
        local_changes_excluded: !bare,
        managed_worktree: bare ? state.worktreeOptions : { available: false, default_create: false },
      });
    }
    if (pathname === '/api/paths/complete') {
      const request = route.request().postDataJSON();
      state.completions.push(request);
      if (state.directoryEntries) return json({ candidates: state.directoryEntries[request.prefix] || [], insert: null, truncated: false });
      return json({ candidates: ['/work/recent/', '/work/repos/'], insert: '/work/re', truncated: false });
    }
    if (pathname === '/api/bundles') {
      const request = route.request().postDataJSON();
      state.creates.push(request);
      if (state.holdCreate) await state.holdCreate;
      if (state.rejectCreate) return route.fulfill({ status: 400, contentType: 'application/json', body: JSON.stringify({ error: 'Repository source is invalid' }) });
      const repositories = (request.sources || [request.source]).map((source, index) => ({
        id: `repo-${index + 1}`, github: source, destination: `repo-${index + 1}`,
      }));
      state.snapshot.bundles.push({ id: 'created', primary_repository: 'repo-1', repositories });
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
// is being typed; a bundle source that is not a path asks nothing at all.
test('a project directory suggests paths on its own host and a bundle source only when it is one', async ({ page }) => {
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
  await page.locator('#new-project-source').fill('owner/repo');
  await page.waitForTimeout(400);
  expect(state.completions).toHaveLength(2);
});

test('a repository link proceeds directly to review and retains the source after failure', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await page.locator('#new-next').click();
  await expect(page.locator('#new-error')).toContainText('Choose a project');
  await page.locator('#new-project-source').fill('example/created');
  state.rejectCreate = true;
  await page.locator('#new-next').click();
  await expect(page.locator('#new-error')).toContainText('Repository source is invalid');
  await expect(page.locator('#new-project-source')).toHaveValue('example/created');
  state.rejectCreate = false;
  await page.locator('#new-next').click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.creates).toEqual([{ source: 'example/created' }, { source: 'example/created' }]);
  await expect(page.locator('#new-step')).toContainText('Local changes');
  await page.locator('#new-next').click();
  await expect(page).toHaveURL(/#workspace\/test$/);
  expect(state.actions).toHaveLength(1);
  expect(state.actions[0]).toMatchObject({ action: 'new', workspace_id: 'test', bundle_id: 'created' });
  expect(state.actions[0]).not.toHaveProperty('dirty_ack');
});

test('an isolated session can use a recent local project without entering a source', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  state.snapshot.local_project_directories = ['/work/recent'];
  await refresh(page, state);
  await projectStep(page);
  await expect(page.locator('#new-step')).not.toContainText(/bundle/i);
  await page.getByRole('radio', { name: /^\/work\/recent/ }).check();
  await page.locator('#new-next').click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.creates).toEqual([{ source: '/work/recent' }]);
  expect(state.preflights).toHaveLength(1);
  await page.locator('#new-back').click();
  await expect(page.getByRole('radio', { name: 'created', exact: true })).toBeChecked();
  await page.locator('#new-next').click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.creates).toHaveLength(1);
});

test('multiple repositories survive refresh, Back, and a failed preparation before launching together', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  const source = page.locator('#new-project-source');
  const add = page.getByRole('button', { name: 'Add repository', exact: true });
  await source.fill('example/app');
  await add.click();
  await expect(source).toBeFocused();
  await expect(source).toHaveValue('');
  await expect(page.locator('.project-repositories')).toContainText('example/app · Primary');
  await source.fill('example/api');
  const original = await source.elementHandle();
  await refresh(page, state);
  expect(await original.evaluate(node => node.isConnected)).toBe(true);
  await expect(source).toBeFocused();
  await expect(source).toHaveValue('example/api');
  await page.locator('#new-back').click();
  await page.locator('#new-next').click();
  await expect(page.locator('.project-repositories')).toContainText('example/app · Primary');
  await expect(source).toHaveValue('example/api');
  state.rejectCreate = true;
  await page.locator('#new-next').click();
  await expect(page.locator('#new-error')).toContainText('Repository source is invalid');
  await expect(page.locator('.project-repositories')).toContainText('example/app');
  await expect(source).toHaveValue('example/api');
  state.rejectCreate = false;
  await source.fill('example/service');
  await page.locator('#new-next').click();
  await expect(page.locator('#new-next')).toHaveText('Start');
  expect(state.creates).toEqual([
    { sources: ['example/app', 'example/api'] },
    { sources: ['example/app', 'example/service'] },
  ]);
  await expect(page.locator('#new-step')).toContainText('https://github.com/example/app.git');
  await expect(page.locator('#new-step')).toContainText('https://github.com/example/service.git');
  await expect(page.locator('#new-step')).not.toContainText(/bundle/i);
  await page.locator('#new-back').click();
  await expect(page.getByRole('radio', { name: /created.*2 repositories/ })).toBeChecked();
  await page.locator('#new-next').click();
  await expect(page.locator('#new-next')).toHaveText('Start');
  expect(state.creates).toHaveLength(2);
  await page.locator('#new-next').click();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0]).toMatchObject({ action: 'new', bundle_id: 'created' });
});

test('removing the primary repository promotes the next and Next uses the staged list', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  const source = page.locator('#new-project-source');
  const add = page.getByRole('button', { name: 'Add repository', exact: true });
  for (const repository of ['example/app', 'example/api', 'example/shared']) {
    await source.fill(repository);
    await add.click();
  }
  await page.getByRole('button', { name: 'Remove example/app', exact: true }).click();
  await expect(page.locator('.project-repositories li')).toHaveCount(2);
  await expect(page.locator('.project-repositories li').first()).toContainText('example/api · Primary');
  await expect(source).toHaveValue('');
  await page.locator('#new-next').click();
  await expect(page.locator('#new-next')).toHaveText('Start');
  expect(state.creates).toEqual([{ sources: ['example/api', 'example/shared'] }]);
});

test('duplicate repositories stay editable and choosing a saved project replaces the draft group', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page);
  const source = page.locator('#new-project-source');
  const add = page.getByRole('button', { name: 'Add repository', exact: true });
  await source.fill('example/app');
  await add.click();
  await source.fill(' example/app ');
  await add.click();
  await expect(page.locator('#new-error')).toContainText('already in the project');
  await expect(page.locator('.project-repositories li')).toHaveCount(1);
  await page.locator('#new-next').click();
  await expect(page.locator('#new-error')).toContainText('Remove the duplicate');
  expect(state.creates).toHaveLength(0);
  await page.getByRole('radio', { name: 'existing', exact: true }).check();
  await expect(page.locator('.project-repositories')).toHaveCount(0);
  await expect(source).toHaveValue('');
  await expect(page.locator('#new-error')).toBeEmpty();
  await page.locator('#new-next').click();
  await expect(page.locator('#new-next')).toHaveText('Start');
  expect(state.preflights.at(-1)).toMatchObject({ bundle_id: 'existing' });
  expect(state.creates).toHaveLength(0);
});

test('folder browsing can add a repository to a group without typing its path', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  state.directoryEntries = { '~/': ['~/projects/'], '~/projects/': ['~/projects/app/'] };
  await projectStep(page);
  await page.getByRole('button', { name: 'Browse folders', exact: true }).click();
  await page.getByRole('option', { name: '~/projects/', exact: true }).click();
  await page.getByRole('option', { name: '~/projects/app/', exact: true }).click();
  await page.getByRole('button', { name: 'Add repository', exact: true }).click();
  await expect(page.locator('.project-repositories')).toContainText('~/projects/app/ · Primary');
  await page.locator('#new-project-source').fill('example/api');
  await page.locator('#new-next').click();
  await expect(page.locator('#new-next')).toHaveText('Start');
  expect(state.creates).toEqual([{ sources: ['~/projects/app/', 'example/api'] }]);
  expect(state.completions.every(request => request.target_id === null)).toBe(true);
});

test('folder browsing navigates and selects a project without typing a path', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  state.directoryEntries = { '~/': ['~/projects/'], '~/projects/': ['~/projects/app/'] };
  await projectStep(page);
  await page.getByRole('button', { name: 'Browse folders', exact: true }).click();
  await page.getByRole('option', { name: '~/projects/', exact: true }).click();
  await page.getByRole('option', { name: '~/projects/app/', exact: true }).click();
  await expect(page.locator('#new-project-source')).toHaveValue('~/projects/app/');
  await page.getByRole('button', { name: 'Parent folder', exact: true }).click();
  await expect(page.locator('#new-project-source')).toHaveValue('~/projects/');
  await page.getByRole('option', { name: '~/projects/app/', exact: true }).click();
  await page.locator('#new-next').click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.creates).toEqual([{ source: '~/projects/app/' }]);
  expect(state.completions.every(request => request.target_id === null)).toBe(true);
});

test('late launch completion cannot replace another workspace wizard', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page);
  await page.locator('#new-next').click();
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
  await page.locator('#new-next').click();
  await expect.poll(() => state.preflights.length).toBe(1);
  await expect(page.locator('#new-next')).toHaveText('Checking…');
  await page.evaluate(() => { location.hash = '#workspace/other/new'; });
  await expect(page.locator('#new-profile')).toBeVisible();
  release();
  await expect.poll(() => state.preflightFailures).toBeGreaterThan(0);
  await expect(page.locator('#new-step')).toContainText('Profile');
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
  await page.locator('#new-next').click();
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
  await page.locator('#new-next').click();
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
  await page.locator('#new-next').click();
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
  await page.locator('#new-next').click();
  await expect(page.locator('#new-next')).toBeDisabled();
  const other = await context.newPage();
  const second = await mount(other);
  await projectStep(other);
  await other.locator('#new-next').click();
  await expect(other.locator('#new-next')).toHaveText('Start');
  await expect(other.locator('#new-next')).toBeEnabled();
  expect(second.preflights).toHaveLength(1);
  await expect(page.locator('#new-next')).toBeDisabled();
  release();
  await expect(page.locator('#new-next')).toBeEnabled();
});

test('project preparation stays single-flight and a late result cannot alter a replacement wizard', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  let release;
  state.holdCreate = new Promise(resolve => { release = resolve; });
  await page.locator('#new-project-source').fill('example/primary');
  await page.getByRole('button', { name: 'Add repository', exact: true }).click();
  await page.locator('#new-project-source').fill('example/created');
  await page.locator('#new-next').click();
  await expect.poll(() => state.creates.length).toBe(1);
  await expect(page.locator('#new-next')).toHaveText('Preparing project…');
  await expect(page.getByRole('button', { name: 'Add repository', exact: true })).toBeDisabled();
  await expect(page.getByRole('button', { name: 'Remove example/primary', exact: true })).toBeDisabled();
  await page.locator('#new-form').evaluate(form => form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })));
  expect(state.creates).toHaveLength(1);
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
  await page.locator('#new-next').click();
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
  await page.locator('#new-next').click();
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
    await page.locator('#new-next').click();
    const checkbox = page.getByRole('checkbox', { name: 'Create isolated checkout' });
    await expect(checkbox).not.toBeChecked();
    await expect(checkbox).toBeDisabled();
    await refresh(page, state);
    await expect(checkbox).toBeDisabled();
  });
}
