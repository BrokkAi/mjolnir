const { test, expect } = require('@playwright/test');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, serviceWorkers: 'block' });

function preview(overrides = {}) {
  return {
    checkout: '/work/repo',
    destination: '/workspace/repo',
    branch: 'mj/session',
    fetch_url: 'https://github.com/example/repo.git',
    push_urls: ['https://github.com/example/repo.git'],
    default_branch: 'main',
    unpushed_commits: 2,
    staged_files: 1,
    unstaged_files: 1,
    untracked_files: 3,
    untracked_bytes: 2_621_440,
    host_checkout_retained: true,
    ...overrides,
  };
}

function session(overrides = {}) {
  return {
    id: 'stopped',
    title: 'Local checkout',
    state: 'stopped',
    lifecycle: 'stopped',
    workspace_id: 'test',
    profile_id: 'alpha',
    target_id: 'local',
    capabilities: { resume: true },
    compatible_resume_targets: ['local', 'container'],
    last_activity_at_ms: 1_000,
    ...overrides,
  };
}

async function mount(page, { answer = { kind: 'converting-raw-checkout', preview: preview() } } = {}) {
  const state = {
    snapshot: {
      revision: 1,
      workspaces: [{ id: 'test', name: 'Test' }],
      sessions: [session()],
      profiles: [{ id: 'alpha', harness_kind: 'codex' }],
      targets: [
        { id: 'local', kind: 'local', requires_project_directory: true, recent_project_directories: [] },
        { id: 'container', kind: 'podman', requires_project_directory: false, recent_project_directories: [] },
      ],
      bundles: [],
      capacity: [],
      launch_failures: [],
    },
    snapshots: 0,
    actions: [],
    preflights: [],
    answer,
    holdPreflight: null,
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
    if (pathname === '/api/snapshot') {
      state.snapshots += 1;
      return json(state.snapshot);
    }
    if (pathname === '/api/events') return route.fulfill({ contentType: 'text/event-stream', body: ': fixture\n\n' });
    if (pathname === '/api/preflight/resume') {
      state.preflights.push(route.request().postDataJSON());
      if (state.holdPreflight) await state.holdPreflight;
      return json(state.answer);
    }
    if (pathname === '/api/actions') {
      state.actions.push(route.request().postDataJSON());
      return route.fulfill({ status: 202, body: '' });
    }
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    if (['viewer.html', 'viewer.js', 'viewer.css', 'markdown.js', 'tool-output.js', 'manifest.webmanifest', 'icon.svg'].includes(file)) {
      return route.fulfill({ path: path.join(webRoot, file === 'icon.svg' ? '../icons/icon.svg' : file) });
    }
    return route.fulfill({ status: 404, body: '' });
  });
  await page.goto('https://viewer.test/#workspace/test/resume/stopped');
  await expect(page.locator('#resume-detail-view')).toBeVisible();
  return state;
}

function detail(page) {
  return page.locator('#resume-detail-view');
}

async function chooseContainer(page) {
  await detail(page).locator('[data-role="resume-target"] select').selectOption('container');
}

test('a container destination warns what travels and holds Resume until it is acknowledged', async ({ page }) => {
  const state = await mount(page);
  const resume = detail(page).locator('button[data-action="resume"]');
  await expect(resume).toBeEnabled();

  await chooseContainer(page);
  await expect.poll(() => state.preflights.length).toBe(1);
  expect(state.preflights[0]).toEqual({ session_id: 'stopped', target_id: 'container' });

  await expect(detail(page)).toContainText(
    'Clone https://github.com/example/repo.git (default branch main) into /workspace/repo on branch mj/session; push to https://github.com/example/repo.git.',
  );
  await expect(detail(page)).toContainText(
    '1 staged, 1 unstaged, and 3 untracked files (2.5 MB) will be copied into the container. Ignored files such as build output, .env, and node_modules will not.',
  );
  await expect(detail(page)).toContainText('2 commits not on https://github.com/example/repo.git travel in the checkpoint.');
  await expect(detail(page)).toContainText('/work/repo stays on this machine and will no longer track this session.');

  const acknowledge = detail(page).locator('[data-role="resume-conversion"] input');
  await expect(acknowledge).not.toBeChecked();
  await expect(resume).toBeDisabled();
  expect(state.actions).toEqual([]);

  await acknowledge.check();
  await expect(resume).toBeEnabled();
  await resume.click();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0].target_id).toBe('container');
});

test('a checkout that cannot convert reports why and leaves Resume disabled', async ({ page }) => {
  const state = await mount(page, {
    answer: { kind: 'unavailable', detail: '/work/repo has no network Git remote; add one or resume this session on a bare target' },
  });
  await chooseContainer(page);
  await expect.poll(() => state.preflights.length).toBe(1);
  await expect(detail(page)).toContainText('has no network Git remote');
  await expect(detail(page).locator('button[data-action="resume"]')).toBeDisabled();
  await expect(detail(page).locator('[data-role="resume-conversion"]')).toHaveCount(0);
});

test('a bare destination asks nothing and leaves Resume available', async ({ page }) => {
  const state = await mount(page);
  await detail(page).locator('[data-role="resume-target"] select').selectOption('local');
  await expect(detail(page).locator('button[data-action="resume"]')).toBeEnabled();
  await expect(detail(page)).not.toContainText('Checking checkout…');
  expect(state.preflights).toEqual([]);
});
