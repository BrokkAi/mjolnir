const { test, expect } = require('@playwright/test');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 } });

const WEB_ROOT = path.resolve(__dirname, '../../../mj-controller/src/web');
const WORKSPACE_ID = 'workspace-1';
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
    title: id,
    harness_kind: 'codex',
    profile_id: 'codex',
    bundle_id: `bundle-${id}`,
    target_id: 'local',
    state: lifecycle === 'live' ? 'running' : lifecycle,
    created_at: '2026-09-05T00:00:00Z',
    updated_at: '2026-09-05T00:00:00Z',
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
      cancel_turn: false,
      cancel_operation: false,
      stop: false,
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
    // Deliberately arrive out of label order. The TUI orders these project
    // rows by their visible short label, not by whichever session came first.
    sessions: [
      session('beta-session', 'project-beta', 'Beta'),
      session('alpha-first', 'project-alpha', 'Alpha'),
      session('alpha-second', 'project-alpha', 'Alpha'),
      // A visible label is not an identity: this must remain its own group.
      session('other-alpha', 'project-other-alpha', 'Alpha'),
      session('stopped-alpha', 'project-alpha', 'Alpha', { lifecycle: 'stopped' }),
      session('other-workspace', 'project-other-workspace', 'Other', {
        workspaceId: 'workspace-2',
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
  await expect(page.locator('#sessions .project')).toHaveCount(3);
}

test('the live session list separates projected projects with visible divider headings', async ({ page }) => {
  await mount(page);

  const groups = page.locator('#sessions > .project');
  await expect(groups.locator('.project-heading')).toHaveText(['Alpha 2', 'Alpha 1', 'Beta 1']);
  await expect(groups.nth(0).locator('.session h3')).toHaveText(['alpha-first', 'alpha-second']);
  await expect(groups.nth(1).locator('.session h3')).toHaveText(['other-alpha']);
  await expect(groups.nth(2).locator('.session h3')).toHaveText(['beta-session']);

  // A stopped session and a session from another workspace never enter the
  // live dashboard, so their project names cannot create false headings.
  await expect(page.locator('#sessions')).not.toContainText('stopped-alpha');
  await expect(page.locator('#sessions')).not.toContainText('other-workspace');

  const headingStyle = await page.locator('.project-heading').first().evaluate(node => {
    const style = getComputedStyle(node);
    return {
      tag: node.tagName,
      role: node.getAttribute('role'),
      borderBottomStyle: style.borderBottomStyle,
      borderBottomWidth: style.borderBottomWidth,
      color: style.color,
      textTransform: style.textTransform,
      fontWeight: style.fontWeight,
    };
  });
  expect(headingStyle).toEqual({
    tag: 'H2',
    role: null,
    borderBottomStyle: 'solid',
    borderBottomWidth: '1px',
    color: 'rgb(230, 235, 224)',
    textTransform: 'none',
    fontWeight: '700',
  });
  await expect(page.locator('.project-heading button')).toHaveCount(0);

  const metrics = await page.evaluate(() => ({
    documentWidth: document.documentElement.scrollWidth,
    viewportWidth: document.documentElement.clientWidth,
    headingWidths: [...document.querySelectorAll('.project-heading')].map(
      node => node.getBoundingClientRect().width,
    ),
  }));
  expect(metrics.documentWidth).toBeLessThanOrEqual(metrics.viewportWidth);
  expect(metrics.headingWidths.every(width => width > 0)).toBe(true);
});
