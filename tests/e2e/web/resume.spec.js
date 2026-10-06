const { test, expect } = require('@playwright/test');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, serviceWorkers: 'block', timezoneId: 'UTC' });

function session(id, overrides = {}) {
  return {
    id,
    title: id,
    state: 'suspended',
    lifecycle: 'suspended',
    workspace_id: 'test',
    profile_id: 'alpha',
    target_id: 'local',
    capabilities: { resume: true },
    compatible_resume_targets: ['local', 'remote'],
    last_activity_at_ms: 1_000,
    ...overrides,
  };
}

async function mount(page, {
  sessions = [session('suspended', { title: 'Suspended test' })],
  profiles = [{ id: 'alpha', harness_kind: 'codex' }, { id: 'beta', harness_kind: 'claude' }],
  targets = [
    { id: 'local', kind: 'local', requires_project_directory: true, recent_project_directories: [] },
    { id: 'remote', kind: 'ssh', requires_project_directory: true, recent_project_directories: [] },
  ],
  holdAction = null,
  rejectAction = false,
  // A ready index with nothing in it, which is what most of these tests want:
  // search works, and there is nothing archived. Pass `wiki: null` for a
  // daemon that has no wiki routes at all.
  wiki = { rows: [], restoredId: 'restored' },
} = {}) {
  const state = {
    snapshot: {
      revision: 1,
      workspaces: [{ id: 'test', name: 'Test' }, { id: 'other', name: 'Other' }],
      sessions,
      profiles,
      targets,
      bundles: [],
      capacity: [],
      launch_failures: [],
    },
    snapshots: 0,
    actions: [],
    holdAction,
    rejectAction,
    wiki,
    wikiQueries: [],
    wikiRestores: [],
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
    if (pathname === '/api/actions') {
      state.actions.push(route.request().postDataJSON());
      if (state.holdAction) await state.holdAction;
      if (state.rejectAction) {
        return route.fulfill({
          status: 400,
          contentType: 'application/json',
          body: JSON.stringify({ error: 'The selected target is unavailable' }),
        });
      }
      return route.fulfill({ status: 202, body: '' });
    }
    if (pathname.startsWith('/api/v1/wiki/')) {
      if (!state.wiki) return route.fulfill({ status: 404, contentType: 'application/json', body: JSON.stringify({ error: 'not found' }) });
      if (pathname === '/api/v1/wiki/search') {
        const query = new URL(route.request().url()).searchParams.get('q') || '';
        state.wikiQueries.push(query);
        const rows = (state.wiki.rows || []).filter(row =>
          !query || JSON.stringify(row).toLowerCase().includes(query.toLowerCase()));
        return json({ rows, status: state.wiki.status || { state: 'ready', topping_up: false } });
      }
      if (pathname.endsWith('/brief')) return json({ markdown: state.wiki.brief || '' });
      if (pathname.endsWith('/restore')) {
        state.wikiRestores.push({ id: pathname.split('/')[5], body: route.request().postDataJSON() });
        state.snapshot.sessions = [
          ...state.snapshot.sessions,
          session(state.wiki.restoredId, { state: 'running', lifecycle: 'running', capabilities: { open: true, resume: false } }),
        ];
        return route.fulfill({
          status: 201,
          contentType: 'application/json',
          body: JSON.stringify({ session_id: state.wiki.restoredId }),
        });
      }
    }
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    if (['viewer.html', 'viewer.js', 'viewer.css', 'markdown.js', 'tool-output.js', 'manifest.webmanifest', 'icon.svg'].includes(file)) {
      return route.fulfill({ path: path.join(webRoot, file === 'icon.svg' ? '../icons/icon.svg' : file) });
    }
    return route.fulfill({ status: 404, body: '' });
  });
  await page.goto('https://viewer.test/#workspace/test/resume');
  await expect(page.locator('#resume-page')).toBeVisible();
  return state;
}

async function refresh(page, state) {
  const previous = state.snapshots;
  state.snapshot.revision += 1;
  // The viewer reconciles a changed daemon by reconnecting and reloading the
  // whole snapshot; the changes stream itself carries deltas this fixture
  // does not model.
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect.poll(() => state.snapshots).toBeGreaterThan(previous);
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
}

async function openSession(page, id) {
  const row = page.locator(`#resumable [data-session-id="${id}"]`);
  await expect(row).toBeVisible();
  await row.click();
  await expect(page).toHaveURL(new RegExp(`/resume/${id}$`));
  await expect(page.locator('#resume-detail-view')).toBeVisible();
  return row;
}

function picker(detail, role) {
  return detail.locator(`[data-role="${role}"]`);
}

async function choose(field, value) {
  await field.locator('select').selectOption(value);
}

async function checkedValue(field) {
  return field.locator('select').inputValue();
}

function wikiRow(id, sessionId, overrides = {}) {
  return {
    id,
    tool: 'mjolnir',
    project: '/tmp/project',
    title: sessionId,
    started: '2026-09-17T00:23:00Z',
    msgs: 3,
    preview: '',
    archived: false,
    native_id: null,
    snippet: null,
    hel_session_id: sessionId,
    ...overrides,
  };
}

async function captureResumeState(output, page, label, state, extra = null) {
  const viewport = page.viewportSize();
  output.push(`=== ${label} (${viewport.width}x${viewport.height}) ===`);
  output.push((await page.locator('#resume-page').isVisible())
    ? (await page.locator('#resume-page').innerText()).trim()
    : (await page.locator('#app').innerText()).trim());
  const controls = await page.locator('#resume-page input, #resume-page select, #resume-page button').evaluateAll(nodes => nodes.map(node => ({
    id: node.id || null,
    text: node.innerText?.trim() || node.getAttribute('aria-label') || null,
    value: 'value' in node ? node.value : null,
    disabled: 'disabled' in node ? node.disabled : null,
  })));
  output.push(`controls: ${JSON.stringify(controls)}`);
  if (state.actions.length) output.push(`action: POST /api/actions ${JSON.stringify(state.actions)}`);
  if (state.wikiQueries.length) output.push(`request: GET /api/v1/wiki/search ${JSON.stringify(state.wikiQueries)}`);
  if (state.wikiRestores.length) output.push(`action: POST /api/v1/wiki/archive/restore ${JSON.stringify(state.wikiRestores)}`);
  if (extra !== null) output.push(`state: ${JSON.stringify(extra)}`);
}

test('golden_viewer_resume', async ({ context }) => {
  const { assertGolden } = await import('./golden.mjs');
  const output = [];

  const listPage = await context.newPage();
  const listState = await mount(listPage, {
    sessions: [
      session('newest', { title: 'Newest profile work', project_label: 'alpha-project', last_activity_at_ms: 3_000 }),
      session('older', { title: 'A very long session title that must stay inside the phone viewport without horizontal scrolling', project_label: 'older-project', last_activity_at_ms: 2_000 }),
      session('tie-b', { title: 'Tie B', project_label: 'shared-project', last_activity_at_ms: 1_000 }),
      session('tie-a', { title: 'Tie A', project_label: 'shared-project', last_activity_at_ms: 1_000 }),
      session('other-workspace', { title: 'Other workspace', workspace_id: 'other', last_activity_at_ms: 9_000 }),
    ],
    profiles: Array.from({ length: 18 }, (_, index) => ({ id: `profile-${index}`, harness_kind: 'codex' })),
    wiki: {
      rows: [wikiRow('w-older', 'older', { snippet: 'a pomegranate sentinel' }), wikiRow('w-newest', 'newest', { title: 'pomegranate' })],
      restoredId: 'restored',
    },
  });
  await captureResumeState(output, listPage, 'current-workspace sessions in index order', listState, {
    sessionIds: await listPage.locator('#resumable [data-session-id]').evaluateAll(nodes => nodes.map(node => node.dataset.sessionId)),
    width: await listPage.evaluate(() => ({ document: document.documentElement.scrollWidth, viewport: innerWidth })),
  });
  await listPage.locator('#resume-search').fill('pomegranate');
  await expect(listPage.locator('#resumable [data-session-id]')).toHaveCount(2);
  await captureResumeState(output, listPage, 'search uses index hits and their snippets', listState, {
    sessionIds: await listPage.locator('#resumable [data-session-id]').evaluateAll(nodes => nodes.map(node => node.dataset.sessionId)),
  });
  await listPage.locator('#resume-search').fill('does-not-exist');
  await expect.poll(() => listState.wikiQueries.at(-1)).toBe('does-not-exist');
  await expect(listPage.locator('#resumable [data-session-id]')).toHaveCount(0);
  await captureResumeState(output, listPage, 'empty resume search', listState);
  await listPage.close();

  const selectedPage = await context.newPage();
  const selectedState = await mount(selectedPage, { sessions: [session('mutating', { title: 'Mutating session' })] });
  await openSession(selectedPage, 'mutating');
  await captureResumeState(output, selectedPage, 'selected resumable session', selectedState);
  selectedState.snapshot.sessions[0].has_error = true;
  selectedState.snapshot.sessions[0].queued_prompts = [{ id: 'prompt-1', text: 'queued later' }];
  await refresh(selectedPage, selectedState);
  await captureResumeState(output, selectedPage, 'selected card after error and queued prompt arrive', selectedState);
  await selectedPage.close();

  const archivedListPage = await context.newPage();
  const archivedListState = await mount(archivedListPage, {
    wiki: {
      rows: [
        { id: 'archived-one', tool: 'mjolnir', project: '/tmp/project', title: 'Archived pomegranate work', started: '2026-09-17T00:23:00Z', msgs: 3, preview: 'the single word is hello', archived: true, native_id: null, snippet: null, hel_session_id: null },
        { id: 'live-one', tool: 'mjolnir', project: '/tmp/project', title: 'Suspended test', started: '2026-09-17T01:00:00Z', msgs: 5, preview: 'still here', archived: false, native_id: null, snippet: 'a pomegranate sentinel', hel_session_id: 'suspended' },
      ],
      brief: '# Previous session\n\nThe single word is hello.',
      restoredId: 'restored',
    },
  });
  await expect.poll(() => archivedListState.wikiQueries.at(-1)).toBe('');
  await archivedListPage.locator('#resume-search').pressSequentially('pomegranate', { delay: 20 });
  await expect.poll(() => archivedListState.wikiQueries.at(-1)).toBe('pomegranate');
  await captureResumeState(output, archivedListPage, 'archived index entry and live search hit', archivedListState, {
    archivedIds: await archivedListPage.locator('#resume-archived [data-wiki-id]').evaluateAll(nodes => nodes.map(node => node.dataset.wikiId)),
    liveIds: await archivedListPage.locator('#resumable [data-session-id]').evaluateAll(nodes => nodes.map(node => node.dataset.sessionId)),
  });
  await archivedListPage.close();

  const restorePage = await context.newPage();
  const restoreState = await mount(restorePage, {
    wiki: {
      rows: [{ id: 'archived-one', tool: 'mjolnir', project: '/tmp/project', title: 'Archived pomegranate work', started: '2026-09-17T00:23:00Z', msgs: 3, preview: 'the single word is hello', archived: true, native_id: null, snippet: null, hel_session_id: null }],
      brief: '# Previous session\n\nThe single word is hello.',
      restoredId: 'restored',
    },
  });
  await expect.poll(() => restoreState.wikiQueries.at(-1)).toBe('');
  await restorePage.locator('#resume-archived [data-wiki-id="archived-one"]').click();
  await expect(restorePage.locator('#resume-detail')).toContainText('The single word is hello.');
  await captureResumeState(output, restorePage, 'archived brief before restore', restoreState);
  await choose(picker(restorePage.locator('#resume-detail'), 'wiki-profile'), 'beta');
  await choose(picker(restorePage.locator('#resume-detail'), 'wiki-target'), 'remote');
  await restorePage.locator('#resume-detail').getByRole('button', { name: 'Restore', exact: true }).click();
  await expect.poll(() => restoreState.wikiRestores.length).toBe(1);
  await expect(restorePage).toHaveURL(/restored/);
  await expect(restorePage.locator('#conversation-title')).toHaveText('restored');
  await captureResumeState(output, restorePage, 'restored archive opens as a new conversation', restoreState);
  await restorePage.close();

  const barePage = await context.newPage();
  const preflightRequests = [];
  barePage.on('request', request => {
    if (new URL(request.url()).pathname === '/api/preflight/resume') preflightRequests.push(request.postDataJSON());
  });
  const bareState = await mount(barePage);
  await expect.poll(() => bareState.wikiQueries.at(-1)).toBe('');
  await openSession(barePage, 'suspended');
  await barePage.locator('[data-role="resume-target"] select').selectOption('local');
  await captureResumeState(output, barePage, 'bare destination leaves Resume ready without conversion questions', bareState, {
    preflightRequests,
  });
  await barePage.close();

  assertGolden('viewer_resume', output.join('\n'));
});

test('selecting a row asks for settings only after selection and Back restores search and focus', async ({ page }) => {
  const state = await mount(page, {
    sessions: [
      session('first', { title: 'First session', last_activity_at_ms: 2_000 }),
      session('second', { title: 'Second session', last_activity_at_ms: 1_000 }),
    ],
    wiki: {
      rows: [wikiRow('w-first', 'first'), wikiRow('w-second', 'second')],
      restoredId: 'restored',
    },
  });
  const search = page.locator('#resume-search');
  await search.fill('session');
  await search.focus();
  await openSession(page, 'first');
  await expect(page.locator('#resume-detail')).toContainText('First session');
  await expect(page.locator('#resume-detail [data-role="resume-profile"]').first()).toBeVisible();
  await expect(page.locator('#resume-list-view')).toBeHidden();

  await page.locator('#resume-detail-back').click();
  await expect(page).toHaveURL(/\/resume$/);
  await expect(page.locator('#resume-search')).toHaveValue('session');
  await expect(page.locator('#resumable [data-session-id="first"]')).toBeFocused();
  await expect(page.locator('#resumable [data-session-id]')).toHaveCount(2);
  await refresh(page, state);
  await expect(page.locator('#resume-search')).toHaveValue('session');
});

test('selected settings and queued-work choice survive a snapshot and become the action payload', async ({ page }) => {
  const state = await mount(page, {
    sessions: [session('queued', {
      title: 'Queued session',
      profile_id: 'alpha',
      target_id: 'local',
      queued_prompts: [{ id: 'prompt-1', text: 'run tests' }],
    })],
  });
  await openSession(page, 'queued');
  const detail = page.locator('#resume-detail');
  await choose(picker(detail, 'resume-profile'), 'beta');
  await choose(picker(detail, 'resume-target'), 'remote');
  await choose(picker(detail, 'resume-queue'), 'discard');
  await refresh(page, state);
  await expect.poll(() => checkedValue(picker(detail, 'resume-profile'))).toBe('beta');
  await expect.poll(() => checkedValue(picker(detail, 'resume-target'))).toBe('remote');
  await expect.poll(() => checkedValue(picker(detail, 'resume-queue'))).toBe('discard');
  await detail.getByRole('button', { name: 'Resume', exact: true }).click();
  await expect.poll(() => state.actions).toHaveLength(1);
  expect(state.actions[0]).toEqual({
    action: 'resume', session_id: 'queued', workspace_id: 'test',
    profile_id: 'beta', target_id: 'remote', queue: 'discard',
  });
});

test('missing previous configuration requires a new choice before Resume', async ({ page }) => {
  await mount(page, {
    sessions: [session('invalid', { profile_id: 'removed-profile', target_id: 'removed-target' })],
  });
  await openSession(page, 'invalid');
  const detail = page.locator('#resume-detail');
  const profile = picker(detail, 'resume-profile');
  await expect(profile.locator('select')).toHaveValue('');
  const resume = detail.getByRole('button', { name: 'Resume', exact: true });
  await expect(resume).toBeDisabled();
  await choose(profile, 'beta');
  await choose(picker(detail, 'resume-target'), 'remote');
  await expect(resume).toBeEnabled();
});

test('removing the selected profile clears the draft instead of silently replacing it', async ({ page }) => {
  const state = await mount(page, {
    profiles: [
      { id: 'alpha', harness_kind: 'codex' },
      { id: 'beta', harness_kind: 'claude' },
      { id: 'gamma', harness_kind: 'codex' },
    ],
    sessions: [session('profile-update', { profile_id: 'alpha' })],
  });
  await openSession(page, 'profile-update');
  const detail = page.locator('#resume-detail');
  await choose(picker(detail, 'resume-profile'), 'beta');
  state.snapshot.profiles = [{ id: 'alpha', harness_kind: 'codex' }, { id: 'gamma', harness_kind: 'codex' }];
  await refresh(page, state);
  await expect(picker(detail, 'resume-profile').locator('select')).toHaveValue('');
  await expect(detail.getByRole('button', { name: 'Resume', exact: true })).toBeDisabled();
});

test('active or vanished selected sessions replace stale resume controls with an explanation', async ({ page }) => {
  const state = await mount(page, { sessions: [session('volatile', { title: 'Volatile session' })] });
  await openSession(page, 'volatile');
  state.snapshot.sessions[0].lifecycle = 'live';
  state.snapshot.sessions[0].capabilities = { resume: true, open: true };
  await refresh(page, state);
  const detail = page.locator('#resume-detail');
  await expect(detail).toContainText(/active now and cannot be resumed/i);
  await expect(detail.getByRole('button', { name: 'Resume', exact: true })).toHaveCount(0);
  state.snapshot.sessions = [];
  await refresh(page, state);
  await expect(detail).toContainText(/no longer available/i);
  await expect(detail.getByRole('button', { name: 'Resume', exact: true })).toHaveCount(0);
});

test('a direct link cannot open a session from another workspace', async ({ page }) => {
  await mount(page, { sessions: [session('private', { title: 'Test only' })] });
  await page.evaluate(() => { location.hash = '#workspace/other/resume/private'; });
  await expect(page).toHaveURL(/#workspace\/other\/resume\/private$/);
  await expect(page.locator('#resume-list-view')).toBeHidden();
  await expect(page.locator('#resume-detail')).toContainText(/no longer available/i);
  await expect(page.locator('#resume-detail').getByRole('button', { name: 'Resume', exact: true })).toHaveCount(0);
});

test('Resume is single-flight and does not navigate late', async ({ page }) => {
  let release;
  const holdAction = new Promise(resolve => { release = resolve; });
  const state = await mount(page, {
    sessions: [session('pending', { title: 'Pending session' })],
    holdAction,
  });
  await openSession(page, 'pending');
  const detail = page.locator('#resume-detail');
  const resume = detail.getByRole('button', { name: 'Resume', exact: true });
  await resume.dblclick();
  await expect.poll(() => state.actions).toHaveLength(1);
  await expect(resume).toBeDisabled();
  await page.evaluate(() => { location.hash = '#workspace/other'; });
  await expect(page).toHaveURL(/#workspace\/other$/);
  release();
  await expect.poll(() => state.snapshots).toBeGreaterThan(1);
  await expect(page).toHaveURL(/#workspace\/other$/);
});

test('a rejected Resume request preserves its selected card and draft controls', async ({ page }) => {
  const failureState = await mount(page, {
    sessions: [session('rejected', { title: 'Rejected session' })],
    rejectAction: true,
  });
  await openSession(page, 'rejected');
  const failureDetail = page.locator('#resume-detail');
  await failureDetail.getByRole('button', { name: 'Resume', exact: true }).click();
  await expect(failureDetail).toContainText('selected target is unavailable');
  await expect(failureDetail).toBeVisible();
  await expect(failureDetail.locator('[data-role="resume-profile"]')).toBeVisible();
  expect(failureState.actions).toHaveLength(1);
});

test('errors and move recovery live in the selected card, with retained source settings', async ({ page }) => {
  const state = await mount(page, {
    sessions: [session('failed-move', {
      title: 'Failed move',
      has_error: true,
      move_recovery: {
        phase: 'failed',
        checkpoint_retained: true,
        source_profile_id: 'alpha',
        source_target_template_id: 'local',
        source_additional_mounts: [{ source: '/work/project', destination: '/project', read_only: true }],
        source_resource_allocation: { kind: 'container', cpus: 2, memory_bytes: 2147483648 },
        destination_target_template_id: 'remote',
        destination_profile_id: 'beta',
        queue_admission_started: false,
        queue_admission_finished: false,
      },
      queued_prompts: [{ id: 'q', text: 'queued work' }],
    })],
  });
  await page.evaluate(() => { location.hash = '#workspace/test'; });
  await expect(page.locator('#dashboard')).toBeVisible();
  await expect(page.locator('#launch-failures')).not.toContainText('Retry move');
  await page.getByRole('button', { name: 'Resume a session' }).click();
  await expect(page.locator('#resume-list-view')).toBeVisible();
  await expect(page.locator('#resumable [data-session-id="failed-move"]')).toBeVisible();
  await expect(page.locator('#resumable [data-session-id="failed-move"]')).not.toContainText(/previous operation failed/i);
  await openSession(page, 'failed-move');
  const detail = page.locator('#resume-detail');
  await expect(detail).toContainText(/previous operation failed|error|failed/i);
  await choose(picker(detail, 'resume-profile'), 'beta');
  await choose(picker(detail, 'resume-target'), 'remote');
  await choose(picker(detail, 'resume-queue'), 'start');
  await detail.getByRole('button', { name: 'Resume', exact: true }).click();
  await expect.poll(() => state.actions).toHaveLength(1);
  expect(state.actions[0]).toMatchObject({
    additional_mounts: [{ source: '/work/project', destination: '/project', read_only: true }],
    resource_allocation: { kind: 'container', cpus: 2, memory_bytes: 2147483648 },
    queue: 'start',
  });
});

test('a queue-pinned move recovery exposes retry Move and does not offer unsafe resume', async ({ page }) => {
  await mount(page, {
    sessions: [session('pinned', {
      title: 'Pinned recovery',
      move_recovery: {
        phase: 'failed', checkpoint_retained: true,
        source_profile_id: 'alpha', source_target_template_id: 'local',
        destination_target_template_id: 'remote', destination_profile_id: 'beta',
        queue_admission_started: true, queue_admission_finished: false,
      },
      queued_prompts: [{ id: 'q', text: 'queued work' }],
    })],
  });
  await openSession(page, 'pinned');
  const detail = page.locator('#resume-detail');
  await expect(detail).toContainText(/queued work already began|retry move/i);
  await expect(detail.getByRole('button', { name: 'Resume', exact: true })).toHaveCount(0);
  await expect(detail.getByRole('button', { name: 'Retry move', exact: true })).toBeVisible();
  await detail.getByRole('button', { name: 'Retry move', exact: true }).click();
  await expect(page).toHaveURL(/#workspace\/test\/move\/pinned$/);
  await expect(page.locator('#move-page')).toBeVisible();
});

test('a removed choice can be explicitly replaced when only one alternative remains', async ({ page }) => {
  const state = await mount(page);
  await openSession(page, 'suspended');
  const detail = page.locator('#resume-detail');
  await choose(picker(detail, 'resume-profile'), 'beta');
  await picker(detail, 'resume-profile').locator('select').focus();
  state.snapshot.profiles = [{ id: 'alpha', harness_kind: 'codex' }];
  await refresh(page, state);
  const profile = picker(detail, 'resume-profile').locator('select');
  await expect(profile).toHaveValue('');
  await expect(profile).toBeFocused();
  await expect(detail.getByRole('button', { name: 'Resume', exact: true })).toBeDisabled();
  await profile.selectOption('alpha');
  await expect(detail.getByRole('button', { name: 'Resume', exact: true })).toBeEnabled();
});

test('single configured choices need no dropdowns and unrecoverable sessions offer no Resume', async ({ page }) => {
  const state = await mount(page, {
    profiles: [{ id: 'alpha', harness_kind: 'codex' }],
    sessions: [session('only', { compatible_resume_targets: ['local'] })],
  });
  await openSession(page, 'only');
  const detail = page.locator('#resume-detail');
  await expect(detail.locator('select')).toHaveCount(0);
  await expect(detail.getByRole('button', { name: 'Resume', exact: true })).toBeEnabled();
  for (const [sessionState, explanation] of [
    ['lost', 'No verified recovery checkpoint'],
    ['destroyed-with-data-loss', 'No session data remains'],
  ]) {
    state.snapshot.sessions[0].state = sessionState;
    await refresh(page, state);
    await expect(detail).toContainText(explanation);
    await expect(detail.getByRole('button', { name: 'Resume', exact: true })).toHaveCount(0);
  }
});

test('request errors survive card updates and stay with their session after navigation', async ({ page }) => {
  let release;
  const state = await mount(page, {
    sessions: [session('first'), session('second')],
    holdAction: new Promise(resolve => { release = resolve; }),
    rejectAction: true,
  });
  await openSession(page, 'first');
  await page.locator('#resume-detail').getByRole('button', { name: 'Resume', exact: true }).click();
  await expect.poll(() => state.actions.length).toBe(1);
  state.snapshot.sessions[0].has_error = true;
  await refresh(page, state);
  await page.locator('#resume-detail-back').click();
  await openSession(page, 'second');
  release();
  await expect(page.locator('#resume-detail')).not.toContainText('selected target is unavailable');
  await page.locator('#resume-detail-back').click();
  await openSession(page, 'first');
  await expect(page.locator('#resume-detail')).toContainText('selected target is unavailable');
  state.snapshot.sessions[0].title = 'Updated title';
  await refresh(page, state);
  await expect(page.locator('#resume-detail')).toContainText('selected target is unavailable');
});

test('keyboard selection and browser Back restore a scrolled list', async ({ page }) => {
  const state = await mount(page, {
    sessions: Array.from({ length: 24 }, (_, index) => session(`session-${String(index).padStart(2, '0')}`)),
  });
  const row = page.locator('#resumable [data-session-id="session-18"]');
  await row.scrollIntoViewIfNeeded();
  await row.focus();
  const scroll = await page.evaluate(() => window.scrollY);
  expect(scroll).toBeGreaterThan(500);
  await row.press('Enter');
  await expect(page).toHaveURL(/resume\/session-18$/);
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(0);
  await page.goBack();
  await expect(page).toHaveURL(/\/resume$/);
  await expect(row).toBeFocused();
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(scroll);
  await refresh(page, state);
  await expect(row).toBeFocused();
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(scroll);
});

test('a late success does not redirect a new visit to the same card', async ({ page }) => {
  let release;
  const state = await mount(page, { holdAction: new Promise(resolve => { release = resolve; }) });
  await openSession(page, 'suspended');
  await page.locator('#resume-detail').getByRole('button', { name: 'Resume', exact: true }).click();
  await expect.poll(() => state.actions.length).toBe(1);
  await page.locator('#resume-detail-back').click();
  await openSession(page, 'suspended');
  release();
  await expect(page.locator('#resume-detail').getByRole('button', { name: 'Resume', exact: true })).toBeEnabled();
  await expect(page).toHaveURL(/\/resume\/suspended$/);
});

test('a daemon without the wiki routes still lists sessions and closes the search box', async ({ page }) => {
  const state = await mount(page, { wiki: null });
  await expect(page.locator('#resume-archived')).toBeHidden();
  await expect(page.locator('#resume-wiki-note')).toBeHidden();
  // The rows are Mjolnir's own and are still listed; only searching them is
  // gone, because the index is what searches.
  await expect(page.locator('#resumable [data-session-id]')).toHaveCount(1);
  await expect(page.locator('#resume-search')).toBeDisabled();
  await expect(page.locator('#resume-search')).toHaveAttribute('placeholder', 'Search is unavailable');
  const asked = state.wikiQueries.length;
  await page.waitForTimeout(600);
  expect(state.wikiQueries.length).toBe(asked);
});

test('the search box stays closed while the first index build runs and opens when it ends', async ({ page }) => {
  const state = await mount(page, {
    wiki: { rows: [], restoredId: 'restored', status: { state: 'indexing', topping_up: true } },
  });
  await expect(page.locator('#resume-search')).toBeDisabled();
  await expect(page.locator('#resume-search')).toHaveAttribute('placeholder', 'Indexing…');
  // Rows keep working while it builds.
  await expect(page.locator('#resumable [data-session-id="suspended"]')).toBeVisible();

  // The page asks again on its own; when the build ends the box opens without
  // the person reopening the page.
  const asked = state.wikiQueries.length;
  state.wiki.status = { state: 'ready', topping_up: false };
  await expect.poll(() => state.wikiQueries.length, { timeout: 15_000 }).toBeGreaterThan(asked);
  await expect(page.locator('#resume-search')).toBeEnabled();
});
