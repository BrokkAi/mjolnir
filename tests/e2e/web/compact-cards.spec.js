const { test, expect } = require('@playwright/test');
const path = require('node:path');
const { viewerWireSnapshot, viewerDetailResponse, viewerDelta, dispatchRuntimeFrame } = require('./lab-env');

test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, serviceWorkers: 'block', timezoneId: 'UTC' });

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
    interrupt_turn: false,
    cancel_operation: false,
    suspend: false,
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
    active: options.active ?? ['live', 'starting', 'suspending'].includes(lifecycle),
    transitioning: Boolean(options.transitioning),
    created_at: options.createdAt || '2030-06-15T00:00:00Z',
    updated_at: '2030-06-15T00:00:00Z',
    last_activity_at_ms: options.activity || SERVER_TIME_MS,
    last_message_at_ms: options.message,
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
    operation: options.operation || null,
    chat_phase: 'idle',
    is_idle: options.isIdle !== false,
    retry_assessment_pending: Boolean(options.retryAssessmentPending),
    config_options: [],
    plan_mode_active: false,
    turn_review: null,
    available_commands: [],
    subagent_session_ids: options.subagentSessionIds || [],
    reviewer_session_ids: options.reviewerSessionIds || [],
    subagent_parent_id: options.subagentParentId,
    managed_checkout_kind: options.managedCheckoutKind ?? null,
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
      review_config: { trigger: "off", profile: null },
    },
    snapshots: 0,
    detailRequests: [],
    actions: [],
    conversationRequests: 0,
    conversationResponses: 0,
    conversation: { entries: [], latest_seq: 0, reset: true },
    holdConversation: false,
    releaseConversation: null,
    failAction: null,
  };
}

async function mount(page, sessions, holdDetail = null) {
  const state = stateWith(sessions);
  state.holdDetail = holdDetail;
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
      const wire = viewerWireSnapshot(state.snapshot);
      state.lastWireSnapshot = wire;
      state.streamInternedKeys = new Set(Object.keys(wire.interned));
      return json(wire);
    }
    const detailPath = pathname.match(/^\/api\/sessions\/([^/]+)\/row$/);
    if (detailPath) {
      const id = decodeURIComponent(detailPath[1]);
      state.detailRequests.push(id);
      if (state.holdDetail) await state.holdDetail;
      const detail = viewerDetailResponse(state.snapshot, id);
      return detail ? json(detail) : route.fulfill({ status: 404, contentType: 'application/json', body: JSON.stringify({ error: 'session not found' }) });
    }
    if (pathname === '/api/events') {
      return route.fulfill({
        status: 200,
        headers: { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' },
        body: ': fixture\n\n',
      });
    }
    const lifecycle = pathname.match(/^\/api\/v1\/sessions\/([^/]+)\/(suspend|destroy)$/);
    if (lifecycle) {
      const body = { action: lifecycle[2], session_id: lifecycle[1], ...route.request().postDataJSON() };
      state.actions.push(body);
      if (state.failAction?.action === body.action) return route.fulfill({ status: 422, contentType: 'application/json', body: JSON.stringify({ error: state.failAction.error }) });
      return route.fulfill({ status: 202, body: '' });
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
    if (pathname.endsWith('/client-state')) return json({ draft: '', through_event_ordinal: 0 });
    if (pathname.endsWith('/draft')) return route.fulfill({ status: 204, body: '' });
    if (pathname.startsWith('/api/conversations/')) {
      if (route.request().method() === 'GET') {
        state.conversationRequests += 1;
        if (state.holdConversation) {
          await new Promise(resolve => { state.releaseConversation = resolve; });
        }
      }
      state.conversationResponses += 1;
      return json(state.conversation);
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
  state.snapshot.revision += 1;
  const update = viewerDelta(state.lastWireSnapshot, state.snapshot, state.streamInternedKeys);
  state.lastWireSnapshot = update.wire;
  state.streamInternedKeys = update.knownInternedKeys;
  await dispatchRuntimeFrame(page, update.frame);
  await renderAfterFrame(page);
}

async function reconnect(page, state) {
  state.snapshot.revision += 1;
  const update = viewerDelta(state.lastWireSnapshot, state.snapshot, state.streamInternedKeys);
  state.lastWireSnapshot = update.wire;
  state.streamInternedKeys = update.knownInternedKeys;
  await page.evaluate(() => {
    window.fixtureEvents.dispatchEvent(new Event('error'));
    window.dispatchEvent(new Event('online'));
  });
  await dispatchRuntimeFrame(page, update.frame);
  await renderAfterFrame(page);
}

function card(page, id) {
  return page.locator(`#sessions .session[data-session-id="${id}"]`);
}

function activity(kind, fields = {}) {
  return { kind, ...fields };
}

function transcript(text) {
  return {
    entries: [{
      id: 1,
      updated_seq: 1,
      role: 'agent',
      label: 'Agent',
      recorded_at_ms: SERVER_TIME_MS,
      lines: [text],
      glyph: '●',
      tone: 'agent',
      tool_status: null,
      diffstats: [],
    }],
    latest_seq: 1,
    reset: true,
  };
}

async function captureGoldenState(output, page, label, extra = null) {
  const viewport = page.viewportSize();
  output.push(`=== ${label} (${viewport.width}x${viewport.height}) ===`);
  output.push((await page.locator('#app').innerText()).trim());
  if (extra !== null) output.push(`layout: ${JSON.stringify(extra)}`);
}

async function fixGoldenTime(page) {
  await page.clock.install({ time: new Date(SERVER_TIME_MS) });
}

// Hard-won: b72adcc6f: an idle session lost its genuine pending response check.
test('golden_viewer_dashboard', async ({ context }) => {
  const { assertGolden } = await import('./golden.mjs');
  const output = [];
  const page = await context.newPage();
  await fixGoldenTime(page);
  const responsiveState = await mount(page, [
    ...Array.from({ length: 6 }, (_, index) => session(`session-${index}`, 'mjolnir', 'Mjolnir', {
      title: `Session ${index}: investigate the desktop layout and validate responsive behavior`,
      capabilities: { rename: true },
    })),
    session('other-project', 'other', 'Other project', { activity: SERVER_TIME_MS - 60_000 }),
  ]);
  for (const width of [390, 900, 1024, 1440, 1920, 2560, 390]) {
    await page.setViewportSize({ width, height: 900 });
    const geometry = await page.evaluate(() => {
      const first = document.querySelector('[data-session-id="session-0"]').getBoundingClientRect();
      const second = document.querySelector('[data-session-id="session-1"]').getBoundingClientRect();
      const app = document.querySelector('#app').getBoundingClientRect();
      return {
        appWidth: Math.round(app.width),
        first: { x: Math.round(first.x), y: Math.round(first.y), width: Math.round(first.width), height: Math.round(first.height) },
        second: { x: Math.round(second.x), y: Math.round(second.y) },
        documentWidth: document.documentElement.scrollWidth,
      };
    });
    if (width === 1440) {
      await card(page, 'session-1').getByRole('button', { name: /actions/i }).click();
      await captureGoldenState(output, page, 'dashboard desktop actions open', geometry);
      await page.keyboard.press('Escape');
    } else {
      await captureGoldenState(output, page, `dashboard responsive ${width}`, geometry);
    }
  }
  await page.close();

  const compactPage = await context.newPage();
  await fixGoldenTime(compactPage);
  await mount(compactPage, [
    session('beta-turn', 'project-beta', 'Beta', {
      activity: SERVER_TIME_MS - 1_000,
      title: 'A long session title that occupies one ellipsized line on a narrow phone screen',
      displayLocation: '/work/attention',
      profileId: 'codex',
      hasError: true,
      pendingElicitations: [{ id: 'input-1' }],
      queued: 2,
      activityDetails: activity('turn', { turn_started_at_ms: SERVER_TIME_MS - 65_000, step_started_at_ms: SERVER_TIME_MS - 65_000 }),
      isIdle: false,
    }),
    session('beta-step', 'project-beta', 'Beta', { activity: SERVER_TIME_MS - 2_000, activityDetails: activity('step', { step_started_at_ms: SERVER_TIME_MS - 3_600_000 }), isIdle: false }),
    session('alpha-background', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS - 3_000, activityDetails: activity('background', { background_started_at_ms: SERVER_TIME_MS - 65_000, label: 'Indexing' }), isIdle: false }),
    session('alpha-idle', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS - 4_000, activityDetails: activity('idle', { idle_since_ms: SERVER_TIME_MS - 65_000 }) }),
    session('alpha-yesterday', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS - 4_500, activityDetails: activity('idle', { idle_since_ms: SERVER_TIME_MS - 86_465_000 }) }),
    session('lifecycle', 'project-lifecycle', 'Lifecycle', { activity: SERVER_TIME_MS - 5_000, activityDetails: activity('lifecycle', { label: 'Starting target' }), isIdle: false }),
    session('checking-response', 'project-checking', 'Checking', { activity: SERVER_TIME_MS - 5_500, activityDetails: activity('idle', { idle_since_ms: SERVER_TIME_MS - 65_000 }), retryAssessmentPending: true }),
    session('unknown-idle', 'project-unknown', 'Unknown', { activity: SERVER_TIME_MS - 6_000, activityDetails: activity('idle') }),
  ]);
  await captureGoldenState(output, compactPage, 'compact cards with attention and activity clocks', await compactPage.evaluate(() => ({
    documentWidth: document.documentElement.scrollWidth,
    viewportWidth: document.documentElement.clientWidth,
    cards: [...document.querySelectorAll('#sessions .session')].map(node => ({
      title: node.querySelector('h3')?.textContent,
      attention: [...node.querySelectorAll('.session-attention-item')].map(item => ({
        text: item.textContent,
        label: item.getAttribute('aria-label'),
      })),
      meta: [...node.querySelectorAll('.session-project, .session-location, .session-profile')].map(item => item.textContent),
      activity: node.querySelector('.session-activity')?.textContent,
      titleStyle: (() => {
        const title = node.querySelector('h3');
        const style = getComputedStyle(title);
        return { height: +title.getBoundingClientRect().height.toFixed(2), lineHeight: style.lineHeight, whiteSpace: style.whiteSpace };
      })(),
    })),
  })));
  await compactPage.close();

  const orderedPage = await context.newPage();
  await fixGoldenTime(orderedPage);
  const orderedState = await mount(orderedPage, [
    session('gamma-message', 'project-gamma', 'Gamma', { message: SERVER_TIME_MS + 3_000, activity: SERVER_TIME_MS + 9_000 }),
    session('alpha-created', 'project-alpha', 'Alpha', { createdAt: '2030-06-14T00:00:00Z' }),
    session('stopped', 'project-stopped', 'Stopped', { lifecycle: 'suspended', message: SERVER_TIME_MS + 20_000 }),
    session('beta-activity', 'project-beta', 'Beta', { activity: SERVER_TIME_MS + 5_000 }),
    session('echo-tie', 'project-echo', 'Echo', { activity: SERVER_TIME_MS + 1_000 }),
    session('delta-tie', 'project-delta', 'Delta', { activity: SERVER_TIME_MS + 1_000 }),
    session('other-workspace', 'project-other', 'Other', { workspaceId: OTHER_WORKSPACE_ID, message: SERVER_TIME_MS + 30_000 }),
  ]);
  await captureGoldenState(output, orderedPage, 'flat live dashboard sorted by latest message', await orderedPage.evaluate(() => ({
    cards: [...document.querySelectorAll('#sessions .session')].map(node => ({
      title: node.querySelector('h3')?.textContent,
      meta: node.querySelector('.session-meta')?.innerText,
    })),
  })));
  orderedState.snapshot.targets = [
    { id: 'container', default_candidate: true, runtime_missing: false },
    { id: 'configured-remote', default_candidate: false, runtime_missing: true },
    { id: 'missing-local', default_candidate: true, runtime_missing: true },
  ];
  orderedState.snapshot.capacity = [{
    id: 'local-host',
    label: 'Local workstation',
    target_ids: ['container', 'configured-remote', 'missing-local'],
    sampled_at_epoch_seconds: SERVER_TIME_MS / 1000,
    cpu_percent: 32,
    memory_total_bytes: 16 * 1024 ** 3,
    memory_used_bytes: 8 * 1024 ** 3,
    logical_cores: 8,
    disk_total_bytes: 512 * 1024 ** 3,
    storage: [],
  }];
  await refresh(orderedPage, orderedState);
  await orderedPage.getByRole('button', { name: 'Menu' }).click();
  await orderedPage.getByRole('menuitem', { name: 'Targets' }).click();
  await expect(orderedPage.locator('#targets-page')).toBeVisible();
  await expect(orderedPage.locator('#targets')).toContainText('container, configured-remote');
  await expect(orderedPage.locator('#targets')).not.toContainText('missing-local');
  await captureGoldenState(output, orderedPage, 'target capacity omits a missing default candidate', {
    listedTargets: await orderedPage.locator('#targets article p.dim').first().innerText(),
  });
  await orderedPage.close();

  const insertionPage = await context.newPage();
  await fixGoldenTime(insertionPage);
  const insertionState = await mount(insertionPage, [
    session('alpha-first', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS - 1_000 }),
    session('alpha-second', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS - 2_000 }),
    session('beta', 'project-beta', 'Beta', { activity: SERVER_TIME_MS - 3_000 }),
  ]);
  const captureTitles = async label => captureGoldenState(output, insertionPage, label, await insertionPage.locator('#sessions .session h3').allTextContents());
  await captureTitles('initial activity order');
  insertionState.snapshot.sessions.push(
    session('alpha-late', 'project-alpha', 'Alpha', { activity: SERVER_TIME_MS - 500 }),
    session('gamma-late', 'project-gamma', 'Gamma', { activity: SERVER_TIME_MS - 1_500 }),
  );
  await refresh(insertionPage, insertionState);
  await captureTitles('later sessions inserted in order');
  await card(insertionPage, 'gamma-late').focus();
  insertionState.snapshot.sessions = insertionState.snapshot.sessions.filter(item => item.id !== 'beta');
  await refresh(insertionPage, insertionState);
  insertionState.snapshot.sessions.push(session('beta', 'project-beta', 'Beta', { activity: SERVER_TIME_MS + 1_000 }));
  await refresh(insertionPage, insertionState);
  await captureGoldenState(output, insertionPage, 'reappearing session re-sorted while focus stays put', {
    titles: await insertionPage.locator('#sessions .session h3').allTextContents(),
    focused: await insertionPage.evaluate(() => document.activeElement?.dataset.sessionId || document.activeElement?.closest('[data-session-id]')?.dataset.sessionId || null),
  });
  await insertionPage.close();

  const reloadPage = await context.newPage();
  await fixGoldenTime(reloadPage);
  const reloadState = await mount(reloadPage, [
    session('first', 'project-first', 'First', { activity: SERVER_TIME_MS - 1_000 }),
    session('second', 'project-second', 'Second', { activity: SERVER_TIME_MS - 2_000 }),
  ]);
  reloadState.snapshot.sessions.find(item => item.id === 'first').last_activity_at_ms = SERVER_TIME_MS - 4_000;
  reloadState.snapshot.sessions.find(item => item.id === 'second').last_activity_at_ms = SERVER_TIME_MS - 100;
  await reloadPage.reload();
  await expect.poll(() => reloadPage.locator('#sessions .session h3').allTextContents()).toEqual(['second', 'first']);
  await captureGoldenState(output, reloadPage, 'reload uses latest activity order', await reloadPage.locator('#sessions .session h3').allTextContents());
  await reloadPage.close();

  const desktopPage = await context.newPage();
  await fixGoldenTime(desktopPage);
  await desktopPage.setViewportSize({ width: 1440, height: 900 });
  const desktopState = await mount(desktopPage, [session('desktop', 'mjolnir', 'Mjolnir', {
    queued: 20,
    capabilities: { prompt: true },
  })]);
  desktopState.conversation = transcript('A long transcript paragraph to exercise scrolling.\n\n'.repeat(100));
  await card(desktopPage, 'desktop').click();
  await desktopPage.locator('#conversation-side > summary').click();
  await desktopPage.locator('#prompt-text').fill('Keep this draft while resizing the window.');
  for (const size of [{ width: 1440, height: 900 }, { width: 1920, height: 1080 }, { width: 1024, height: 768 }]) {
    await desktopPage.setViewportSize(size);
    await desktopPage.evaluate(() => { document.body.dataset.connection = 'offline'; });
    const geometry = await desktopPage.evaluate(() => {
      const bounds = selector => {
        const box = document.querySelector(selector).getBoundingClientRect();
        return { x: +box.x.toFixed(2), y: +box.y.toFixed(2), width: +box.width.toFixed(2), height: +box.height.toFixed(2) };
      };
      const scroll = document.querySelector('#conversation-scroll');
      const before = { header: bounds('#shell-header'), composer: bounds('#prompt-form') };
      scroll.scrollTop = scroll.scrollHeight;
      const after = { header: bounds('#shell-header'), composer: bounds('#prompt-form') };
      return {
        header: after.header,
        composer: after.composer,
        conversation: bounds('#conversation'),
        scroll: { top: scroll.scrollTop, height: scroll.clientHeight, scrollHeight: scroll.scrollHeight },
        fixedWhileScrolling: {
          header: JSON.stringify(before.header) === JSON.stringify(after.header),
          composer: JSON.stringify(before.composer) === JSON.stringify(after.composer),
        },
        draft: document.querySelector('#prompt-text').textContent,
        documentHeight: document.documentElement.scrollHeight,
      };
    });
    expect(geometry.scroll.top).toBeGreaterThan(0);
    expect(geometry.fixedWhileScrolling).toEqual({ header: true, composer: true });
    await captureGoldenState(output, desktopPage, 'desktop conversation with fixed header and composer', geometry);
  }
  await desktopPage.close();

  const checkpointPage = await context.newPage();
  await fixGoldenTime(checkpointPage);
  const checkpointAt = Math.floor((SERVER_TIME_MS - 30_000) / 1_000);
  const checkpointState = await mount(checkpointPage, [session('checkpoint', 'project-checkpoint', 'Checkpoint', {
    operation: { id: 'checkpoint-1', session_id: 'checkpoint', kind: 'checkpoint', started_at_epoch_seconds: checkpointAt, stages: [{ label: 'Checkpointing', started_at_epoch_seconds: checkpointAt }], notice: null, cancellable: false },
  })]);
  checkpointState.conversation = transcript('checkpoint conversation remains readable');
  await card(checkpointPage, 'checkpoint').click();
  await expect(checkpointPage.locator('#conversation')).toContainText('checkpoint conversation remains readable');
  await captureGoldenState(output, checkpointPage, 'ordinary checkpoint keeps conversation and composer readable');
  await checkpointPage.close();

  const subagentPage = await context.newPage();
  await fixGoldenTime(subagentPage);
  await mount(subagentPage, [
    session('parent', 'project-parent', 'Parent project', { title: 'Parent session', subagentSessionIds: ['child-one', 'child-two'] }),
    session('child-one', 'project-parent', 'Parent project', { title: 'Grok helper', subagentParentId: 'parent' }),
    session('child-two', 'project-parent', 'Parent project', { title: 'Muse helper', subagentParentId: 'parent' }),
  ]);
  await captureGoldenState(output, subagentPage, 'parent dashboard hides child sessions');
  await card(subagentPage, 'parent').click();
  await expect(subagentPage).toHaveURL(/#conversation\/parent$/);
  await subagentPage.locator('#subagents-button').click();
  await expect(subagentPage).toHaveURL(/#subagents\/parent$/);
  await expect(subagentPage.locator('#workspaces .virtual-workspace')).toContainText('Parent session');
  await expect(subagentPage.locator('#sessions')).toContainText('All 2 sub-agents are idle. Show idle to view them.');
  await expect(card(subagentPage, 'child-one')).toHaveCount(0);
  await subagentPage.getByRole('button', { name: 'Show idle' }).click();
  await expect(card(subagentPage, 'child-one')).toBeVisible();
  await captureGoldenState(output, subagentPage, 'named subagent workspace lists child sessions');
  await card(subagentPage, 'child-one').click();
  await expect(subagentPage).toHaveURL(/#subagents\/parent\/child-one$/);
  await subagentPage.locator('#back').click();
  await expect(subagentPage).toHaveURL(/#subagents\/parent$/);
  await subagentPage.getByRole('button', { name: 'Close Parent session sub-agent workspace' }).click();
  await expect(subagentPage).toHaveURL(/#conversation\/parent$/);
  await expect(subagentPage.locator('#conversation-title')).toContainText('Parent session');
  await captureGoldenState(output, subagentPage, 'closing subagent workspace returns to parent conversation');
  await subagentPage.close();

  assertGolden('viewer_dashboard', output.join('\n'));
});

// Hard-won: 6eb3146b8: an idle child with review work was hidden from its parent workspace.
test('child workspace hides idle history while preserving active work and an opened conversation', async ({ page }) => {
  const parent = session('parent', 'project', 'Project', {
    title: 'Parent session',
    subagentSessionIds: ['idle', 'working', 'blocked', 'failed', 'unknown', 'reviewing'],
  });
  parent.native_subagents = [
    { session_id: 'native-completed', name: 'Completed helper', state: 'completed', availability: 'available' },
    { session_id: 'native-cancelled', name: 'Cancelled helper', state: 'cancelled', availability: 'available' },
    { session_id: 'native-running', name: 'Running helper', state: 'running', availability: 'available' },
    { session_id: 'native-failed', name: 'Failed helper', state: 'failed', availability: 'available' },
    { session_id: 'native-disconnected', name: 'Disconnected helper', state: 'disconnected', availability: 'available' },
    { session_id: 'native-unknown', name: 'Unknown helper', state: 'completed', availability: 'unknown' },
  ];
  const child = (id, options = {}) => session(id, 'project', 'Project', { subagentParentId: 'parent', ...options });
  const unknown = child('unknown', { activityDetails: { kind: 'unknown' } });
  delete unknown.is_idle;
  const reviewing = child('reviewing');
  reviewing.turn_review = { status: 'Preparing reviewer' };
  const state = await mount(page, [
    parent,
    child('idle'),
    child('working', { isIdle: false, activityDetails: { kind: 'turn' } }),
    child('blocked', { pendingElicitations: [{ id: 'question', question: 'Need input' }] }),
    child('failed', { hasError: true }),
    unknown,
    reviewing,
  ]);
  await expect(card(page, 'parent')).toBeVisible();
  await card(page, 'parent').click();
  await page.locator('#subagents-button').click();
  await expect(page).toHaveURL(/#subagents\/parent$/);
  await expect(page.locator('#sessions')).toContainText('3 idle sub-agents hidden');
  await expect(card(page, 'idle')).toHaveCount(0);
  for (const id of ['working', 'blocked', 'failed', 'unknown', 'reviewing']) await expect(card(page, id)).toBeVisible();
  for (const name of ['Running helper', 'Failed helper', 'Disconnected helper', 'Unknown helper']) {
    await expect(page.locator('#sessions')).toContainText(name);
  }
  for (const name of ['Completed helper', 'Cancelled helper']) {
    await expect(page.locator('#sessions')).not.toContainText(name);
  }

  // This is the same published verdict shape used by the turn-review browser tests.
  reviewing.turn_review = {
    status: 'Findings sent to the agent',
    roles: [],
    verdict: { text: '[P1] a concrete finding' },
  };
  await refresh(page, state);
  await expect(card(page, 'reviewing')).toBeVisible();
  reviewing.turn_review = null;
  await refresh(page, state);
  await expect(card(page, 'reviewing')).toHaveCount(0);

  await page.getByRole('button', { name: 'Show idle' }).click();
  await expect(card(page, 'idle')).toBeVisible();
  for (const name of ['Completed helper', 'Cancelled helper']) await expect(page.locator('#sessions')).toContainText(name);
  await card(page, 'idle').click();
  await expect(page).toHaveURL(/#subagents\/parent\/idle$/);
  await page.locator('#back').click();
  await expect(page.getByRole('button', { name: 'Hide idle' })).toHaveAttribute('aria-pressed', 'true');
  await expect(card(page, 'idle')).toBeVisible();
  await page.reload();
  await expect(page.getByRole('button', { name: 'Hide idle' })).toHaveAttribute('aria-pressed', 'true');
  await expect(card(page, 'idle')).toBeVisible();
  await page.getByRole('button', { name: 'Hide idle' }).click();
  await expect(card(page, 'idle')).toHaveCount(0);
  await page.evaluate(() => { location.hash = '#subagents/parent/idle'; });
  await expect(page).toHaveURL(/#subagents\/parent\/idle$/);
  await expect(page.locator('#conversation-title')).toContainText('idle');
  await expect(page.locator('#conversation')).toBeVisible();
  await page.locator('#back').click();
  await expect(page).toHaveURL(/#subagents\/parent$/);
  await expect(card(page, 'idle')).toHaveCount(0);
});

test('virtual subagent workspace fetches a summary child row before rendering its card', async ({ page }) => {
  let releaseDetail;
  const heldDetail = new Promise(resolve => { releaseDetail = resolve; });
  const parent = session('parent', 'project', 'Project', { subagentSessionIds: ['child'] });
  const child = session('child', 'project', 'Project', {
    title: 'Stopped helper',
    lifecycle: 'suspended',
    subagentParentId: 'parent',
    capabilities: { open: false, resume: true },
  });
  child.detail = false;
  const state = await mount(page, [parent, child], heldDetail);
  await card(page, 'parent').click();
  await expect(page).toHaveURL(/#conversation\/parent$/);
  await page.locator('#subagents-button').click();
  await expect(page).toHaveURL(/#subagents\/parent$/);
  await page.getByRole('button', { name: 'Show idle', exact: true }).click();
  await expect(page.locator('#sessions')).toContainText('Loading session details…');
  await expect.poll(() => state.detailRequests).toEqual(['child']);
  releaseDetail();
  await expect(card(page, 'child')).toBeVisible();
  await expect(card(page, 'child')).not.toContainText('Loading session details');
});

test('a direct conversation link fetches a summary child before checking whether it can open', async ({ page }) => {
  let releaseDetail;
  const heldDetail = new Promise(resolve => { releaseDetail = resolve; });
  const parent = session('parent', 'project', 'Project', { subagentSessionIds: ['child'] });
  const child = session('child', 'project', 'Project', {
    title: 'Stopped helper',
    lifecycle: 'suspended',
    subagentParentId: 'parent',
    capabilities: { open: false, resume: true },
  });
  child.detail = false;
  const state = await mount(page, [parent, child], heldDetail);
  await page.evaluate(() => { location.hash = '#subagents/parent/child'; });
  await expect(page).toHaveURL(/#subagents\/parent\/child$/);
  await expect(page.locator('#conversation-error')).toHaveText('Loading session details…');
  await expect.poll(() => state.detailRequests).toEqual(['child']);
  releaseDetail();
  await expect(page).toHaveURL(/#subagents\/parent$/);
  expect(state.conversationRequests).toBe(0);
});

test('virtual subagent workspace fetches a summary parent before reading native history', async ({ page }) => {
  let releaseDetail;
  const heldDetail = new Promise(resolve => { releaseDetail = resolve; });
  const parent = session('parent', 'project', 'Project');
  parent.native_subagents = [{ stable_id: 'helper', name: 'Retained helper', state: 'completed', availability: 'unknown' }];
  const state = await mount(page, [parent], heldDetail);
  Object.assign(parent, {
    detail: false,
    lifecycle: 'suspended',
    state: 'suspended',
    capabilities: { ...parent.capabilities, open: false },
    conversation_available: false,
  });
  await refresh(page, state);
  await page.evaluate(() => { location.hash = '#subagents/parent'; });
  await expect(page).toHaveURL(/#subagents\/parent$/);
  await expect(page.locator('#sessions')).toContainText('Loading session details…');
  await expect.poll(() => state.detailRequests).toEqual(['parent']);
  releaseDetail();
  await expect(page.locator('#sessions')).toContainText('Retained helper');
  await expect(page.getByRole('button', { name: 'View history' })).toBeVisible();
});

test('transition cards show compact stages and suppress a late transcript response', async ({ page }) => {
  const state = await mount(page, [
    session('suspending', 'project-transition', 'Transition', {
      capabilities: { open: true },
    }),
    session('other-live', 'project-other', 'Other'),
  ]);
  state.conversation = transcript('old conversation must not return');
  state.holdConversation = true;

  await card(page, 'suspending').click();
  await expect(page).toHaveURL(/#conversation\/suspending$/);
  await expect.poll(() => state.conversationRequests).toBe(1);

  const stopping = state.snapshot.sessions.find(item => item.id === 'suspending');
  Object.assign(stopping, {
    lifecycle: 'suspending',
    state: 'closing',
    transitioning: true,
    operation: {
      id: 'stop-operation-1',
      session_id: 'suspending',
      kind: 'suspend',
      started_at_epoch_seconds: Math.floor((SERVER_TIME_MS - 60_000) / 1_000),
      stages: [
        {
          label: 'Stop target',
          started_at_epoch_seconds: Math.floor((SERVER_TIME_MS - 60_000) / 1_000),
        },
        {
          label: 'Remove storage',
          started_at_epoch_seconds: Math.floor((SERVER_TIME_MS - 30_000) / 1_000),
        },
      ],
      notice: null,
      cancellable: true,
    },
    capabilities: {
      ...stopping.capabilities,
      open: false,
      cancel_operation: true,
      rename: false,
      suspend: false,
    },
  });
  await refresh(page, state);

  await expect(page.locator('#conversation-transition')).toBeVisible();
  await expect(page.locator('#conversation-transition-stage')).toHaveText(/Stop target · Remove storage 1m\d\ds/);
  const firstStageClock = await page.locator('#conversation-transition-stage').textContent();
  await expect.poll(
    () => page.locator('#conversation-transition-stage').textContent(),
    { timeout: 5_000 },
  ).not.toBe(firstStageClock);
  await expect(page.locator('#conversation-scroll')).toBeHidden();
  await expect(page.locator('#prompt-form')).toBeHidden();
  await expect(card(page, 'other-live')).toHaveCount(1);

  await page.setViewportSize({ width: 1280, height: 900 });
  await expect(page.locator('#conversation-transition')).toBeVisible();
  await expect(page.locator('#conversation-scroll')).toBeHidden();
  await expect(page.locator('#prompt-form')).toBeHidden();

  state.releaseConversation();
  await expect.poll(() => state.conversationResponses).toBe(1);
  await expect(page.locator('#conversation-feed')).not.toContainText('old conversation must not return');
  await expect(page.locator('#conversation-transition')).toBeVisible();
  await page.locator('#back').click();
  await expect(card(page, 'suspending').locator('.session-activity')).toHaveText(/Stop target · Remove storage/);
});

test('a live route survives transition completion while its conversation projection catches up', async ({ page }) => {
  const state = await mount(page, [
    session('created', 'project-created', 'Created', {
      capabilities: { open: true },
    }),
  ]);
  state.conversation = transcript('old conversation must not return');
  state.holdConversation = true;

  await card(page, 'created').click();
  await expect(page).toHaveURL(/#conversation\/created$/);
  await expect.poll(() => state.conversationRequests).toBe(1);

  const created = state.snapshot.sessions.find(item => item.id === 'created');
  Object.assign(created, {
    lifecycle: 'starting',
    state: 'provisioning',
    transitioning: true,
    operation: {
      id: 'create-operation-1',
      session_id: 'created',
      kind: 'create',
      started_at_epoch_seconds: Math.floor((SERVER_TIME_MS - 60_000) / 1_000),
      stages: [{
        label: 'Starting target',
        started_at_epoch_seconds: Math.floor((SERVER_TIME_MS - 60_000) / 1_000),
      }],
      notice: null,
      cancellable: false,
    },
    capabilities: {
      ...created.capabilities,
      open: false,
      cancel_operation: false,
    },
  });
  await refresh(page, state);
  await expect(page.locator('#conversation-transition')).toBeVisible();
  await expect(page.locator('#conversation-scroll')).toBeHidden();
  await expect(page.locator('#prompt-form')).toBeHidden();

  // The daemon has finished the lifecycle operation, but its conversation
  // projection has not caught up yet. Keep the selected route and show the
  // loading panel instead of falling back to the dashboard.
  Object.assign(created, {
    lifecycle: 'live',
    state: 'running',
    transitioning: false,
    operation: null,
    capabilities: {
      ...created.capabilities,
      open: false,
      cancel_operation: false,
    },
  });
  await refresh(page, state);
  await expect(page).toHaveURL(/#conversation\/created$/);
  await expect(page.locator('#conversation-transition-title')).toHaveText('Loading conversation');
  await expect(page.locator('#conversation-scroll')).toBeHidden();
  await expect(page.locator('#prompt-form')).toBeHidden();
  await expect(page.locator('#conversation-feed')).not.toContainText('old conversation must not return');

  // Once the ready projection appears, the stale held request is retired and
  // only a fresh transcript is painted.
  state.conversation = transcript('fresh ready conversation');
  state.holdConversation = false;
  Object.assign(created, {
    capabilities: {
      ...created.capabilities,
      open: true,
      prompt: true,
    },
  });
  await refresh(page, state);
  state.releaseConversation?.();
  await expect.poll(() => state.conversationResponses).toBeGreaterThan(1);
  await expect(page.locator('#conversation-feed')).toContainText('fresh ready conversation');
  await expect(page.locator('#conversation-feed')).not.toContainText('old conversation must not return');
  await expect(page.locator('#conversation-transition')).toBeHidden();
  await expect(page.locator('#conversation-scroll')).toBeVisible();
  await expect(page.locator('#prompt-form')).toBeVisible();
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
  await expect(page.locator('#sessions .session h3')).toHaveText(['alpha-new', 'alpha-old', 'beta']);

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

  await page.getByRole('combobox', { name: 'Workspace' }).selectOption({ label: 'Other workspace' });
  await expect(page).toHaveURL(/#workspace\/workspace-2$/);
  await expect(card(page, 'other')).toBeVisible();
  await page.getByRole('combobox', { name: 'Workspace' }).selectOption({ label: 'Browser tests' });
  await expect(page).toHaveURL(/#workspace\/workspace-1$/);
  await expect(alphaCard).toBeVisible();
  expect(await original.evaluate(node => node.isConnected)).toBe(true);
  await expect(page.locator('#sessions .session h3')).toHaveText(['alpha-new', 'alpha-old', 'beta']);

  state.snapshot.sessions.find(item => item.id === 'beta').last_activity_at_ms = SERVER_TIME_MS - 10;
  await reconnect(page, state);
  // The list re-sorts live as newer activity arrives; identity survives it.
  await expect(page.locator('#sessions .session h3')).toHaveText(['beta', 'alpha-new', 'alpha-old']);
  expect(await original.evaluate(node => node.isConnected)).toBe(true);
});

test('menus follow capabilities, long press cancellation, right click, keyboard, confirmation, and errors', async ({ page }) => {
  const state = await mount(page, [
    session('openable', 'project-menu', 'Menu', {
      activity: SERVER_TIME_MS - 1_000,
      capabilities: { rename: true, cancel_operation: true, suspend: true },
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
  await expect(openMenu.getByRole('menuitem')).toHaveText(['Changed files…', 'Rename', 'Copy session ID', 'Cancel operation', 'Suspend session…']);
  await openMenu.getByRole('menuitem', { name: 'Rename' }).focus();
  await page.keyboard.press('ArrowDown');
  expect(await page.evaluate(() => document.activeElement?.dataset.action)).toBe('copy-session-id');
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
  expect(await page.evaluate(() => document.activeElement?.dataset.action)).toBe('changed-files');
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

  state.failAction = { action: 'suspend', error: 'stop failed in fixture' };
  await openTrigger.click();
  let confirmed = false;
  page.once('dialog', async dialog => {
    confirmed = dialog.type() === 'confirm' && dialog.message().includes('Suspend session?');
    await dialog.accept();
  });
  await openMenu.getByRole('menuitem', { name: 'Suspend session…' }).click();
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

test('composer steering survives reconnect and Escape never confirms cancellation', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 740 });
  const parent = session('steering', 'project', 'Project', {
    queued: 1, capabilities: { prompt: true, interrupt_turn: true },
  });
  parent.targeted_turn_control_supported = true;
  parent.active_prompt_id = 'turn-one'; parent.chat_phase = 'running';
  parent.turn_controls = { status: 'ready', primary: { kind: 'steer', command: { type: 'steer', data: { active_prompt_id: 'turn-one', queued_prompt_id: 'queued-steering-0' } } }, interrupt: { type: 'cancel_turn_for', data: { active_prompt_id: 'turn-one' } } };
  const state = await mount(page, [parent]);
  await card(page, 'steering').click();
  const composer = page.locator('#prompt-text');
  await composer.fill('keep this draft');
  await composer.press('Escape');
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0]).toEqual({ action: 'turn-control', session_id: 'steering', command: {
    type: 'steer', data: { active_prompt_id: 'turn-one', queued_prompt_id: 'queued-steering-0' },
  } });
  parent.turn_controls.status = 'pending'; parent.turn_controls.primary = null;
  parent.steering = { command_id: 'steer-one', active_prompt_id: 'turn-one', queued_prompt_id: 'queued-steering-0', status: 'pending' };
  await refresh(page, state);
  await expect(page.locator('#cancel-turn')).toHaveText('Steering…');
  await composer.press('Escape');
  expect(state.actions).toHaveLength(1);
  await reconnect(page, state);
  await expect(page.locator('#cancel-turn')).toBeDisabled();
  parent.turn_controls.status = 'ready';
  parent.steering.status = 'failed'; parent.steering.message = 'Adapter refused steering';
  await refresh(page, state);
  const cancel = page.locator('#turn-control-status').getByRole('button', { name: 'Interrupt', exact: true });
  await expect(cancel).toBeVisible();
  await composer.press('Escape');
  await expect(cancel).toHaveCount(0);
  expect(state.actions).toHaveLength(1);
  // A new failure offers a fresh, deliberate choice.
  parent.steering.command_id = 'steer-two';
  await refresh(page, state);
  await cancel.click();
  await expect.poll(() => state.actions.length).toBe(2);
  expect(state.actions[1].command).toEqual({ type: 'cancel_turn_for', data: { active_prompt_id: 'turn-one' } });
  await expect(composer).toHaveText('keep this draft');
});

test('a moved parent keeps 23 native histories without claiming any are working', async ({ page }) => {
  const parent = session('native-parent', 'project', 'Project');
  parent.native_subagents = Array.from({ length: 23 }, (_, i) => ({
    session_id: `child-${i}`, name: `Helper ${i}`, state: i < 17 ? 'completed' : 'disconnected', availability: 'unknown',
  }));
  const state = await mount(page, [parent]);
  await card(page, 'native-parent').click();
  await expect(page.locator('#subagents-button')).toHaveText('Subagents · 0');
  parent.profile_id = 'another-profile';
  await reconnect(page, state);
  await expect(page.locator('#subagents-button')).toHaveText('Subagents · 0');
  parent.native_subagents[0].state = 'running';
  await refresh(page, state);
  await expect(page.locator('#subagents-button')).toHaveText('Subagents · 1');
  await expect(page.locator('#subagents-button-desktop')).toHaveText('Subagents · 1');
  parent.native_subagents[0].state = 'completed';
  await refresh(page, state);
  await expect(page.locator('#subagents-button')).toHaveText('Subagents · 0');
  await page.locator('#subagents-button').click();
  await expect(page.getByRole('button', { name: 'View history' })).toHaveCount(23);
  await expect(page.getByRole('heading', { name: 'History and availability unknown' })).toBeVisible();
});

test('suspension remains pending after acceptance and exposes failure after reconnect', async ({ page }) => {
  const state = await mount(page, [session('suspend-me', 'project', 'Project', { capabilities: { suspend: true, destroy: true } })]);
  const current = card(page, 'suspend-me');
  await current.locator('button[aria-label^="Actions for"]').click();
  page.once('dialog', dialog => dialog.dismiss());
  await current.getByRole('menuitem', { name: 'Suspend session…' }).click();
  expect(state.actions).toHaveLength(0);
  await current.locator('button[aria-label^="Actions for"]').click();
  page.once('dialog', dialog => dialog.accept());
  await current.getByRole('menuitem', { name: 'Suspend session…' }).click();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0]).toEqual({ action: 'suspend', session_id: 'suspend-me', acknowledge_active_subagents: true, acknowledge_unpublished_work: true });
  await expect(current.locator('.session-activity')).toHaveText('Suspending…');
  await current.locator('button[aria-label^="Actions for"]').click();
  await expect(current.getByRole('menuitem', { name: 'Suspend session…' })).toBeDisabled();
  const saved = state.snapshot.sessions[0];
  saved.launch_error = 'the suspension did not finish: checkpoint unavailable';
  saved.has_error = true;
  await reconnect(page, state);
  await expect(current.locator('.session-activity')).toHaveText(saved.launch_error);
  await page.reload();
  await expect(current.locator('.session-activity')).toHaveText(saved.launch_error);
});

test('destroy is separately confirmed and defaults to keeping the branch', async ({ page }) => {
  const state = await mount(page, [session('destroy-me', 'project', 'Project', { managedCheckoutKind: 'worktree', capabilities: { suspend: true, destroy: true } })]);
  const current = card(page, 'destroy-me');
  await current.locator('button[aria-label^="Actions for"]').click();
  await current.getByRole('menuitem', { name: 'Destroy session…' }).click();
  const dialog = page.getByRole('dialog', { name: 'Destroy session' });
  await expect(dialog).toContainText('Work held only in the environment will be lost');
  await expect(dialog.getByRole('checkbox')).not.toBeChecked();
  await dialog.getByRole('button', { name: 'Cancel', exact: true }).click();
  expect(state.actions).toHaveLength(0);
  await current.locator('button[aria-label^="Actions for"]').click();
  await current.getByRole('menuitem', { name: 'Destroy session…' }).click();
  await dialog.getByRole('button', { name: 'Destroy session', exact: true }).click();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0]).toEqual({ action: 'destroy', session_id: 'destroy-me', delete_branch: false });
  await expect(current.locator('.session-activity')).toHaveText('Destroying…');
  state.snapshot.sessions = [];
  await refresh(page, state);
  await expect(current).toHaveCount(0);
});

test('destroying retained history offers explicit branch deletion', async ({ page }) => {
  const state = await mount(page, [session('retained', 'project', 'Project', { lifecycle: 'suspended', managedCheckoutKind: 'worktree', capabilities: { open: false, resume: true, destroy: true } }), session('live-other', 'project', 'Project')]);
  await page.goto(`https://viewer.test/#workspace/${WORKSPACE_ID}/resume/retained`);
  await page.getByRole('button', { name: 'Destroy session…', exact: true }).click();
  const dialog = page.getByRole('dialog', { name: 'Destroy session' });
  await dialog.getByRole('checkbox').check();
  await dialog.getByRole('button', { name: 'Destroy session', exact: true }).click();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0]).toEqual({ action: 'destroy', session_id: 'retained', delete_branch: true });
  await expect(page.getByRole('button', { name: 'Destroy session…', exact: true })).toBeDisabled();
});

for (const queued of [0, 1]) {
  test(`older worker remains interruptible with ${queued} queued prompts`, async ({ page }) => {
    const parent = session('old-worker', 'project', 'Project', {
      queued, capabilities: { prompt: true, interrupt_turn: true },
    });
    parent.targeted_turn_control_supported = false;
    parent.active_prompt_id = 'old-turn';
    parent.chat_phase = 'running';
    const state = await mount(page, [parent]);
    await card(page, 'old-worker').click();
    // Opening the card hands focus to the conversation screen. Until that
    // screen renders, the composer is not the key target and Escape would
    // reach the page body instead of the turn control.
    await expect(page.locator('#cancel-turn')).toBeVisible();
    await expect(page.locator('#cancel-turn')).toHaveText('Interrupt');
    await page.locator('#prompt-text').press('Escape');
    await expect.poll(() => state.actions.length).toBe(1);
    expect(state.actions[0]).toEqual({ action: 'interrupt-turn', session_id: 'old-worker' });
  });
}

test('earlier messages page backwards, retry errors, and preserve the live draft', async ({ page }) => {
  await mount(page, [session('history', 'alpha', 'Alpha', { capabilities: { prompt: true } })]);
  const queries = [];
  await page.route('**/api/v1/sessions/history/history*', async route => {
    const query = new URL(route.request().url()).search;
    queries.push(query);
    if (queries.length === 2) return route.fulfill({ status: 500, contentType: 'application/json', body: JSON.stringify({ error: 'History temporarily unavailable' }) });
    return route.fulfill({ contentType: 'application/json', body: JSON.stringify(queries.length === 1 ? {
      items: [{ position: 0, stable_id: 'user:101', role: 'user', text: 'Recent stored request' }], before: { position: 101, stable_id: 'user:101' }, frontier: 200,
    } : { items: [{ position: -1, stable_id: 'agent:1', role: 'agent', text: 'First stored answer' }], before: null, frontier: 201 }) });
  });
  await page.route('**/api/conversations/history*', route => route.fulfill({
    contentType: 'application/json', body: JSON.stringify(transcript('Live answer\n\n'.repeat(100))),
  }));
  await card(page, 'history').click();
  await expect(page.locator('#conversation-feed')).toContainText('Live answer');
  await page.locator('#prompt-text').fill('Keep my draft');
  const anchorBefore = await page.evaluate(() => {
    const scroll = document.querySelector('#conversation-scroll');
    scroll.scrollTop = 0;
    const top = document.querySelector('#conversation-feed article').getBoundingClientRect().top;
    scroll.dispatchEvent(new Event('scroll'));
    return top;
  });
  const reader = page.locator('#conversation-feed');
  await expect(reader).toContainText('Recent stored request');
  expect(await reader.locator('[data-entry-id="1"]').evaluate(node => node.getBoundingClientRect().top)).toBeCloseTo(anchorBefore, 0);
  await page.locator('#conversation-scroll').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
  await expect(page.locator('#earlier-messages')).toContainText('History temporarily unavailable');
  await page.locator('#earlier-messages').click();
  await expect(reader).toContainText('First stored answer');
  await expect(reader).toContainText('Recent stored request');
  await expect(page.locator('#earlier-messages')).toBeHidden();
  expect(queries).toEqual(['?before_position=2&before_id=', '?before_position=101&before_id=user%3A101', '?before_position=101&before_id=user%3A101']);
  await expect(page.locator('#prompt-text')).toHaveText('Keep my draft');
});

// Hard-won: inline history used stable IDs to match live numeric positions.
test('inline history reconciles clipped and grouped live rows and continues overlap pages', async ({ page }) => {
  const state = await mount(page, [session('history', 'alpha', 'Alpha')]);
  await page.addStyleTag({ content: '.entry.tone-thought { display: none; }' });
  const live = transcript('Newest live answer');
  live.entries[0].id = 30;
  live.entries.unshift({ ...live.entries[0], id: 20, lines: ['[… 12 earlier lines omitted …]', 'Clipped tail'] });
  await page.route('**/api/conversations/history*', route => route.fulfill({ json: live }));
  const queries = [];
  await page.route('**/api/v1/sessions/history/history*', route => {
    queries.push(new URL(route.request().url()).search);
    const pages = [
      { items: [{ position: 20, stable_id: 'agent:20', role: 'agent', text: 'Restored beginning\nClipped tail' }, { position: 21, stable_id: 'tool:21', role: 'tool', text: 'Collapsed group member' }, { position: 30, stable_id: 'agent:30', role: 'agent', text: 'Newest live answer' }], before: { position: 20, stable_id: 'agent:20' } },
      { items: [{ position: 10, stable_id: 'thought:10', role: 'thought', text: 'Hidden thinking' }], before: { position: 10, stable_id: 'thought:10' } },
      { items: [{ position: 1, stable_id: 'user:1', role: 'user', text: 'Oldest stored request' }], before: null },
    ];
    return route.fulfill({ json: pages[queries.length - 1] });
  });
  await card(page, 'history').click();
  await expect(page.locator('#conversation-feed')).toContainText('Clipped tail');
  await page.locator('#earlier-messages').click();
  const reader = page.locator('#conversation-feed');
  await expect(reader).toContainText('Oldest stored request');
  await expect(reader).toContainText('Restored beginning');
  await expect(reader.locator('article:visible')).toHaveCount(3);
  expect(await reader.locator('article:visible').allTextContents()).toEqual([
    expect.stringContaining('Oldest stored request'), expect.stringContaining('Restored beginning'), expect.stringContaining('Newest live answer'),
  ]);
  state.snapshot.sessions[0].latest_event_ordinal = 31;
  await refresh(page, state);
  await expect(reader).toContainText('Restored beginning');
  await expect(reader.locator('article:visible')).toHaveCount(3);
  expect(queries).toEqual(['?before_position=21&before_id=', '?before_position=20&before_id=agent%3A20', '?before_position=10&before_id=thought%3A10']);
  await expect(page.locator('#earlier-messages')).toBeHidden();
});

// Hard-won: live resets dropped the interval between retained history and the new tail.
test('inline history fills an advancing live window and keeps older paging', async ({ page }) => {
  const state = await mount(page, [session('history', 'alpha', 'Alpha')]);
  const live = transcript('Row 200');
  live.entries[0].id = 200;
  live.entries.unshift({ ...live.entries[0], id: 100, lines: ['Row 100'] });
  await page.route('**/api/conversations/history*', route => route.fulfill({ json: live }));
  const queries = [];
  const item = position => ({ position, stable_id: `agent:${position}`, role: 'agent', text: `Row ${position}` });
  await page.route('**/api/v1/sessions/history/history*', route => {
    const before = Number(new URL(route.request().url()).searchParams.get('before_position'));
    queries.push(before);
    const positions = before === 101 ? [99, 100] : before === 150 ? [100, 120] : [1];
    // The first page captures row 100 while it is still changing; the gap
    // page carries its final stored text.
    const items = positions.map(item).map(row => before === 101 && row.position === 100 ? { ...row, text: 'Row 100 draft' } : row);
    return route.fulfill({ json: { items, before: before === 99 ? null : { position: positions[0], stable_id: `agent:${positions[0]}` } } });
  });
  await card(page, 'history').click();
  await page.locator('#earlier-messages').click();
  const reader = page.locator('#conversation-feed');
  await expect(reader).toContainText('Row 99');
  live.entries = [{ ...live.entries[0], id: 150, lines: ['Row 150'] }, { ...live.entries[1], id: 250, lines: ['Row 250'] }];
  state.snapshot.sessions[0].latest_event_ordinal = 251;
  await refresh(page, state);
  await expect(reader).toContainText('Row 120');
  expect(await reader.locator('article').allTextContents()).toEqual([99, 100, 120, 150, 250].map(n => expect.stringContaining(`Row ${n}`)));
  await expect(reader).not.toContainText('Row 100 draft');
  await page.locator('#conversation-scroll').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
  await expect(reader).toContainText('Row 1');
  expect(queries).toEqual([101, 150, 99]);
  await expect(reader.locator('article')).toHaveCount(6);
});

// Hard-won: restored history froze a clipped live row that was still growing.
test('inline history refetches a restored clipped row that keeps growing', async ({ page }) => {
  const state = await mount(page, [session('history', 'alpha', 'Alpha')]);
  const live = transcript('Newest live answer');
  live.entries[0].id = 30;
  live.entries.unshift({ ...live.entries[0], id: 20, updated_seq: 20, lines: ['[… 12 earlier lines omitted …]', 'Clipped tail'] });
  await page.route('**/api/conversations/history*', route => route.fulfill({ json: live }));
  const queries = [];
  let stored = 'Restored beginning\nClipped tail';
  await page.route('**/api/v1/sessions/history/history*', route => {
    const search = new URL(route.request().url()).search;
    queries.push(search);
    if (search === '?before_position=21&before_id=') {
      return route.fulfill({ json: { items: [{ position: 20, stable_id: 'agent:20', role: 'agent', text: stored }], before: { position: 20, stable_id: 'agent:20' } } });
    }
    return route.fulfill({ json: { items: [], before: null } });
  });
  await card(page, 'history').click();
  await page.locator('#earlier-messages').click();
  const reader = page.locator('#conversation-feed');
  await expect(reader).toContainText('Restored beginning');
  // The row keeps streaming: the live feed carries a newer clipped version.
  stored = 'Restored beginning\nClipped tail\nNew streamed line';
  live.entries[0] = { ...live.entries[0], updated_seq: 25, lines: ['[… 13 earlier lines omitted …]', 'Clipped tail', 'New streamed line'] };
  state.snapshot.sessions[0].latest_event_ordinal = 32;
  await refresh(page, state);
  await expect(reader).toContainText('New streamed line');
  await expect(reader).toContainText('Restored beginning');
  expect(queries.filter(query => query === '?before_position=21&before_id=').length).toBeGreaterThanOrEqual(2);
});

test('inline history ignores a response after navigation', async ({ page }) => {
  await mount(page, [session('history', 'alpha', 'Alpha'), session('other', 'alpha', 'Alpha')]);
  let release;
  let entered = false;
  await page.route('**/api/v1/sessions/history/history*', async route => {
    entered = true;
    await new Promise(resolve => { release = resolve; });
    await route.fulfill({ contentType: 'application/json', body: JSON.stringify({ items: [{role: 'agent', text: 'Late answer'}], before: null, frontier: 1 }) });
  });
  await card(page, 'history').click();
  await page.locator('#earlier-messages').click();
  await expect.poll(() => entered).toBe(true);
  await page.evaluate(() => { location.hash = '#conversation/other'; });
  release();
  await renderAfterFrame(page);
  await expect(page.getByText('Late answer', { exact: true })).toHaveCount(0);
});

// Hard-won: 8ba3d377e: queued model changes were offered as steerable prompts.
test('mobile Apply queued preserves the published target and keeps interruption separate', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 740 });
  const parent = session('settings-queue', 'project', 'A long project title', {
    capabilities: { prompt: true, interrupt_turn: false, set_config: true },
  });
  parent.chat_phase = 'running'; parent.active_prompt_id = 'parent-turn';
  parent.queued_prompts = [
    { id: 'change-model', kind: { set_config: { key: 'model', value: 'fable' } }, text: '/model fable' },
    { id: 'change-effort', kind: { set_config: { key: 'effort', value: 'high' } }, text: '/effort high' },
    { id: 'question', kind: 'prompt', text: 'Compare Go with Rust' },
  ];
  parent.config_options = [{ key: 'model', current: 'opus', choices: [{ value: 'opus', name: 'Opus 5.5' }, { value: 'fable', name: 'Fable 5.1' }] }];
  const command = { type: 'apply_queued_for', data: { active_prompt_id: 'parent-turn', queued_command_id: 'change-model' } };
  parent.turn_controls = { status: 'ready', primary: { kind: 'apply_queued', command },
    interrupt: { type: 'cancel_turn_for', data: { active_prompt_id: 'parent-turn' } },
    message: 'Interrupts this turn to apply model: fable, effort: high and queued input. Sub-agents continue.' };
  const state = await mount(page, [parent]);
  await card(page, parent.id).click();
  const apply = page.getByRole('button', { name: 'Apply queued', exact: true });
  await expect(apply).toBeVisible();
  await expect(page.locator('#turn-control-status')).toContainText('Sub-agents continue.');
  const overflow = await page.evaluate(() => [...document.querySelectorAll('body *')].filter(e => {
    const r = e.getBoundingClientRect(); return r.width > 0 && (r.right > innerWidth + 1 || r.left < -1);
  }).map(e => ({ tag: e.tagName, id: e.id, cls: e.className, width: e.getBoundingClientRect().width, right: e.getBoundingClientRect().right })));
  expect(overflow).toEqual([]);
  const widths = await page.evaluate(() => ({ width: innerWidth, scroll: document.documentElement.scrollWidth,
    elements: [...document.querySelectorAll('body *')].filter(e => e.scrollWidth > e.clientWidth + 1 && e.getBoundingClientRect().width > 0).map(e => ({id:e.id, cls:e.className, width:e.clientWidth, scroll:e.scrollWidth})) }));
  expect(widths.scroll, JSON.stringify(widths)).toBeLessThanOrEqual(widths.width);
  const bounds = await apply.boundingBox();
  expect(bounds.x + bounds.width).toBeLessThanOrEqual(320);
  await reconnect(page, state);
  const press = await apply.boundingBox();
  await page.mouse.move(press.x + press.width / 2, press.y + press.height / 2);
  await page.mouse.down();
  // The old turn ends and the next settings batch appears while the finger is down.
  parent.active_prompt_id = 'replacement-turn';
  parent.turn_controls = { ...parent.turn_controls,
    primary: { kind: 'apply_queued', command: { type: 'apply_queued_for', data: { active_prompt_id: 'replacement-turn', queued_command_id: 'replacement-setting' } } },
    interrupt: { type: 'cancel_turn_for', data: { active_prompt_id: 'replacement-turn' } },
  };
  await refresh(page, state);
  await page.mouse.up();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0].command).toEqual(command);
  await page.getByLabel('More turn actions').click();
  await page.getByRole('button', { name: 'Interrupt turn', exact: true }).click();
  await expect.poll(() => state.actions.length).toBe(2);
  expect(state.actions[1].command).toEqual(parent.turn_controls.interrupt);
});

test('a running review leaves the conversation composer usable', async ({ page }) => {
  const reviewing = session('reviewing-primary', 'project-review', 'Review', {
    capabilities: { prompt: true },
  });
  reviewing.turn_review = { status: 'Review running · 2 reviewers', roles: [], verdict: null };
  const state = await mount(page, [reviewing]);
  await card(page, 'reviewing-primary').click();
  await expect(page.locator('#turn-review')).toContainText('Review running · 2 reviewers');
  await expect(page.locator('#send-button')).toBeEnabled();
  await page.locator('#prompt-text').fill('Continue while reviewers work');
  await expect(page.locator('#prompt-text')).toHaveText('Continue while reviewers work');

  // Roles and a verdict stay out of the conversation: the review is one short
  // passive line, and the reviewers' own conversations are in Reviewers.
  const line = page.locator('#turn-review');
  reviewing.turn_review = {
    status: 'Findings sent to the agent',
    roles: [
      { label: 'Reviewer 1', state: 'findings', child_session_id: 'r1' },
      { label: 'Reviewer 2', state: 'running', child_session_id: 'r2' },
    ],
    verdict: { kind: 'findings', text: Array.from({ length: 40 }, (_, i) => `[P1] finding ${i}`).join('\n') },
  };
  await refresh(page, state);
  await expect(line).toHaveText('Review: Findings sent to the agent');
  await expect(line.locator('*')).toHaveCount(1);
  expect((await line.boundingBox()).height).toBeLessThan(40);
  await expect(page.locator('#conversation-scroll')).toBeVisible();
});

test('reviewers open their own workspace and stay out of Subagents', async ({ page }) => {
  const parent = session('parent', 'project', 'Project', {
    title: 'Parent session',
    subagentSessionIds: ['worker', 'review-open', 'review-done'],
    reviewerSessionIds: ['review-open', 'review-done'],
  });
  const child = (id, options = {}) => session(id, 'project', 'Project', { subagentParentId: 'parent', ...options });
  const worker = child('worker', { title: 'Worker', isIdle: false });
  worker.chat_phase = 'running';
  const reviewOpen = child('review-open', { title: 'Open reviewer', lifecycle: 'failed', active: true, isIdle: false });
  reviewOpen.chat_phase = 'running';
  const reviewDone = child('review-done', { title: 'Done reviewer', lifecycle: 'suspended', active: false, capabilities: { open: false, read_history: true, resume: true } });
  const state = await mount(page, [parent, worker, reviewOpen, reviewDone]);
  await card(page, 'parent').click();
  await expect(page.locator('#subagents-button')).toHaveText('Subagents · 1');
  await expect(page.locator('#reviewers-button')).toHaveText('Reviewers · 1');
  await expect(page.locator('#reviewers-button')).toHaveAttribute('title', '2 reviewers');
  await expect(page.locator('#subagents-button')).toHaveAttribute('title', '1 retained agents');

  // Both controls fit beside each other without leaving the screen.
  const [subagents, reviewers] = await Promise.all(['#subagents-button', '#reviewers-button']
    .map(selector => page.locator(selector).boundingBox()));
  expect(subagents.x + subagents.width).toBeLessThanOrEqual(reviewers.x);
  expect(reviewers.x + reviewers.width).toBeLessThanOrEqual(390);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390);

  await page.locator('#reviewers-button').click();
  await expect(page).toHaveURL(/#reviewers\/parent$/);
  await expect(page.locator('#workspaces .virtual-workspace')).toContainText('Reviewers · Parent session');
  await expect(card(page, 'review-open')).toBeVisible();
  await expect(card(page, 'worker')).toHaveCount(0);
  await expect(card(page, 'review-done')).toHaveCount(0);
  await expect(page.locator('#sessions')).toContainText('1 idle reviewer hidden');
  await page.getByRole('button', { name: 'Show idle' }).click();
  await expect(card(page, 'review-done')).toBeVisible();
  await expect(card(page, 'worker')).toHaveCount(0);

  // A stopped reviewer cannot open live, but its stored messages can be read
  // without leaving the list. The newest page loads first.
  const historyRequests = [];
  await page.route('**/api/v1/sessions/review-done/history*', route => {
    historyRequests.push(new URL(route.request().url()).search);
    return route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ frontier: 5, before: null, items: [{ role: 'agent', text: 'Stored verdict from the done reviewer' }] }),
    });
  });
  await expect(card(page, 'review-done')).toHaveAttribute('role', 'link');
  await card(page, 'review-done').click();
  const stored = page.getByRole('dialog', { name: 'Conversation: Done reviewer' });
  await expect(stored).toContainText('Stored verdict from the done reviewer');
  await expect(stored.getByRole('heading', { name: 'Done reviewer' })).toBeVisible();
  await expect(page).toHaveURL(/#reviewers\/parent$/);
  expect(historyRequests).toEqual(['']);
  await stored.getByRole('button', { name: 'Close', exact: true }).click();
  await expect(stored).toHaveCount(0);
  await card(page, 'review-done').focus();
  await page.keyboard.press('Enter');
  await expect(page.getByRole('dialog', { name: 'Conversation: Done reviewer' })).toContainText('Stored verdict');
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).toHaveCount(0);

  // A reviewer conversation returns to the reviewer list, and the close
  // control returns to the parent conversation.
  await card(page, 'review-open').click();
  await expect(page).toHaveURL(/#reviewers\/parent\/review-open$/);
  await page.locator('#back').click();
  await expect(page).toHaveURL(/#reviewers\/parent$/);
  await page.reload();
  await expect(card(page, 'review-open')).toBeVisible();
  await page.getByRole('button', { name: 'Close Parent session reviewers workspace' }).click();
  await expect(page).toHaveURL(/#conversation\/parent$/);

  await page.locator('#subagents-button').click();
  await expect(page).toHaveURL(/#subagents\/parent$/);
  await expect(page.locator('#workspaces .virtual-workspace')).not.toContainText('Reviewers');
  await expect(card(page, 'worker')).toBeVisible();
  await expect(card(page, 'review-open')).toHaveCount(0);
  await expect(card(page, 'review-done')).toHaveCount(0);

  // A link that names a child through the other kind's workspace is stale.
  await page.evaluate(() => { location.hash = '#subagents/parent/review-open'; });
  await expect(page).toHaveURL(/#subagents\/parent$/);
  await page.evaluate(() => { location.hash = '#reviewers/parent/worker'; });
  await expect(page).toHaveURL(/#reviewers\/parent$/);

  // Desktop shows the same two controls on the prompt's lower border.
  await page.getByRole('button', { name: 'Close Parent session reviewers workspace' }).click();
  await page.setViewportSize({ width: 1280, height: 800 });
  await expect(page.locator('#subagents-button-desktop')).toHaveText('Subagents · 1');
  await expect(page.locator('#reviewers-button-desktop')).toHaveText('Reviewers · 1');
  await expect(page.locator('#reviewers-button')).toBeHidden();
  await page.locator('#reviewers-button-desktop').click();
  await expect(page).toHaveURL(/#reviewers\/parent$/);
  await page.getByRole('button', { name: 'Close Parent session reviewers workspace' }).click();
  await page.setViewportSize({ width: 390, height: 844 });

  // Finished reviewers keep the control reachable with an open count of zero;
  // with none at all the control is gone and Subagents keeps its own count.
  reviewOpen.lifecycle = 'suspended';
  reviewOpen.state = 'suspended';
  reviewOpen.active = false;
  await refresh(page, state);
  await expect(page.locator('#reviewers-button')).toHaveText('Reviewers · 0');
  parent.subagent_session_ids = ['worker'];
  parent.reviewer_session_ids = [];
  await refresh(page, state);
  await expect(page.locator('#reviewers-button')).toBeHidden();
  await expect(page.locator('#reviewers-button-desktop')).toBeHidden();
  await expect(page.locator('#subagents-button')).toHaveText('Subagents · 1');
});
