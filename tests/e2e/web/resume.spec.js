const { test, expect } = require('@playwright/test');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, serviceWorkers: 'block' });

function session(id, overrides = {}) {
  return {
    id,
    title: id,
    state: 'stopped',
    lifecycle: 'stopped',
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
  sessions = [session('stopped', { title: 'Stopped test' })],
  profiles = [{ id: 'alpha', harness_kind: 'codex' }, { id: 'beta', harness_kind: 'claude' }],
  targets = [
    { id: 'local', kind: 'local', requires_project_directory: true, recent_project_directories: [] },
    { id: 'remote', kind: 'ssh', requires_project_directory: true, recent_project_directories: [] },
  ],
  holdAction = null,
  rejectAction = false,
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
  await page.evaluate(() => window.fixtureEvents.dispatchEvent(new Event('revision')));
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

test('resume opens with compact current-workspace rows and searches by session details', async ({ page }) => {
  const sessions = [
    session('newest', { title: 'Newest profile work', project_label: 'alpha-project', last_activity_at_ms: 3_000 }),
    session('older', { title: 'A very long session title that must stay inside the phone viewport without horizontal scrolling', project_label: 'older-project', last_activity_at_ms: 2_000 }),
    session('tie-b', { title: 'Tie B', project_label: 'shared-project', last_activity_at_ms: 1_000 }),
    session('tie-a', { title: 'Tie A', project_label: 'shared-project', last_activity_at_ms: 1_000 }),
    session('other-workspace', { title: 'Other workspace', workspace_id: 'other', last_activity_at_ms: 9_000 }),
  ];
  const state = await mount(page, { sessions, profiles: Array.from({ length: 18 }, (_, index) => ({ id: `profile-${index}`, harness_kind: 'codex' })) });
  await expect(page.locator('#resume-list-view')).toBeVisible();
  await expect(page.locator('#resumable [data-session-id]')).toHaveCount(4);
  await expect(page.locator('#resume-detail-view')).toBeHidden();
  await expect(page.locator('#resumable')).toContainText('Newest profile work');
  await page.screenshot({ path: '/tmp/hel2-resume-populated.png', fullPage: true });
  const boxes = await page.locator('#resumable [data-session-id]').evaluateAll(nodes => nodes.map(node => {
    const row = node.getBoundingClientRect();
    const title = node.querySelector('.resume-session-title').getBoundingClientRect();
    return { bottom: row.bottom, rowWidth: row.width, titleWidth: title.width };
  }));
  expect(boxes.every(box => box.bottom <= 844)).toBe(true);
  expect(boxes.every(box => box.titleWidth > box.rowWidth * 0.85)).toBe(true);
  await expect(page.locator('#resumable select, #resumable input')).toHaveCount(0);
  const ids = await page.locator('#resumable [data-session-id]').evaluateAll(nodes => nodes.map(node => node.dataset.sessionId));
  expect(ids).toEqual(['newest', 'older', 'tie-a', 'tie-b']);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  expect(await page.evaluate(() => document.body.scrollWidth <= window.innerWidth)).toBe(true);

  await page.locator('#resume-search').fill('older-project');
  await expect(page.locator('#resumable [data-session-id]')).toHaveCount(1);
  await expect(page.locator('#resumable [data-session-id="older"]')).toBeVisible();
  await page.locator('#resume-search').fill('alpha');
  await expect(page.locator('#resumable [data-session-id]')).toHaveCount(4);
  await page.locator('#resume-search').fill('does-not-exist');
  await expect(page.locator('#resumable')).toContainText(/no (matching sessions|sessions match)/i);
  await expect(page.locator('#resumable [data-session-id]')).toHaveCount(0);
  await expect.poll(() => state.snapshots).toBeGreaterThan(0);
});

test('selecting a row asks for settings only after selection and Back restores search and focus', async ({ page }) => {
  const state = await mount(page, {
    sessions: [
      session('first', { title: 'First session', last_activity_at_ms: 2_000 }),
      session('second', { title: 'Second session', last_activity_at_ms: 1_000 }),
    ],
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

test('a selected card reflects new errors and queued prompts from later snapshots', async ({ page }) => {
  const state = await mount(page, { sessions: [session('mutating', { title: 'Mutating session' })] });
  await openSession(page, 'mutating');
  const detail = page.locator('#resume-detail');
  await expect(detail.locator('[data-role="resume-queue"]')).toHaveCount(0);
  state.snapshot.sessions[0].has_error = true;
  state.snapshot.sessions[0].queued_prompts = [{ id: 'prompt-1', text: 'queued later' }];
  await refresh(page, state);
  await expect(detail).toContainText(/previous operation reported an error/i);
  await expect(detail.locator('[data-role="resume-queue"]').first()).toBeVisible();
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
  await openSession(page, 'stopped');
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
  await openSession(page, 'stopped');
  await page.locator('#resume-detail').getByRole('button', { name: 'Resume', exact: true }).click();
  await expect.poll(() => state.actions.length).toBe(1);
  await page.locator('#resume-detail-back').click();
  await openSession(page, 'stopped');
  release();
  await expect(page.locator('#resume-detail').getByRole('button', { name: 'Resume', exact: true })).toBeEnabled();
  await expect(page).toHaveURL(/\/resume\/stopped$/);
});
