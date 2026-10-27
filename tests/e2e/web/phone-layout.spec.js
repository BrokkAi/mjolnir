const { test, expect } = require('@playwright/test');
const fs = require('node:fs');
const path = require('node:path');
const { viewerWireSnapshot, viewerDetailResponse } = require('./lab-env');

test.use({
  viewport: { width: 390, height: 844 },
  deviceScaleFactor: 3,
  isMobile: true,
  hasTouch: true,
  serviceWorkers: 'block',
});

const WEB_ROOT = path.resolve(__dirname, '../../../mj-controller/src/web');
const WORKSPACE_ID = 'workspace-phone';
const SERVER_TIME_MS = Date.parse('2030-06-15T15:00:00Z');
const ASSETS = new Set([
  'viewer.html',
  'viewer.js',
  'viewer.css',
  'markdown.js',
  'tool-output.js',
  'manifest.webmanifest',
  'icon.svg',
]);

function session(options = {}) {
  const capabilities = {
    open: true,
    prompt: true,
    run_shell: true,
    interrupt_turn: false,
    cancel_operation: false,
    suspend: false,
    rename: false,
    resume: false,
    set_config: false,
    set_plan_mode: false,
  };
  return {
    id: 'parent',
    workspace_id: WORKSPACE_ID,
    title: 'Phone conversation',
    harness_kind: 'codex',
    profile_id: 'codex',
    bundle_id: 'bundle-phone',
    target_id: 'local',
    display_location: '/work/project',
    state: 'running',
    lifecycle: 'live',
    transitioning: false,
    created_at: '2030-06-15T00:00:00Z',
    updated_at: '2030-06-15T00:00:00Z',
    last_activity_at_ms: SERVER_TIME_MS,
    has_error: false,
    preview: [],
    queued_prompts: [{ id: 'queue-1', text: 'Inspect the recent changes' }],
    active_user_shells: [{ id: 'shell-1', command: 'git status --short' }],
    background_tasks: [{
      id: 'task-1',
      command: 'cargo check --workspace',
      started_at_ms: SERVER_TIME_MS - 180_000,
      can_stop: true,
    }],
    pending_elicitations: [],
    conversation_available: true,
    prompt_images_supported: true,
    incompatible_resume_targets: [],
    compatible_resume_targets: ['local'],
    project_label: 'Phone layout',
    project_key: 'phone-layout',
    latest_event_ordinal: 1,
    activity: '',
    activity_details: { kind: 'idle' },
    operation: null,
    chat_phase: 'idle',
    is_idle: true,
    config_options: options.configOptions || [],
    plan_mode_active: false,
    turn_review: null,
    available_commands: [],
    subagent_session_ids: ['child'],
    managed_checkout_kind: null,
    capabilities,
    ...options.sessionOverrides,
  };
}

function transcript() {
  return {
    entries: Array.from({ length: 16 }, (_, index) => ({
      id: index + 1,
      updated_seq: index + 1,
      role: index % 2 ? 'agent' : 'user',
      label: index % 2 ? 'Agent' : 'You',
      recorded_at_ms: SERVER_TIME_MS - (16 - index) * 60_000,
      lines: [`Conversation entry ${index + 1}. Read the details, prepare a concise response, and keep this thread visible.`],
      glyph: index % 2 ? '●' : '○',
      tone: index % 2 ? 'agent' : 'user',
      tool_status: null,
      diffstats: [],
    })),
    latest_seq: 16,
    reset: true,
  };
}

function fixture(taskCount = 1, options = {}) {
  const parent = session({
    configOptions: options.configOptions || [],
    sessionOverrides: {
      background_tasks: Array.from({ length: taskCount }, (_, index) => ({
        id: `task-${index + 1}`,
        command: `cargo check --package sample-${index + 1}`,
        started_at_ms: SERVER_TIME_MS - 180_000,
        can_stop: true,
      })),
      ...(options.sessionOverrides || {}),
    },
  });
  return {
    snapshot: {
      revision: 1,
      generated_at: '2030-06-15T15:00:00Z',
      server_time_ms: SERVER_TIME_MS,
      workspaces: [{ id: WORKSPACE_ID, name: 'Phone layout' }],
      sessions: [parent, {
        ...session(),
        id: 'child',
        title: 'Review helper',
        subagent_session_ids: [],
        subagent_parent_id: 'parent',
      }],
      profiles: [],
      targets: [],
      bundles: [],
      review_config: { trigger: "off", profile: null },
      operator_away: options.operatorAway === true,
      auto_afk_minutes: options.autoAfkMinutes ?? 0,
    },
    actions: [],
    snapshots: 0,
    conversation: transcript(),
  };
}

async function mount(page, taskCount = 1, options = {}) {
  const state = fixture(taskCount, options);
  await page.addInitScript(() => {
    window.EventSource = class extends EventTarget {
      constructor(url) {
        super();
        this.url = url;
        window.fixtureEvents = this;
        queueMicrotask(() => this.dispatchEvent(new Event('open')));
      }

      close() {}
    };
  });
  await page.route('**/*', async route => {
    const pathname = new URL(route.request().url()).pathname;
    const json = value => route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(value),
    });
    if (pathname === '/api/snapshot') {
      state.snapshots += 1;
      return json(viewerWireSnapshot(state.snapshot));
    }
    const detailPath = pathname.match(/^\/api\/sessions\/([^/]+)\/row$/);
    if (detailPath) {
      const detail = viewerDetailResponse(state.snapshot, decodeURIComponent(detailPath[1]));
      return detail ? json(detail) : route.fulfill({ status: 404, contentType: 'application/json', body: JSON.stringify({ error: 'session not found' }) });
    }
    if (pathname === '/api/events') {
      return route.fulfill({
        status: 200,
        headers: { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' },
        body: ': fixture\n\n',
      });
    }
    if (pathname === '/api/actions') {
      const action = route.request().postDataJSON();
      state.actions.push(action);
      if (action.action === 'set-operator-away') state.snapshot.operator_away = action.away;
      return route.fulfill({ status: 202, body: '' });
    }
    if (pathname.startsWith('/api/conversations/')) {
      if (route.request().method() === 'GET') return json(state.conversation);
      return route.fulfill({ status: 204, body: '' });
    }
    if (pathname.endsWith('/client-state')) {
      return json({ draft: '', through_event_ordinal: 0 });
    }
    if (pathname.endsWith('/draft')) return route.fulfill({ status: 204, body: '' });
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    if (ASSETS.has(file)) {
      return route.fulfill({
        path: path.join(WEB_ROOT, file === 'icon.svg' ? '../icons/icon.svg' : file),
      });
    }
    return route.fulfill({ status: 404, body: '' });
  });

  await page.goto(`https://viewer.test/#workspace/${WORKSPACE_ID}`);
  await expect(page.locator('#sessions .session[data-session-id="parent"]')).toBeVisible();
  await page.locator('#sessions .session[data-session-id="parent"]').click();
  await expect(page.locator('#conversation-title')).toHaveText('Phone conversation');
  await expect(page.locator('#conversation-feed')).toContainText('Conversation entry 16');
  return state;
}

test('phone header menu toggles AFK and the snapshot indicator ends it', async ({ page }) => {
  const state = await mount(page);
  const indicator = page.locator('#operator-away-indicator');
  await expect(indicator).toBeHidden();

  await page.locator('#menu-button').tap();
  await page.getByRole('menuitem', { name: 'Go AFK' }).tap();
  await expect(indicator).toBeVisible();
  await expect(page.getByRole('menuitem', { name: 'End AFK' })).toBeVisible();
  expect(state.actions[0]).toEqual({ action: 'set-operator-away', away: true });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true);

  await indicator.tap();
  await expect(indicator).toBeHidden();
  expect(state.actions[1]).toEqual({ action: 'set-operator-away', away: false });
});

test('phone AFK action shows its pending state and reports a failure', async ({ page }) => {
  await mount(page);
  let releaseAction;
  let actionReached;
  const blockedAction = new Promise(resolve => { releaseAction = resolve; });
  const reachedAction = new Promise(resolve => { actionReached = resolve; });
  await page.route('**/api/actions', async route => {
    actionReached();
    await blockedAction;
    await route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'operator action failed' }),
    });
  });

  await page.locator('#menu-button').tap();
  await page.getByRole('menuitem', { name: 'Go AFK' }).tap();
  await reachedAction;
  await expect(page.locator('#operator-away-indicator')).toBeVisible();
  await expect(page.getByRole('menuitem', { name: 'Going AFK…' })).toBeDisabled();
  releaseAction();
  await expect(page.locator('#operator-away-error')).toHaveText('Could not go AFK: operator action failed');
  await expect(page.locator('#operator-away-indicator')).toBeHidden();
});

test('operator activity reporting stays off when disabled or already AFK', async ({ page }) => {
  for (const options of [{ autoAfkMinutes: 0 }, { autoAfkMinutes: 5, operatorAway: true }]) {
    const state = await mount(page, 1, options);
    state.actions.length = 0;
    await page.mouse.move(12, 12);
    await page.keyboard.press('Tab');
    await page.waitForTimeout(1100);
    expect(state.actions).toEqual([]);
  }
});

test('trusted global input signals activity immediately and coalesces while a request is pending', async ({ page }) => {
  const state = await mount(page, 1, { autoAfkMinutes: 5 });
  state.actions.length = 0;

  await page.evaluate(() => {
    for (const type of ['keydown', 'paste', 'input', 'pointermove', 'pointerdown', 'pointerup', 'pointercancel', 'wheel']) {
      document.dispatchEvent(new Event(type, { bubbles: true }));
    }
  });
  expect(state.actions).toEqual([]);

  let releaseFirst;
  let signalReached;
  const blockedSignal = new Promise(resolve => { releaseFirst = resolve; });
  const reachedSignal = new Promise(resolve => { signalReached = resolve; });
  await page.route('**/api/actions', async route => {
    const action = route.request().postDataJSON();
    state.actions.push(action);
    if (action.action === 'record-operator-activity' && state.actions.filter(item => item.action === action.action).length === 1) {
      signalReached();
      await blockedSignal;
    }
    await route.fulfill({ status: 202, body: '' });
  });

  await page.mouse.move(15, 15);
  await reachedSignal;
  await page.keyboard.press('Tab');
  await page.keyboard.press('ArrowDown');
  await page.mouse.move(30, 30);
  expect(state.actions).toEqual([{ action: 'record-operator-activity' }]);

  releaseFirst();
  await expect.poll(() => state.actions.filter(item => item.action === 'record-operator-activity').length).toBe(2);
  await page.waitForTimeout(1100);
  expect(state.actions.filter(item => item.action === 'record-operator-activity')).toHaveLength(2);
});

async function settleLayout(page) {
  await page.evaluate(() => new Promise(resolve => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  }));
}

async function phoneMetrics(page) {
  return page.evaluate(() => {
    const scroll = document.querySelector('#conversation-scroll');
    const feed = document.querySelector('#conversation-feed');
    const scrollBox = scroll.getBoundingClientRect();
    const feedBox = feed.getBoundingClientRect();
    const earlier = document.querySelector('#earlier-messages');
    const earlierStyle = getComputedStyle(earlier);
    const feedViewport = Math.max(0, Math.min(feedBox.bottom, scrollBox.bottom) - Math.max(feedBox.top, scrollBox.top));
    return {
      viewport: { width: innerWidth, height: innerHeight },
      conversationScroll: scroll.clientHeight,
      conversationScrollPercent: +(scroll.clientHeight / innerHeight * 100).toFixed(2),
      visibleFeed: +feedViewport.toFixed(2),
      composerTextHeight: +document.querySelector('#prompt-text').getBoundingClientRect().height.toFixed(2),
      earlierControl: +(
        earlier.getBoundingClientRect().height
        + parseFloat(earlierStyle.marginTop || 0)
        + parseFloat(earlierStyle.marginBottom || 0)
      ).toFixed(2),
      focused: document.activeElement?.id === 'prompt-text',
    };
  });
}

test('phone task details open full width and scroll within forty percent of the viewport', async ({ page }, testInfo) => {
  await mount(page, 12);
  const details = page.locator('#conversation-side');
  await details.locator('summary').tap();
  await expect(details).toHaveAttribute('open', '');
  const content = page.locator('.conversation-side-content');
  const dimensions = await page.evaluate(() => {
    const status = document.querySelector('#conversation-status').getBoundingClientRect();
    const details = document.querySelector('#conversation-side').getBoundingClientRect();
    const content = document.querySelector('.conversation-side-content');
    return {
      statusWidth: status.width,
      detailsWidth: details.width,
      contentWidth: content.getBoundingClientRect().width,
      contentHeight: content.clientHeight,
      contentScrollHeight: content.scrollHeight,
      maxHeight: getComputedStyle(content).maxHeight,
      overflowY: getComputedStyle(content).overflowY,
      summaryBottom: document.querySelector('#conversation-side > summary').getBoundingClientRect().bottom,
      contentTop: content.getBoundingClientRect().top,
    };
  });
  expect(dimensions.detailsWidth).toBeGreaterThan(dimensions.statusWidth - 4);
  expect(dimensions.contentWidth).toBeGreaterThan(dimensions.detailsWidth - 4);
  expect(parseFloat(dimensions.maxHeight)).toBeLessThanOrEqual(844 * 0.4 + 1);
  expect(parseFloat(dimensions.maxHeight)).toBeGreaterThan(844 * 0.4 - 2);
  expect(dimensions.overflowY).toBe('auto');
  expect(dimensions.contentTop).toBeGreaterThanOrEqual(dimensions.summaryBottom);
  expect(dimensions.contentScrollHeight).toBeGreaterThan(dimensions.contentHeight);
  await page.screenshot({ path: testInfo.outputPath('phone-expanded-details.png') });
  fs.writeFileSync(testInfo.outputPath('phone-expanded-metrics.json'), JSON.stringify(dimensions, null, 2));
});


test('delivered messages in one batch keep separate prose rows after reload', async ({ page }) => {
  const state = await mount(page);
  const messages = [
    { stable_id: 'message:review', label: 'Message from Review helper', lines: ['One finding.', 'The complete **second line** with 支持 Unicode.'] },
    { stable_id: 'message:build', label: 'Message from Build helper', lines: ['Build passed.', 'The second message arrived in the same delivery.'] },
  ].map(message => ({ ...message, id: 17, updated_seq: 17, role: 'message', tone: 'message', glyph: '←', recorded_at_ms: SERVER_TIME_MS }));
  state.conversation.entries.push(...messages);
  state.conversation.latest_seq = 17;
  state.snapshot.sessions[0].latest_event_ordinal = 17;
  await page.reload();
  for (const message of messages) {
    const row = page.locator(`.entry[data-entry-id="${message.stable_id}"]`);
    await expect(row).toContainText(message.label);
    await expect(row.locator('time')).toHaveAttribute('datetime', new Date(SERVER_TIME_MS).toISOString());
  }
  await expect(page.locator('.entry[data-entry-id="message:review"] strong').last()).toHaveText('second line');
  await expect(page.locator('.entry.tone-message')).toHaveCount(2);
  await page.route('**/api/v1/sessions/parent/history*', route => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({ frontier: 17, before: null, items: messages.map(message => ({
      role: message.role, label: message.label, text: message.lines.join('\n'), recorded_at_ms: message.recorded_at_ms,
    })) }),
  }));
  await page.locator('#earlier-messages').click();
  const history = page.getByRole('dialog', { name: 'Earlier messages' });
  await expect(history).toContainText('Message from Review helper');
  await expect(history).toContainText('The second message arrived in the same delivery.');
  await expect(history.locator('time')).toHaveCount(2);
  await history.getByRole('button', { name: 'Close', exact: true }).click();
  await page.reload();
  await expect(page.locator('.entry.tone-message')).toHaveCount(2);
  await expect(page.locator('.entry[data-entry-id="message:build"]')).toContainText('The second message arrived in the same delivery.');
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth);
  expect(overflow).toBe(false);
});
