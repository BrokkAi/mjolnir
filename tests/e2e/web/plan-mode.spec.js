const { test, expect } = require('@playwright/test');
const fs = require('node:fs');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 }, serviceWorkers: 'block' });

const SESSION_ID = 'plan-session';

function question(id, message) {
  return {
    id,
    title: 'Choose an architecture',
    description: 'This answer is needed before the turn can continue.',
    message,
    fields: [
      {
        id: 'question_0',
        title: 'Architecture',
        description: 'Pick one, or provide another architecture below.',
        required: true,
        secret: false,
        custom_answer_for: null,
        custom_answer_option: null,
        kind: 'single_select',
        default: null,
        options: [
          { value: 'blue', title: 'Blue', description: 'Blue/green deployment', preview: null },
          { value: 'green', title: 'Green', description: 'Greenfield deployment', preview: null },
        ],
      },
      {
        id: 'question_0_custom',
        title: 'Other',
        description: 'Use this when neither offered architecture fits.',
        required: false,
        secret: false,
        custom_answer_for: 'question_0',
        custom_answer_option: null,
        kind: 'text',
        default: null,
        min_length: null,
        max_length: null,
        pattern: null,
        format: null,
      },
    ],
  };
}

function fixtureSnapshot() {
  return {
    revision: 1,
    generated_at: '2026-09-05T00:00:00Z',
    workspaces: [{ id: 'workspace-1', name: 'Browser tests' }],
    sessions: [
      {
        id: SESSION_ID,
        workspace_id: 'workspace-1',
        title: 'Plan mode browser test',
        harness_kind: 'codex',
        profile_id: 'codex',
        bundle_id: 'bundle-1',
        target_id: 'local',
        state: 'running',
        created_at: '2026-09-05T00:00:00Z',
        updated_at: '2026-09-05T00:00:00Z',
        has_error: false,
        preview: [],
        queued_prompts: [],
        active_user_shells: [],
        pending_elicitations: [],
        conversation_available: true,
        prompt_images_supported: false,
        incompatible_resume_targets: [],
        compatible_resume_targets: ['local'],
        project_label: 'browser-tests',
        project_key: 'browser-tests',
        lifecycle: 'live',
        latest_event_ordinal: 1,
        activity: '',
        operation: null,
        chat_phase: 'idle',
        is_idle: true,
        config_options: [],
        plan_mode_active: false,
        turn_review: null,
        available_commands: [
          { name: 'help', description: 'Show available commands', source: 'mj' },
          { name: 'plan', description: 'Toggle plan mode', source: 'mj' },
          { name: 'implement', description: 'Leave plan mode and implement', source: 'mj' },
        ],
        capabilities: {
          open: true,
          prompt: true,
          run_shell: false,
          cancel_turn: false,
          cancel_operation: false,
          stop: false,
          rename: false,
          resume: false,
          set_config: false,
          set_plan_mode: true,
        },
      },
    ],
    profiles: [{ id: 'codex', harness_kind: 'codex' }],
    targets: [{ id: 'local', kind: 'local', requires_project_directory: false }],
    bundles: [{ id: 'bundle-1', primary_repository: null, repositories: [] }],
    review_config: { enabled: false, tier: 'quick', profile: null },
  };
}

function conversation() {
  return {
    latest_seq: 1,
    window_start_seq: 1,
    reset: false,
    entries: [
      {
        id: 'welcome',
        updated_seq: 1,
        role: 'agent',
        tone: 'agent',
        glyph: '◆',
        label: 'Agent',
        lines: ['Ready to plan.'],
        diffstats: [],
        recorded_at_ms: 1788566400000,
      },
    ],
  };
}

/**
 * Load the real controller shell and viewer modules, but keep the API state
 * local to this test. Returning a mutable fixture lets these checks exercise
 * the same refresh/re-render paths as a live daemon while retaining exact
 * action payloads for inspection.
 */
async function mockViewerApi(page, initialPending = []) {
  const viewerUrl = 'https://viewer.test/';
  const webRoot = path.resolve(__dirname, '../../../mj-controller/src/web');
  // Serve the shipped assets without a daemon. Any unhandled API request is
  // refused, so this suite can never fall through to live auth or providers.
  await page.route('**/*', route => {
    const pathname = new URL(route.request().url()).pathname;
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    if (!['viewer.html', 'viewer.js', 'viewer.css', 'markdown.js', 'tool-output.js', 'manifest.webmanifest', 'icon.svg'].includes(file))
      return route.fulfill({ status: 404, body: '' });
    const asset = file === 'icon.svg' ? path.join(webRoot, '../icons/icon.svg') : path.join(webRoot, file);
    return route.fulfill({ path: asset });
  });
  const state = {
    snapshot: fixtureSnapshot(),
    actions: [],
    drafts: new Map(),
    snapshotRequests: 0,
    readRequests: 0,
    conversationRequests: 0,
    rejectNextPlanMode: false,
    rejectNextAnswer: false,
  };
  state.snapshot.sessions[0].pending_elicitations = initialPending;

  await page.route('**/api/snapshot', route => {
    state.snapshotRequests += 1;
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(state.snapshot),
    });
  });
  // Keep the viewer's EventSource from reaching the daemon. A completed SSE
  // response is enough for the shell to become online; no revision events are
  // needed because tests explicitly trigger the refreshes they assert.
  await page.route('**/api/events', route =>
    route.fulfill({
      status: 200,
      headers: { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' },
      body: ': mocked event stream\n\n',
    }),
  );
  await page.route(`**/api/conversations/${SESSION_ID}/read`, async route => {
    state.readRequests += 1;
    await route.fulfill({ status: 204, body: '' });
  });
  await page.route(`**/api/conversations/${SESSION_ID}*`, async route => {
    state.conversationRequests += 1;
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(conversation()),
    });
  });
  await page.route(`**/api/sessions/${SESSION_ID}/client-state`, async route => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        draft: state.drafts.get(SESSION_ID) || '',
        through_event_ordinal: 0,
      }),
    });
  });
  await page.route(`**/api/sessions/${SESSION_ID}/draft`, async route => {
    const body = JSON.parse(route.request().postData() || '{}');
    state.drafts.set(SESSION_ID, body.draft || '');
    await route.fulfill({ status: 204, body: '' });
  });
  await page.route('**/api/actions', async route => {
    const body = JSON.parse(route.request().postData() || '{}');
    state.actions.push(body);
    if (body.action === 'set-plan-mode') {
      if (state.rejectNextPlanMode) {
        state.rejectNextPlanMode = false;
        await route.fulfill({
          status: 409,
          contentType: 'application/json',
          body: JSON.stringify({ error: 'plan mode change rejected' }),
        });
        return;
      }
      state.snapshot.sessions[0].plan_mode_active = body.active;
      await route.fulfill({ status: 202, body: '' });
      return;
    }
    if (body.action === 'respond-elicitation') {
      if (state.rejectNextAnswer) {
        state.rejectNextAnswer = false;
        await route.fulfill({ status: 409, contentType: 'application/json', body: JSON.stringify({ error: 'answer temporarily rejected' }) });
        return;
      }
      const pending = state.snapshot.sessions[0].pending_elicitations;
      const index = pending.findIndex(item => item.id === body.elicitation_id);
      if (index >= 0) {
        if (body.elicitation_id === 'enum-question') {
          pending.splice(index, 1, question('custom-question', 'Name the custom architecture.'));
        } else {
          pending.splice(index, 1);
        }
      }
      await route.fulfill({ status: 202, body: '' });
      return;
    }
    if (body.action === 'prompt') {
      await route.fulfill({ status: 202, body: '' });
      return;
    }
    await route.fulfill({ status: 202, body: '' });
  });

  await page.goto(`${viewerUrl}#workspace/workspace-1`);
  await expect(page.locator('#app')).toBeVisible();
  await expect(page.locator('#sessions .session')).toHaveCount(1);
  await page.locator('#sessions .session h3').click();
  await expect(page).toHaveURL(/#conversation\/plan-session$/);
  await expect(page.locator('#conversation-title')).toHaveText('Plan mode browser test');
  return state;
}

async function waitForActionCount(state, count) {
  await expect.poll(() => state.actions.length).toBe(count);
}

test('plan command discovers, toggles, sends a request, and retries a rejected action', async ({ page }) => {
  const state = await mockViewerApi(page);
  const prompt = page.locator('#prompt-text');

  // The palette is part of the actual composer interaction: the first Enter
  // accepts /plan, while the second runs the local command.
  await prompt.fill('/pl');
  await expect(page.locator('#command-palette')).toContainText('/plan');
  await page.keyboard.press('Enter');
  await expect(prompt).toHaveText('/plan ');
  await page.keyboard.press('Enter');
  await waitForActionCount(state, 1);
  expect(state.actions[0]).toEqual({
    action: 'set-plan-mode',
    session_id: SESSION_ID,
    active: true,
  });
  await expect(page.locator('#conversation-state')).toContainText('plan');

  // A command with a trailing instruction is two ordered daemon actions: the
  // mode transition first, and then the user's request in that mode.
  await prompt.fill('/plan inspect deployment safety');
  await page.keyboard.press('Enter');
  await waitForActionCount(state, 3);
  expect(state.actions.slice(1)).toEqual([
    { action: 'set-plan-mode', session_id: SESSION_ID, active: false },
    { action: 'prompt', session_id: SESSION_ID, text: 'inspect deployment safety', images: [] },
  ]);
  await expect(prompt).toHaveText('');

  // A refused action leaves the command in the composer and leaves the mode
  // unchanged. Re-entering it is the user-visible retry path.
  state.rejectNextPlanMode = true;
  await prompt.fill('/plan ');
  await page.keyboard.press('Enter');
  await waitForActionCount(state, 4);
  await expect(page.locator('#conversation-error')).toHaveText('plan mode change rejected');
  await expect(prompt).toHaveText('/plan ');
  await page.keyboard.press('Enter');
  await waitForActionCount(state, 5);
  expect(state.actions[4]).toEqual({
    action: 'set-plan-mode',
    session_id: SESSION_ID,
    active: true,
  });
  await expect(page.locator('#conversation-error')).toHaveText('');
  await expect(page.locator('#conversation-state')).toContainText('plan');
  await prompt.fill('/implement ');
  await page.keyboard.press('Enter');
  await waitForActionCount(state, 6);
  expect(state.actions[5]).toEqual({ action: 'set-plan-mode', session_id: SESSION_ID, active: false });
  await expect(page.locator('#conversation-state')).not.toContainText('plan');
});

test('long question forms scroll without pushing answer controls or the composer off the phone', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 568 });
  const long = question('long-question', 'Answer these three questions.');
  long.fields = [0, 1, 2].map(index => ({
    ...long.fields[0], id: `question_${index}`, title: `Question ${index + 1}`,
    options: Array.from({ length: 5 }, (_, option) => ({
      value: String(option), title: `Choice ${index + 1}.${option + 1}`,
      description: 'An option with enough detail to wrap on a narrow phone.',
    })),
  }));
  const state = await mockViewerApi(page, [long]);
  const panel = page.locator('#elicitations');
  expect(await panel.evaluate(node => node.scrollHeight > node.clientHeight)).toBe(true);
  await panel.getByRole('radio', { name: /^Choice 3.5/ }).check();
  await panel.getByRole('button', { name: 'Send answer', exact: true }).scrollIntoViewIfNeeded();
  const send = await page.locator('#send-button').boundingBox();
  expect(send.y + send.height).toBeLessThanOrEqual(569);
  await panel.getByRole('button', { name: 'Send answer', exact: true }).click();
  await waitForActionCount(state, 1);
  expect(state.actions[0].response.content).toEqual({ question_0: '0', question_1: '0', question_2: '4' });
});

test('optional choices start unanswered and can be cleared after selection', async ({ page }) => {
  const optional = question('optional-question', 'Choose an optional architecture.');
  optional.fields[0].required = false;
  const state = await mockViewerApi(page, [optional]);
  const card = page.locator('#elicitations .elicitation');
  await expect(card.locator('input[type="radio"]:checked')).toHaveCount(0);
  await card.getByRole('radio', { name: /^Green/ }).check();
  await card.getByRole('radio', { name: 'No answer', exact: true }).check();
  await card.getByRole('button', { name: 'Send answer', exact: true }).click();
  await waitForActionCount(state, 1);
  expect(state.actions[0].response).toEqual({ action: 'accept', content: {} });
});

test('elicitation enum and custom answers submit exact content and survive snapshot refreshes', async ({ page }) => {
  const state = await mockViewerApi(page, [question('enum-question', 'Which deployment should be used?')]);
  const card = page.locator('#elicitations .elicitation');
  await expect(card).toContainText('Which deployment should be used?');

  // The complete option row is the touch target, while the input remains a
  // native radio for keyboard and assistive-technology semantics.
  const green = card.locator('label.choice-option').filter({ hasText: /^Green/ });
  await green.click();
  await expect(green.locator('input')).toBeChecked();
  await card.getByRole('button', { name: 'Send answer' }).click();
  await waitForActionCount(state, 1);
  expect(state.actions[0]).toEqual({
    action: 'respond-elicitation',
    session_id: SESSION_ID,
    elicitation_id: 'enum-question',
    response: { action: 'accept', content: { question_0: 'green' } },
  });

  await expect(card).toContainText('Name the custom architecture.');
  const custom = page.locator('#elicitations .elicitation');
  await custom.locator('input[type="text"]').fill('Canary');
  const beforeAnswerRefresh = state.snapshotRequests;
  state.snapshot.revision += 1;
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect.poll(() => state.snapshotRequests).toBeGreaterThan(beforeAnswerRefresh);
  await expect(custom.locator('input[type="text"]')).toHaveValue('Canary');
  await expect(custom.locator('input[type="text"]')).toBeFocused();
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390);

  state.rejectNextAnswer = true;
  await custom.getByRole('button', { name: 'Send answer' }).click();
  await waitForActionCount(state, 2);
  await expect(page.locator('#conversation-error')).toHaveText('answer temporarily rejected');
  await expect(custom.locator('input[type="text"]')).toHaveValue('Canary');
  await expect(custom.getByRole('button', { name: 'Send answer' })).toBeEnabled();
  await custom.getByRole('button', { name: 'Send answer' }).click();
  await waitForActionCount(state, 3);
  expect(state.actions[2]).toEqual(state.actions[1]);
  expect(state.actions[1]).toEqual({
    action: 'respond-elicitation',
    session_id: SESSION_ID,
    elicitation_id: 'custom-question',
    response: { action: 'accept', content: { question_0_custom: 'Canary' } },
  });
  await expect(page.locator('#elicitations .elicitation')).toHaveCount(0);

  // The live composer is intentionally independent from elicitation cards.
  // Mutating and refreshing the snapshot must not discard a half-written
  // prompt, and a reload must restore the daemon-backed copy as well.
  const draft = 'keep this request while the snapshot changes';
  await page.locator('#prompt-text').fill(draft);
  await expect.poll(() => state.drafts.get(SESSION_ID)).toBe(draft);
  const snapshotsBefore = state.snapshotRequests;
  state.snapshot.revision += 1;
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect.poll(() => state.snapshotRequests).toBeGreaterThan(snapshotsBefore);
  await expect(page.locator('#prompt-text')).toHaveText(draft);

  await page.reload();
  await expect(page.locator('#conversation-title')).toHaveText('Plan mode browser test');
  await expect(page.locator('#prompt-text')).toHaveText(draft);
});

function multiQuestion(id, message) {
  return {
    id,
    title: 'Choose deployment regions',
    description: 'Select the two regions that should receive this release.',
    message,
    fields: [
      {
        id: 'regions',
        title: 'Regions',
        description: 'Exactly two regions are required.',
        required: true,
        secret: false,
        custom_answer_for: null,
        custom_answer_option: null,
        kind: 'multi_select',
        default: [],
        min_items: 2,
        max_items: 2,
        options: [
          { value: 'us-east', title: 'US East', description: 'Virginia' },
          { value: 'eu-west', title: 'EU West', description: 'Ireland' },
          { value: 'ap-south', title: 'AP South', description: 'Mumbai' },
        ],
      },
      {
        id: 'regions_custom',
        title: 'Other region set',
        description: 'Use this when the offered regions do not fit.',
        required: false,
        secret: false,
        custom_answer_for: 'regions',
        custom_answer_option: null,
        kind: 'text',
        default: null,
        min_length: null,
        max_length: null,
        pattern: null,
        format: null,
      },
    ],
  };
}

test('multi-select choices use full-row phone taps, survive refresh, and retry after rejection', async ({ page }) => {
  const state = await mockViewerApi(page, [multiQuestion('regions-question', 'Where should it deploy?')]);
  const card = page.locator('#elicitations .elicitation');
  const options = card.locator('label.choice-option');
  await expect(options).toHaveCount(3);

  // Each row, including its description, is a reliable mobile-sized target.
  for (let index = 0; index < 3; index += 1) {
    const box = await options.nth(index).boundingBox();
    expect(box.height, `choice row ${index} is too short`).toBeGreaterThanOrEqual(44);
    expect(box.width, `choice row ${index} is too narrow`).toBeGreaterThanOrEqual(44);
  }
  await options.nth(0).click();
  await expect(options.nth(0).locator('input')).toBeChecked();

  // The minimum is enforced before any daemon action is sent.
  await card.getByRole('button', { name: 'Send answer' }).click();
  await expect.poll(() => state.actions.length).toBe(0);
  await options.nth(1).click();
  await expect(options.nth(1).locator('input')).toBeChecked();

  // A snapshot refresh must not reconstruct a live card or lose either check.
  const beforeRefresh = state.snapshotRequests;
  state.snapshot.revision += 1;
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect.poll(() => state.snapshotRequests).toBeGreaterThan(beforeRefresh);
  await expect(options.nth(0).locator('input')).toBeChecked();
  await expect(options.nth(1).locator('input')).toBeChecked();

  state.rejectNextAnswer = true;
  await card.getByRole('button', { name: 'Send answer' }).click();
  await waitForActionCount(state, 1);
  await expect(page.locator('#conversation-error')).toHaveText('answer temporarily rejected');
  await expect(options.nth(0).locator('input')).toBeChecked();
  await expect(options.nth(1).locator('input')).toBeChecked();
  await expect(card.getByRole('button', { name: 'Send answer' })).toBeEnabled();

  await card.getByRole('button', { name: 'Send answer' }).click();
  await waitForActionCount(state, 2);
  expect(state.actions[1]).toEqual({
    action: 'respond-elicitation',
    session_id: SESSION_ID,
    elicitation_id: 'regions-question',
    response: { action: 'accept', content: { regions: ['us-east', 'eu-west'] } },
  });
  await expect(page.locator('#elicitations .elicitation')).toHaveCount(0);
});

test('custom multi-select answers bypass owner constraints until cleared', async ({ page }) => {
  const state = await mockViewerApi(page, [multiQuestion('custom-regions-question', 'Which regions should it cover?')]);
  const card = page.locator('#elicitations .elicitation');
  const ownerInput = card.locator('label.choice-option input').first();
  const options = card.locator('label.choice-option');
  const custom = card.locator('input[type="text"]');

  await expect(ownerInput).toHaveJSProperty('validationMessage', 'Select at least 2 option(s).');
  await options.nth(0).click();
  await options.nth(1).click();
  await options.nth(2).click();
  await expect(ownerInput).toHaveJSProperty('validationMessage', 'Select at most 2 option(s).');
  await custom.fill('Worldwide');
  await expect(ownerInput).toHaveJSProperty('validationMessage', '');

  // Clearing the replacement restores the required owner validation.
  await custom.fill('');
  await expect(ownerInput).toHaveJSProperty('validationMessage', 'Select at most 2 option(s).');
  for (let index = 0; index < 3; index += 1) await options.nth(index).click();
  await expect(ownerInput).toHaveJSProperty('validationMessage', 'Select at least 2 option(s).');
  await card.getByRole('button', { name: 'Send answer' }).click();
  await expect.poll(() => state.actions.length).toBe(0);

  await custom.fill('Worldwide');
  await card.getByRole('button', { name: 'Send answer' }).click();
  await waitForActionCount(state, 1);
  expect(state.actions[0]).toEqual({
    action: 'respond-elicitation',
    session_id: SESSION_ID,
    elicitation_id: 'custom-regions-question',
    response: { action: 'accept', content: { regions_custom: 'Worldwide' } },
  });
});
