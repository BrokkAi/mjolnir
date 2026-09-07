const { test, expect } = require('@playwright/test');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, serviceWorkers: 'block' });

async function mount(page, { bundles = [{ id: 'existing', repositories: [] }] } = {}) {
  const state = {
    snapshot: {
      revision: 1, workspaces: [{ id: 'test', name: 'Test' }, { id: 'other', name: 'Other' }], sessions: [],
      profiles: [{ id: 'alpha', harness_kind: 'codex' }, { id: 'beta', harness_kind: 'claude' }],
      targets: [
        { id: 'container', kind: 'podman', requires_project_directory: false, recent_project_directories: [] },
        { id: 'local', kind: 'local', requires_project_directory: true, recent_project_directories: ['/work/recent', '/work/older'] },
        { id: 'remote', kind: 'ssh', requires_project_directory: true, recent_project_directories: ['/remote/recent'] },
      ], bundles, capacity: [], launch_failures: [],
    },
    snapshots: 0, preflights: [], actions: [], creates: [], dirty: [], rejectCreate: false,
    holdCreate: null, holdLaunch: null,
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
  await page.route('**/*', async route => {
    const pathname = new URL(route.request().url()).pathname;
    const json = value => route.fulfill({ contentType: 'application/json', body: JSON.stringify(value) });
    if (pathname === '/api/snapshot') { state.snapshots++; return json(state.snapshot); }
    if (pathname === '/api/events') return route.fulfill({ contentType: 'text/event-stream', body: ': fixture\n\n' });
    if (pathname === '/api/preflight/new') { state.preflights.push(route.request().postDataJSON()); return json({ dirty_repositories: state.dirty }); }
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

test('empty bundle list supports creation, retry, selection, and dirty confirmation', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await page.locator('#new-next').click();
  await expect(page.locator('#new-error')).toContainText('Choose or create a bundle');
  await page.locator('#new-bundle-source').fill('example/created');
  state.rejectCreate = true;
  await page.getByRole('button', { name: 'Save bundle', exact: true }).click();
  await expect(page.locator('#new-error')).toContainText('Repository source is invalid');
  await expect(page.locator('#new-bundle-source')).toHaveValue('example/created');
  state.rejectCreate = false;
  await page.getByRole('button', { name: 'Save bundle', exact: true }).click();
  await expect(page.locator('#new-bundle').getByRole('radio', { name: 'created', exact: true })).toBeChecked();
  expect(state.creates).toEqual([{ source: 'example/created' }, { source: 'example/created' }]);
  state.dirty = ['created'];
  await page.locator('#new-next').click();
  await page.locator('#new-next').click();
  await expect(page.locator('#new-error')).toContainText('Confirm before starting');
  await page.getByLabel('Start anyway').check();
  await page.locator('#new-next').click();
  await page.locator('#new-next').click();
  await expect(page).toHaveURL(/#workspace\/test$/);
  expect(state.actions).toHaveLength(1);
  expect(state.actions[0]).toMatchObject({ action: 'new', workspace_id: 'test', bundle_id: 'created', dirty_ack: ['created'] });
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

test('bundle save stays single-flight and a late result cannot alter a replacement wizard', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  let release;
  state.holdCreate = new Promise(resolve => { release = resolve; });
  await page.locator('#new-bundle-source').fill('example/created');
  await page.getByRole('button', { name: 'Save bundle', exact: true }).click();
  await expect.poll(() => state.creates.length).toBe(1);
  await expect(page.getByRole('button', { name: 'Creating bundle…', exact: true })).toBeDisabled();
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
    id: 'stopped', title: 'Stopped test', state: 'stopped', lifecycle: 'stopped',
    workspace_id: 'test', profile_id: 'alpha', target_id: 'local',
    capabilities: { resume: true }, compatible_resume_targets: ['local', 'remote'],
    queued_prompts: [{ id: 'queued', text: 'queued work' }],
  });
  await refresh(page, state);
  await page.evaluate(() => { location.hash = '#workspace/test/resume'; });
  await page.locator('[data-role="resume-profile"]').getByRole('radio', { name: /^beta/ }).check();
  await page.locator('[data-role="resume-target"]').getByRole('radio', { name: 'remote', exact: true }).check();
  await page.getByRole('radio', { name: 'Discard them', exact: true }).check();
  await refresh(page, state);
  await expect(page.locator('[data-role="resume-profile"]').getByRole('radio', { name: /^beta/ })).toBeChecked();
  await expect(page.getByRole('radio', { name: 'Discard them', exact: true })).toBeChecked();
  await page.getByRole('button', { name: 'Resume', exact: true }).click();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0]).toEqual({ action: 'resume', session_id: 'stopped', workspace_id: 'test', profile_id: 'beta', target_id: 'remote', queue: 'discard' });
});
