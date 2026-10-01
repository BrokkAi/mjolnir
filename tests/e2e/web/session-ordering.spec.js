const { test, expect } = require('@playwright/test');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 } });

const WEB_ROOT = path.resolve(__dirname, '../../../mj-controller/src/web');
const WORKSPACE_ID = 'workspace-1';
const BASE_MS = Date.parse('2026-09-05T00:00:00Z');
const ASSETS = new Set([
  'viewer.html',
  'viewer.js',
  'viewer.css',
  'markdown.js',
  'tool-output.js',
  'manifest.webmanifest',
  'icon.svg',
]);

function session(id, projectKey, projectLabel, options = {}) {
  const lifecycle = options.lifecycle || 'live';
  return {
    id,
    workspace_id: options.workspaceId || WORKSPACE_ID,
    title: options.title || id,
    harness_kind: 'codex',
    profile_id: 'codex',
    bundle_id: `bundle-${id}`,
    target_id: 'local',
    display_location: '/work/project',
    state: lifecycle === 'live' ? 'running' : lifecycle,
    created_at: options.createdAt || '2026-09-05T00:00:00Z',
    updated_at: '2026-09-05T00:00:00Z',
    last_activity_at_ms: options.activity,
    last_message_at_ms: options.message,
    has_error: false,
    preview: [],
    queued_prompts: [],
    active_user_shells: [],
    pending_elicitations: [],
    conversation_available: false,
    prompt_images_supported: false,
    incompatible_resume_targets: [],
    compatible_resume_targets: ['local'],
    project_label: projectLabel,
    project_key: projectKey,
    lifecycle,
    latest_event_ordinal: 1,
    activity: '',
    operation: null,
    chat_phase: 'idle',
    is_idle: true,
    config_options: [],
    plan_mode_active: false,
    turn_review: null,
    available_commands: [],
    capabilities: {
      open: false,
      prompt: false,
      run_shell: false,
      interrupt_turn: false,
      cancel_operation: false,
      suspend: false,
      rename: false,
      resume: false,
      set_config: false,
      set_plan_mode: false,
    },
  };
}

function snapshot() {
  return {
    revision: 1,
    generated_at: '2026-09-05T00:00:00Z',
    workspaces: [{ id: WORKSPACE_ID, name: 'Browser tests' }],
    // Deliberately arrive out of order: the flat list is sorted live, never
    // by arrival.
    sessions: [
      // The message stamp wins over this session's own newer activity stamp,
      // so it sorts below beta-activity despite having the newest activity.
      session('gamma-message', 'project-gamma', 'Gamma', { message: BASE_MS + 3_000, activity: BASE_MS + 9_000 }),
      // No message and no activity: created_at is the last-resort key.
      session('alpha-created', 'project-alpha', 'Alpha', { createdAt: '2026-09-04T00:00:00Z' }),
      // The newest message in the fixture, but a suspended session never
      // enters the live dashboard.
      session('stopped', 'project-stopped', 'Stopped', { lifecycle: 'suspended', message: BASE_MS + 20_000 }),
      session('beta-activity', 'project-beta', 'Beta', { activity: BASE_MS + 5_000 }),
      // Equal keys break on the title, then the id.
      session('echo-tie', 'project-echo', 'Echo', { activity: BASE_MS + 1_000 }),
      session('delta-tie', 'project-delta', 'Delta', { activity: BASE_MS + 1_000 }),
      // Another workspace's session must not appear under this tab.
      session('other-workspace', 'project-other', 'Other', {
        workspaceId: 'workspace-2',
        message: BASE_MS + 30_000,
      }),
    ],
    profiles: [],
    targets: [],
    bundles: [],
    review_config: { enabled: false, tier: 'quick', profile: null },
  };
}

async function mount(page) {
  const state = snapshot();
  await page.route('**/*', route => {
    const pathname = new URL(route.request().url()).pathname;
    if (pathname === '/api/snapshot') {
      return route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(state),
      });
    }
    if (pathname === '/api/events') {
      return route.fulfill({
        status: 200,
        headers: { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' },
        body: ': mocked event stream\n\n',
      });
    }
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    if (ASSETS.has(file)) return route.fulfill({ path: path.join(WEB_ROOT, file === 'icon.svg' ? '../icons/icon.svg' : file) });
    return route.fulfill({ status: 404, body: '' });
  });

  await page.goto(`https://viewer.test/#workspace/${WORKSPACE_ID}`);
  await expect(page.locator('#app')).toBeVisible();
  await expect(page.locator('#sessions > .session-grid')).toHaveCount(1);
  await expect(page.locator('#sessions .session')).toHaveCount(5);
}

test('the live dashboard is one flat list ordered by the last top-level message', async ({ page }) => {
  await mount(page);

  await expect(page.locator('#sessions .project')).toHaveCount(0);
  await expect(page.locator('#sessions .project-heading')).toHaveCount(0);
  await expect(page.locator('#sessions .session h3')).toHaveText([
    'beta-activity',
    'gamma-message',
    'delta-tie',
    'echo-tie',
    'alpha-created',
  ]);

  await expect(page.locator('#sessions')).not.toContainText('stopped');
  await expect(page.locator('#sessions')).not.toContainText('other-workspace');

  const metrics = await page.evaluate(() => ({
    documentWidth: document.documentElement.scrollWidth,
    viewportWidth: document.documentElement.clientWidth,
  }));
  expect(metrics.documentWidth).toBeLessThanOrEqual(metrics.viewportWidth);
});

test('each card names its project first in the meta row', async ({ page }) => {
  await mount(page);

  const expected = {
    'beta-activity': 'Beta',
    'gamma-message': 'Gamma',
    'delta-tie': 'Delta',
    'echo-tie': 'Echo',
    'alpha-created': 'Alpha',
  };
  for (const [id, label] of Object.entries(expected)) {
    const meta = page.locator(`#sessions .session[data-session-id="${id}"] .session-meta`);
    await expect(meta.locator('> span:first-child')).toHaveText(label);
  }

  const beta = page.locator('#sessions .session[data-session-id="beta-activity"] .session-meta');
  await expect(beta.locator('.session-location')).toHaveText('/work/project');
  await expect(beta.locator('.session-profile')).toHaveText('codex');
  await expect(beta).toHaveText('Beta·/work/project·codex');
});
