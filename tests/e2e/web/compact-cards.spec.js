const { test, expect } = require('@playwright/test');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, serviceWorkers: 'block' });

const WEB_ROOT = path.resolve(__dirname, '../../../mj-controller/src/web');
const WORKSPACE_ID = 'workspace-1';
const OTHER_WORKSPACE_ID = 'workspace-2';
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

function session(id, projectKey, projectLabel, options = {}) {
  const lifecycle = options.lifecycle || 'live';
  const capabilities = {
    open: true,
    prompt: false,
    run_shell: false,
    cancel_turn: false,
    cancel_operation: false,
    stop: false,
    rename: false,
    resume: false,
    set_config: false,
    set_plan_mode: false,
    ...(options.capabilities || {}),
  };
  return {
    id,
    workspace_id: options.workspaceId || WORKSPACE_ID,
    title: options.title || id,
    harness_kind: 'codex',
    profile_id: options.profileId || 'codex',
    bundle_id: `bundle-${projectKey}`,
    target_id: options.targetId || 'local',
    display_location: options.displayLocation || '/work/project',
    state: lifecycle === 'live' ? 'running' : lifecycle,
    lifecycle,
    created_at: '2030-06-15T00:00:00Z',
    updated_at: '2030-06-15T00:00:00Z',
    last_activity_at_ms: options.activity || SERVER_TIME_MS,
    has_error: Boolean(options.hasError),
    preview: [],
    queued_prompts: Array.from({ length: options.queued || 0 }, (_, index) => ({ id: `queued-${id}-${index}`, text: 'queued' })),
    active_user_shells: [],
    pending_elicitations: options.pendingElicitations || [],
    conversation_available: capabilities.open,
    prompt_images_supported: false,
    incompatible_resume_targets: [],
    compatible_resume_targets: ['local'],
    project_label: projectLabel,
    project_key: projectKey,
    latest_event_ordinal: 1,
    activity: '',
    activity_details: options.activityDetails || { kind: 'idle' },
    operation: null,
    chat_phase: 'idle',
    is_idle: options.isIdle !== false,
    config_options: [],
    plan_mode_active: false,
    turn_review: null,
    available_commands: [],
    capabilities,
  };
}

function stateWith(sessions) {
  return {
    snapshot: {
      revision: 1,
      generated_at: '2030-06-15T15:00:00Z',
      server_time_ms: SERVER_TIME_MS,
      workspaces: [
        { id: WORKSPACE_ID, name: 'Browser tests' },
        { id: OTHER_WORKSPACE_ID, name: 'Other workspace' },
      ],
      sessions,
      profiles: [],
      targets: [],
      bundles: [],
      review_config: { enabled: false, tier: 'quick', profile: null },
    },
    snapshots: 0,
    actions: [],
    failAction: null,
  };
}

async function mount(page, sessions) {
  const state = stateWith(sessions);
  await page.addInitScript(() => {
    window.fixtureEventSources = [];
    window.EventSource = class extends EventTarget {
      constructor(url) {
        super();
        this.url = url;
        this.closed = false;
        window.fixtureEvents = this;
        window.fixtureEventSources.push(this);
        queueMicrotask(() => this.dispatchEvent(new Event('open')));
      }

      close() {
        this.closed = true;
      }
    };
  });
  await page.route('**/*', async route => {
    const pathname = new URL(route.request().url()).pathname;
    const json = value => route.fulfill({ contentType: 'application/json', body: JSON.stringify(value) });
    if (pathname === '/api/snapshot') {
      state.snapshots += 1;
      return json(state.snapshot);
    }
    if (pathname === '/api/events') {
      return route.fulfill({
        status: 200,
        headers: { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' },
        body: ': fixture\n\n',
      });
    }
    if (pathname === '/api/actions') {
      const body = route.request().postDataJSON();
      state.actions.push(body);
      if (state.failAction?.action === body.action) {
        return route.fulfill({
          status: 422,
          contentType: 'application/json',
          body: JSON.stringify({ error: state.failAction.error }),
        });
      }
      return route.fulfill({ status: 202, body: '' });
    }
    if (pathname.startsWith('/api/conversations/')) {
      return json({ entries: [], latest_seq: 0, reset: true });
    }
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    if (ASSETS.has(file)) {
      return route.fulfill({
        path: path.join(WEB_ROOT, file === 'icon.svg' ? '../icons/icon.svg' : file),
      });
    }
    return route.fulfill({ status: 404, body: '' });
  });

  await page.goto(`https://viewer.test/#workspace/${WORKSPACE_ID}`);
  await expect(page.locator('#app')).toBeVisible();
  await expect(page.locator('#sessions .session').first()).toBeVisible();
  return state;
}

async function renderAfterFrame(page) {
  await page.evaluate(() => new Promise(resolve => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  }));
}

async function refresh(page, state) {
  const previous = state.snapshots;
  state.snapshot.revision += 1;
  await page.evaluate(() => window.fixtureEvents.dispatchEvent(new Event('revision')));
  await expect.poll(() => state.snapshots).toBeGreaterThan(previous);
  await renderAfterFrame(page);
}

async function reconnect(page, state) {
  const previous = state.snapshots;
  await page.evaluate(() => {
    window.fixtureEvents.dispatchEvent(new Event('error'));
    window.dispatchEvent(new Event('online'));
  });
  await expect.poll(() => state.snapshots).toBeGreaterThan(previous);
  await renderAfterFrame(page);
}

function card(page, id) {
  return page.locator(`#sessions .session[data-session-id="${id}"]`);
}

function activity(kind, fields = {}) {
  return { kind, ...fields };
}

test('compact cards sort initial activity, expose metadata, clocks, attention, and a phone screenshot', async ({ page }) => {
  const longTitle = 'A long session title that occupies one ellipsized line on a narrow phone screen';
  await mount(page, [
    session('beta-turn', 'project-beta', 'Beta', {
      activity: SERVER_TIME_MS - 1_000,
      title: longTitle,
      displayLocation: '/work/attention',
      profileId: 'codex',
      hasError: true,
      pendingElicitations: [{ id: 'input-1' }],
      queued: 2,
      activityDetails: activity('turn', {
        turn_started_at_ms: SERVER_TIME_MS - 65_000,
        step_started_at_ms: SERVER_TIME_MS - 65_000,
      }),
      isIdle: false,
    }),
    session('beta-step', 'project-beta', 'Beta', {
      activity: SERVER_TIME_MS - 2_000,
      activityDetails: activity('step', {
        step_started_at_ms: SERVER_TIME_MS - 3_600_000,
      }),
      isIdle: false,
    }),
    session('alpha-background', 'project-alpha', 'Alpha', {
      activity: SERVER_TIME_MS - 3_000,
      activityDetails: activity('background', {
        background_started_at_ms: SERVER_TIME_MS - 65_000,
        label: 'Indexing',
      }),
      isIdle: false,
    }),
    session('alpha-idle', 'project-alpha', 'Alpha', {
      activity: SERVER_TIME_MS - 4_000,
      activityDetails: activity('idle', { idle_since_ms: SERVER_TIME_MS - 65_000 }),
    }),
    session('alpha-yesterday', 'project-alpha', 'Alpha', {
      activity: SERVER_TIME_MS - 4_500,
      activityDetails: activity('idle', { idle_since_ms: SERVER_TIME_MS - 86_465_000 }),
    }),
    session('lifecycle', 'project-lifecycle', 'Lifecycle', {
      activity: SERVER_TIME_MS - 5_000,
      activityDetails: activity('lifecycle', { label: 'Starting target' }),
      isIdle: false,
    }),
    session('unknown-idle', 'project-unknown', 'Unknown', {
      activity: SERVER_TIME_MS - 6_000,
      activityDetails: activity('idle'),
    }),
  ]);

  const groups = page.locator('#sessions > .project');
  await expect(groups.locator('.project-heading')).toHaveText([
    'Beta 2',
    'Alpha 3',
    'Lifecycle 1',
    'Unknown 1',
  ]);
  await expect(groups.nth(0).locator('.session h3')).toHaveText([longTitle, 'beta-step']);
  await expect(groups.nth(1).locator('.session h3')).toHaveText(['alpha-background', 'alpha-idle', 'alpha-yesterday']);

  const attention = card(page, 'beta-turn').locator('.session-attention-item');
  await expect(attention).toHaveText(['!', '?', '2']);
  await expect.poll(() => attention.evaluateAll(nodes => nodes.map(node => node.getAttribute('aria-label')))).toEqual([
    'Error',
    'Input needed',
    '2 queued prompts',
  ]);
  await expect(card(page, 'beta-turn').locator('button[aria-label^="Actions for"]')).toHaveText('⋯');
  await expect(card(page, 'beta-turn')).toHaveAttribute(
    'aria-label',
    /needs attention: error, input needed, 2 queued prompts/,
  );

  const meta = await card(page, 'beta-turn').locator('.session-location, .session-profile').evaluateAll(nodes =>
    nodes.map(node => ({ text: node.textContent, left: node.getBoundingClientRect().left, right: node.getBoundingClientRect().right })));
  expect(meta.map(node => node.text)).toEqual(['/work/attention', 'codex']);
  expect(meta[0].left).toBeLessThan(meta[1].left);
  expect(meta[1].right).toBeGreaterThan(meta[0].right - 2);

  const titleMetrics = await card(page, 'beta-turn').locator('h3').evaluate(node => {
    const style = getComputedStyle(node);
    const lineHeight = Number.parseFloat(style.lineHeight);
    return {
      lines: Math.round(node.getBoundingClientRect().height / lineHeight),
      height: node.getBoundingClientRect().height,
      lineHeight,
      whiteSpace: style.whiteSpace,
    };
  });
  // The card has three rows; its title occupies exactly one of them.
  expect(titleMetrics.lines).toBe(1);
  expect(titleMetrics.height).toBeLessThanOrEqual(titleMetrics.lineHeight + 1);
  expect(titleMetrics.whiteSpace).toBe('nowrap');
  await expect(card(page, 'beta-turn').locator(':scope > div, :scope > p')).toHaveCount(3);

  await expect(card(page, 'beta-turn').locator('.session-activity')).toHaveText(/Turn 1m05s · Step 1m05s/);
  await expect(card(page, 'beta-step').locator('.session-activity')).toHaveText(/Step 1h00m/);
  await expect(card(page, 'alpha-background').locator('.session-activity')).toHaveText('Indexing 1m05s');
  const idleClock = await page.evaluate(timestamp => {
    const date = new Date(timestamp);
    return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`;
  }, SERVER_TIME_MS - 65_000);
  await expect(card(page, 'alpha-idle').locator('.session-activity')).toHaveText(`Idle since ${idleClock}`);
  await expect(card(page, 'alpha-yesterday').locator('.session-activity')).toHaveText(/Idle since yesterday \d\d:\d\d/);
  await expect(card(page, 'lifecycle').locator('.session-activity')).toHaveText('Starting target');
  await expect(card(page, 'unknown-idle').locator('.session-activity')).toHaveText('Idle');

  const overflow = await page.evaluate(() => ({
    documentWidth: document.documentElement.scrollWidth,
    viewportWidth: document.documentElement.clientWidth,
  }));
  expect(overflow.documentWidth).toBeLessThanOrEqual(overflow.viewportWidth + 1);
  await page.screenshot({ path: '/tmp/compact-web-cards.png', fullPage: true });
});

test('refresh, workspace navigation, and reconnect preserve card identity, focus, order, and an active press', async ({ page }) => {
  const state = await mount(page, [
    session('alpha-new', 'project-alpha', 'Alpha', {
      activity: SERVER_TIME_MS - 1_000,
      capabilities: { rename: true },
    }),
    session('alpha-old', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS - 2_000 }),
    session('beta', 'project-beta', 'Beta', { activity: SERVER_TIME_MS - 3_000 }),
    session('other', 'project-other', 'Other', {
      workspaceId: OTHER_WORKSPACE_ID,
      activity: SERVER_TIME_MS - 1_000,
    }),
  ]);

  const alphaCard = card(page, 'alpha-new');
  const original = await alphaCard.elementHandle();
  const trigger = alphaCard.locator('button[aria-label^="Actions for"]');
  await trigger.focus();
  await refresh(page, state);
  expect(await original.evaluate(node => node.isConnected)).toBe(true);
  expect(await page.evaluate(() => document.activeElement?.dataset.sessionMenu)).toBe('alpha-new');
  await expect(groupsFrom(page).locator('.project-heading')).toHaveText(['Alpha 2', 'Beta 1']);

  const box = await alphaCard.boundingBox();
  await page.mouse.move(box.x + 16, box.y + box.height / 2);
  await page.mouse.down();
  state.snapshot.sessions.find(item => item.id === 'alpha-new').activity = 'new activity';
  await refresh(page, state);
  expect(await original.evaluate(node => node.isConnected)).toBe(true);
  await page.waitForTimeout(550);
  await expect(alphaCard.locator('.session-menu')).toBeVisible();
  // The card and its in-progress gesture survive the refresh, so the held
  // pointer can still open the menu after the original 500ms threshold.
  await page.mouse.up();

  await page.getByRole('tab', { name: 'Other workspace' }).click();
  await expect(page).toHaveURL(/#workspace\/workspace-2$/);
  await expect(card(page, 'other')).toBeVisible();
  await page.getByRole('tab', { name: 'Browser tests' }).click();
  await expect(page).toHaveURL(/#workspace\/workspace-1$/);
  await expect(alphaCard).toBeVisible();
  expect(await original.evaluate(node => node.isConnected)).toBe(true);
  await expect(groupsFrom(page).locator('.project-heading')).toHaveText(['Alpha 2', 'Beta 1']);

  state.snapshot.sessions.find(item => item.id === 'beta').last_activity_at_ms = SERVER_TIME_MS - 10;
  await reconnect(page, state);
  await expect(groupsFrom(page).locator('.project-heading')).toHaveText(['Alpha 2', 'Beta 1']);
  await expect(groupsFrom(page).locator('> .project').first().locator('.session h3')).toHaveText(['alpha-new', 'alpha-old']);
});

function groupsFrom(page) {
  return page.locator('#sessions');
}

test('later sessions and projects append, while removed and reappeared ranks remain stable', async ({ page }) => {
  const state = await mount(page, [
    session('alpha-first', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS - 1_000 }),
    session('alpha-second', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS - 2_000 }),
    session('beta', 'project-beta', 'Beta', { activity: SERVER_TIME_MS - 3_000 }),
  ]);
  await expect(page.locator('#sessions > .project .project-heading')).toHaveText(['Alpha 2', 'Beta 1']);

  state.snapshot.sessions.push(
    session('alpha-late', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS - 10 }),
    session('gamma-late', 'project-gamma', 'Gamma', { activity: SERVER_TIME_MS - 20 }),
  );
  await refresh(page, state);
  await expect(page.locator('#sessions > .project .project-heading')).toHaveText(['Alpha 3', 'Beta 1', 'Gamma 1']);
  await expect(page.locator('#sessions > .project').first().locator('.session h3')).toHaveText([
    'alpha-first',
    'alpha-second',
    'alpha-late',
  ]);

  await card(page, 'gamma-late').focus();
  state.snapshot.sessions = state.snapshot.sessions.filter(item => item.id !== 'beta');
  await refresh(page, state);
  await expect(card(page, 'gamma-late')).toBeFocused();
  await expect(page.locator('#sessions > .project .project-heading')).toHaveText(['Alpha 3', 'Gamma 1']);
  state.snapshot.sessions.push(session('beta', 'project-beta', 'Beta', { activity: SERVER_TIME_MS + 1_000 }));
  await refresh(page, state);
  await expect(card(page, 'gamma-late')).toBeFocused();
  await expect(page.locator('#sessions > .project .project-heading')).toHaveText(['Alpha 3', 'Beta 1', 'Gamma 1']);

  await card(page, 'alpha-late').focus();
  state.snapshot.sessions = state.snapshot.sessions.filter(item => item.id !== 'alpha-second');
  await refresh(page, state);
  await expect(card(page, 'alpha-late')).toBeFocused();
  state.snapshot.sessions.push(session('alpha-second', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS + 2_000 }));
  await refresh(page, state);
  await expect(card(page, 'alpha-late')).toBeFocused();
  await expect(page.locator('#sessions > .project').first().locator('.session h3')).toHaveText([
    'alpha-first',
    'alpha-second',
    'alpha-late',
  ]);
});

test('a reload seeds a fresh order from last_activity_at_ms', async ({ page }) => {
  const state = await mount(page, [
    session('first', 'project-first', 'First', { activity: SERVER_TIME_MS - 1_000 }),
    session('second', 'project-second', 'Second', { activity: SERVER_TIME_MS - 2_000 }),
  ]);
  await expect(page.locator('#sessions > .project .project-heading')).toHaveText(['First 1', 'Second 1']);
  state.snapshot.sessions.find(item => item.id === 'first').last_activity_at_ms = SERVER_TIME_MS - 4_000;
  state.snapshot.sessions.find(item => item.id === 'second').last_activity_at_ms = SERVER_TIME_MS - 100;
  await page.reload();
  await expect(page.locator('#app')).toBeVisible();
  await expect(page.locator('#sessions > .project .project-heading')).toHaveText(['Second 1', 'First 1']);
});

test('menus follow capabilities, long press cancellation, right click, keyboard, confirmation, and errors', async ({ page }) => {
  const state = await mount(page, [
    session('openable', 'project-menu', 'Menu', {
      activity: SERVER_TIME_MS - 1_000,
      capabilities: { rename: true, cancel_operation: true, stop: true },
    }),
    session('non-openable', 'project-locked', 'Locked', {
      activity: SERVER_TIME_MS - 2_000,
      capabilities: { open: false, cancel_operation: true },
    }),
  ]);
  const openCard = card(page, 'openable');
  const lockedCard = card(page, 'non-openable');
  const openMenu = openCard.locator('.session-menu');
  const lockedMenu = lockedCard.locator('.session-menu');
  const openTrigger = openCard.locator('button[aria-label^="Actions for"]');

  await expect(openCard).toHaveAttribute('role', 'link');
  await expect(lockedCard).not.toHaveAttribute('role', 'link');
  await openTrigger.click();
  await expect(openMenu).toBeVisible();
  await expect(openMenu.getByRole('menuitem')).toHaveText(['Rename', 'Cancel operation', 'Stop session']);
  await openMenu.getByRole('menuitem', { name: 'Rename' }).focus();
  await page.keyboard.press('ArrowDown');
  expect(await page.evaluate(() => document.activeElement?.dataset.action)).toBe('cancel');
  await page.keyboard.press('ArrowUp');
  expect(await page.evaluate(() => document.activeElement?.dataset.action)).toBe('rename');
  await page.keyboard.press('Tab');
  await page.keyboard.press('Escape');
  await expect(openMenu).toBeHidden();

  // Removing the focused authority during a refresh moves focus to the first
  // remaining enabled action instead of leaving focus on a detached node.
  await openTrigger.click();
  await openMenu.getByRole('menuitem', { name: 'Rename' }).focus();
  state.snapshot.sessions.find(item => item.id === 'openable').capabilities.rename = false;
  await refresh(page, state);
  await expect(openMenu.getByRole('menuitem', { name: 'Rename' })).toHaveCount(0);
  expect(await page.evaluate(() => document.activeElement?.dataset.action)).toBe('cancel');
  await page.keyboard.press('Escape');
  await expect(openMenu).toBeHidden();
  expect(await page.evaluate(() => document.activeElement?.dataset.sessionMenu)).toBe('openable');

  await openCard.click({ button: 'right', position: { x: 16, y: 16 } });
  await expect(openMenu).toBeVisible();
  await page.keyboard.press('Escape');
  await openCard.focus();
  await page.keyboard.press('Shift+F10');
  await expect(openMenu).toBeVisible();
  await page.keyboard.press('Escape');

  // A non-openable card still exposes the operation authority published by
  // the daemon; its menu is not inferred from whether the card is a link.
  await lockedCard.locator('button[aria-label^="Actions for"]').click();
  await expect(lockedMenu.getByRole('menuitem', { name: 'Cancel operation' })).toBeVisible();
  await lockedMenu.getByRole('menuitem', { name: 'Cancel operation' }).click();
  await expect.poll(() => state.actions.filter(action => action.session_id === 'non-openable')).toHaveLength(1);
  expect(state.actions.at(-1)).toMatchObject({ action: 'cancel', session_id: 'non-openable' });

  state.failAction = { action: 'close', error: 'stop failed in fixture' };
  await openTrigger.click();
  let confirmed = false;
  page.once('dialog', async dialog => {
    confirmed = dialog.type() === 'confirm' && dialog.message().includes('Stop session?');
    await dialog.accept();
  });
  await openMenu.getByRole('menuitem', { name: 'Stop session' }).click();
  await expect(page.locator('#action-error')).toHaveText('stop failed in fixture');
  expect(confirmed).toBe(true);

  async function beginPointer(cardLocator) {
    const box = await cardLocator.boundingBox();
    await page.mouse.move(box.x + 16, box.y + box.height / 2);
    await page.mouse.down();
    return box;
  }

  // Movement beyond ten pixels cancels the timer.
  let box = await beginPointer(lockedCard);
  await page.waitForTimeout(150);
  await page.mouse.move(box.x + 28, box.y + box.height / 2);
  await page.waitForTimeout(450);
  await expect(lockedMenu).toBeHidden();
  await page.mouse.up();

  // Scrolling cancels a press even when the pointer has not moved.
  await beginPointer(lockedCard);
  await page.waitForTimeout(150);
  await page.evaluate(() => document.querySelector('#sessions').dispatchEvent(new Event('scroll')));
  await page.waitForTimeout(450);
  await expect(lockedMenu).toBeHidden();
  await page.mouse.up();

  // A platform pointer cancellation has the same result.
  await beginPointer(lockedCard);
  await page.waitForTimeout(150);
  await page.evaluate(() => document.querySelector('[data-session-id="non-openable"]').dispatchEvent(
    new PointerEvent('pointercancel', { bubbles: true, pointerId: 1 }),
  ));
  await page.waitForTimeout(450);
  await expect(lockedMenu).toBeHidden();
  await page.mouse.up();

  // A successful 500ms press opens the menu, and the click generated by the
  // pointerup is consumed instead of navigating the card.
  box = await beginPointer(openCard);
  await page.waitForTimeout(550);
  await expect(openMenu).toBeVisible();
  await page.mouse.up();
  await expect(page).toHaveURL(/#workspace\/workspace-1$/);
});
