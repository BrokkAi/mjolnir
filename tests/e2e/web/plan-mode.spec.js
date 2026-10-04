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

function claudeTwoQuestionRequest() {
  const choice = (id, title, description) => ({
    id,
    title,
    description,
    required: false,
    secret: false,
    custom_answer_for: null,
    custom_answer_option: null,
    kind: 'single_select',
    default: null,
    options: [
      { value: 'first', title: 'First choice', description: 'The first offered answer.' },
      { value: 'second', title: 'Second choice', description: 'The second offered answer.' },
    ],
  });
  const other = id => ({
    id,
    title: 'Other',
    description: 'Type your own answer, or add a note to the option you chose above (optional).',
    required: false,
    secret: false,
    custom_answer_for: id.replace('_custom', ''),
    custom_answer_option: null,
    kind: 'text',
    default: null,
    min_length: null,
    max_length: null,
    pattern: null,
    format: null,
  });
  return {
    id: 'claude-two-question',
    message: 'Please answer the following questions.',
    fields: [
      choice('question_0', 'Release', 'Which deployment strategy should I use?'),
      other('question_0_custom'),
      choice('question_1', 'Rollback', 'What should trigger an automatic rollback?'),
      other('question_1_custom'),
    ],
  };
}

function fixtureSnapshot(configOptions = []) {
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
        config_options: configOptions,
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
          interrupt_turn: false,
          cancel_operation: false,
          suspend: false,
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
async function mockViewerApi(page, initialPending = [], configOptions = []) {
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
    snapshot: fixtureSnapshot(configOptions),
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
    { action: 'prompt', command_id: expect.stringMatching(/^web-prompt-/), session_id: SESSION_ID, text: 'inspect deployment safety', images: [] },
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

test('composer renders current model and effort settings and reconciles refresh changes', async ({ page }) => {
  const state = await mockViewerApi(page, [], [
    {
      key: 'model',
      label: 'Model',
      current: 'gpt-5',
      choices: [{ value: 'gpt-5', name: 'GPT-5' }],
    },
    {
      key: 'effort',
      label: 'Effort',
      current: 'xhigh',
      choices: [{ value: 'high', name: 'High' }],
    },
  ]);
  const settings = page.locator('#prompt-settings');

  await expect(settings).toBeVisible();
  await expect(settings).toContainText('Model:');
  await expect(settings).toContainText('GPT-5');
  await expect(settings).toContainText('Effort:');
  await expect(settings).toContainText('xhigh');

  state.snapshot.sessions[0].config_options = [
    {
      key: 'model',
      label: 'Model',
      current: 'gpt-5-mini',
      choices: [{ value: 'gpt-5-mini', name: 'GPT-5 mini' }],
    },
    {
      key: 'effort',
      label: 'Effort',
      current: 'high',
      choices: [{ value: 'high', name: 'High' }],
    },
  ];
  const beforeUpdate = state.snapshotRequests;
  state.snapshot.revision += 1;
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect.poll(() => state.snapshotRequests).toBeGreaterThan(beforeUpdate);
  await expect(settings.locator('.prompt-setting-value').nth(0)).toHaveText('GPT-5 mini');
  await expect(settings.locator('.prompt-setting-value').nth(1)).toHaveText('High');

  state.snapshot.sessions[0].config_options = [
    { key: 'model', label: 'Model', current: null, choices: [] },
    { key: 'effort', label: 'Effort', current: '', choices: [] },
  ];
  const beforeRemoval = state.snapshotRequests;
  state.snapshot.revision += 1;
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect.poll(() => state.snapshotRequests).toBeGreaterThan(beforeRemoval);
  await expect(settings).toBeHidden();
  await expect(settings).toHaveText('');
});

test('long question forms scroll without pushing answer controls or the composer off the phone', async ({ page }, testInfo) => {
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
  // The completed fixture event stream produces a reconnect banner and its
  // unsent welcome row produces a jump button; neither is part of this layout.
  await page.addStyleTag({ content: '#connection, #jump-to-latest { display: none !important; }' });
  const panel = page.locator('#elicitations');
  expect(await panel.evaluate(node => node.scrollHeight > node.clientHeight)).toBe(true);
  await panel.getByRole('button', { name: 'Answer and next', exact: true }).click();
  await expect(panel.locator('.elicitation-progress')).toContainText('Question 2/3');
  expect(state.actions).toHaveLength(0);
  await panel.getByRole('button', { name: 'Answer and next', exact: true }).click();
  const finalChoice = panel.getByRole('radio', { name: /^Choice 3.5/ });
  await finalChoice.check();
  await expect(page.locator('body')).toHaveClass(/elicitation-focused/);
  await finalChoice.evaluate(input => input.blur());
  await expect(page.locator('body')).not.toHaveClass(/elicitation-focused/);
  const restoredGeometry = await page.evaluate(() => {
    const rect = selector => {
      const bounds = document.querySelector(selector).getBoundingClientRect();
      return { top: bounds.top, bottom: bounds.bottom, height: bounds.height };
    };
    return {
      scrollY: window.scrollY,
      conversation: rect('#conversation'),
      panel: rect('#elicitations'),
      composer: rect('#prompt-form'),
      send: rect('#send-button'),
    };
  });
  await fs.promises.writeFile(
    testInfo.outputPath('elicitation-phone-restored.json'),
    JSON.stringify(restoredGeometry, null, 2),
  );
  await panel.getByRole('button', { name: 'Submit all', exact: true }).scrollIntoViewIfNeeded();
  const send = await page.locator('#send-button').boundingBox();
  expect(send.y + send.height).toBeLessThanOrEqual(569);
  await panel.getByRole('button', { name: 'Submit all', exact: true }).click();
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
  await card.getByRole('button', { name: 'Submit all', exact: true }).click();
  await expect(card.getByRole('button', { name: 'Go back', exact: true })).toBeFocused();
  expect(state.actions).toHaveLength(0);
  await card.getByRole('button', { name: 'Submit anyway', exact: true }).click();
  await waitForActionCount(state, 1);
  expect(state.actions[0].response).toEqual({ action: 'accept', content: {} });
});

test('elicitation enum and custom answers submit exact content and survive snapshot refreshes', async ({ page }) => {
  const state = await mockViewerApi(page, [question('enum-question', 'Which deployment should be used?')]);
  const card = page.locator('#elicitations .elicitation');
  await expect(card).toContainText('Which deployment should be used?');
  const messageBeforeForm = await card.locator('.elicitation-message').evaluate(node => {
    const form = node.parentElement.querySelector('form');
    return Boolean(node.compareDocumentPosition(form) & Node.DOCUMENT_POSITION_FOLLOWING);
  });
  expect(messageBeforeForm).toBe(true);

  // The complete option row is the touch target, while the input remains a
  // native radio for keyboard and assistive-technology semantics.
  const green = card.locator('label.choice-option').filter({ hasText: /^Green/ });
  await green.click();
  await expect(green.locator('input')).toBeChecked();
  await card.getByRole('button', { name: 'Submit all' }).click();
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
  await custom.getByRole('button', { name: 'Submit all' }).click();
  await waitForActionCount(state, 2);
  await expect(page.locator('#conversation-error')).toHaveText('answer temporarily rejected');
  await expect(custom.locator('input[type="text"]')).toHaveValue('Canary');
  await expect(custom.getByRole('button', { name: 'Submit all' })).toBeEnabled();
  await custom.getByRole('button', { name: 'Submit all' }).click();
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

test('phone elicitation prompts stay above choices and the focused answer fits beside its actions', async ({ page }, testInfo) => {
  await mockViewerApi(page, [claudeTwoQuestionRequest()]);
  await expect(page.locator('meta[name="viewport"]')).toHaveAttribute(
    'content', /interactive-widget=resizes-content/,
  );
  const card = page.locator('#elicitations .elicitation');
  const prompt = card.locator('.choice-control .elicitation-question').first();
  const firstOption = card.locator('.choice-control .choice-option').first();
  const questionOrder = await prompt.evaluate(node => {
    const firstChoice = node.closest('.choice-control').querySelector('.choice-option');
    return Boolean(node.compareDocumentPosition(firstChoice) & Node.DOCUMENT_POSITION_FOLLOWING);
  });
  expect(questionOrder).toBe(true);
  const promptBox = await prompt.boundingBox();
  const optionBox = await firstOption.boundingBox();
  expect(promptBox.y + promptBox.height).toBeLessThanOrEqual(optionBox.y);
  const colors = await prompt.evaluate(node => ({
    prompt: getComputedStyle(node).color,
    header: getComputedStyle(node.closest('.choice-control').querySelector('legend')).color,
  }));
  expect(colors.prompt).not.toBe(colors.header);
  const messageBeforeForm = await card.locator('.elicitation-message').evaluate(node => {
    const form = node.parentElement.querySelector('form');
    return Boolean(node.compareDocumentPosition(form) & Node.DOCUMENT_POSITION_FOLLOWING);
  });
  expect(messageBeforeForm).toBe(true);

  const otherField = card.locator('label.elicitation-field').first();
  expect(await otherField.evaluate(node => [...node.children].map(child => child.className || child.tagName.toLowerCase())))
    .toEqual(['elicitation-field-label', 'elicitation-question', 'input']);
  const other = otherField.locator('input[type="text"]');
  const subagents = page.locator('#subagents-button');
  await subagents.evaluate(node => node.classList.remove('hidden'));
  await page.screenshot({ path: testInfo.outputPath('elicitation-phone-before.png') });

  await other.focus();
  await expect(page.locator('body')).toHaveClass(/elicitation-focused/);
  await expect(page.locator('#shell-header')).toBeHidden();
  await expect(page.locator('#prompt-form')).toBeHidden();
  await expect(page.locator('#conversation-side')).toBeHidden();
  await expect(subagents).toBeHidden();

  // Moving focus from the text input to one of the card's buttons blurs first;
  // the card remains expanded through that button's click/focus interaction.
  await card.getByRole('button', { name: 'Answer and next', exact: true }).focus();
  await expect(page.locator('body')).toHaveClass(/elicitation-focused/);
  await other.focus();
  await page.setViewportSize({ width: 390, height: 464 });
  await expect(page.locator('body')).toHaveClass(/elicitation-focused/);
  await expect.poll(() => page.evaluate(() =>
    getComputedStyle(document.documentElement).getPropertyValue('--keyboard-inset').trim(),
  )).toBe('0px');
  const measureFocusedAnswer = () => page.evaluate(() => {
    const panel = document.querySelector('#elicitations');
    const input = document.querySelector('#elicitations .elicitation input[type="text"]');
    const actions = document.querySelector('#elicitations .elicitation-actions');
    const panelRect = panel.getBoundingClientRect();
    const inputRect = input.getBoundingClientRect();
    const actionsRect = actions.getBoundingClientRect();
    return {
      viewport: { width: document.documentElement.clientWidth, height: window.innerHeight },
      panel: { top: panelRect.top, bottom: panelRect.bottom },
      input: { top: inputRect.top, bottom: inputRect.bottom },
      actions: { top: actionsRect.top, bottom: actionsRect.bottom },
      keyboardInset: getComputedStyle(document.documentElement).getPropertyValue('--keyboard-inset').trim(),
      bodyClass: document.body.className,
      activeElement: document.activeElement?.outerHTML.slice(0, 120),
      panelScrollTop: panel.scrollTop,
    };
  });
  await expect.poll(async () => {
    const geometry = await measureFocusedAnswer();
    return geometry.input.top >= geometry.panel.top && geometry.input.bottom <= geometry.actions.top &&
      geometry.actions.bottom <= geometry.panel.bottom;
  }).toBe(true);
  const visible = await measureFocusedAnswer();
  await fs.promises.writeFile(
    testInfo.outputPath('elicitation-phone-keyboard.json'),
    JSON.stringify(visible, null, 2),
  );
  expect(visible.input.top).toBeGreaterThanOrEqual(visible.panel.top);
  expect(visible.input.bottom).toBeLessThanOrEqual(visible.actions.top);
  expect(visible.actions.bottom).toBeLessThanOrEqual(visible.panel.bottom);
  await page.screenshot({ path: testInfo.outputPath('elicitation-phone-keyboard.png') });

  await other.evaluate(input => input.blur());
  await expect(page.locator('body')).not.toHaveClass(/elicitation-focused/);
  await expect(page.locator('#prompt-form')).toBeVisible();
  await expect(subagents).toBeVisible();

  // Simulate a browser that keeps the layout viewport tall while only the
  // visual viewport shrinks, as iOS Safari can do without interactive-widget.
  await page.setViewportSize({ width: 390, height: 844 });
  await other.focus();
  await page.evaluate(() => {
    Object.defineProperty(window, 'visualViewport', {
      configurable: true,
      value: { height: 464, offsetTop: 0, addEventListener() {}, removeEventListener() {} },
    });
    window.dispatchEvent(new Event('resize'));
  });
  await expect.poll(() => page.evaluate(() =>
    getComputedStyle(document.documentElement).getPropertyValue('--keyboard-inset').trim(),
  )).toBe('380px');
  const fallbackBody = await page.locator('body').boundingBox();
  expect(fallbackBody.height).toBeCloseTo(464, 0);
  await page.evaluate(() => {
    delete window.visualViewport;
    window.dispatchEvent(new Event('resize'));
  });
  await other.evaluate(input => input.blur());
  await expect(page.locator('body')).not.toHaveClass(/elicitation-focused/);

  // The phone-only focus layout must not alter desktop geometry.
  await page.setViewportSize({ width: 1200, height: 844 });
  const composerBefore = await page.locator('#prompt-form').boundingBox();
  await other.focus();
  await expect(page.locator('#prompt-form')).toBeVisible();
  const composerAfter = await page.locator('#prompt-form').boundingBox();
  expect(composerAfter.x).toBe(composerBefore.x);
  expect(composerAfter.y).toBe(composerBefore.y);
  expect(composerAfter.width).toBe(composerBefore.width);
  expect(composerAfter.height).toBe(composerBefore.height);
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
  await card.getByRole('button', { name: 'Submit all' }).click();
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
  await card.getByRole('button', { name: 'Submit all' }).click();
  await waitForActionCount(state, 1);
  await expect(page.locator('#conversation-error')).toHaveText('answer temporarily rejected');
  await expect(options.nth(0).locator('input')).toBeChecked();
  await expect(options.nth(1).locator('input')).toBeChecked();
  await expect(card.getByRole('button', { name: 'Submit all' })).toBeEnabled();

  await card.getByRole('button', { name: 'Submit all' }).click();
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
  await card.getByRole('button', { name: 'Submit all' }).click();
  await expect.poll(() => state.actions.length).toBe(0);

  await custom.fill('Worldwide');
  await card.getByRole('button', { name: 'Submit all' }).click();
  await waitForActionCount(state, 1);
  expect(state.actions[0]).toEqual({
    action: 'respond-elicitation',
    session_id: SESSION_ID,
    elicitation_id: 'custom-regions-question',
    response: { action: 'accept', content: { regions_custom: 'Worldwide' } },
  });
});

test('submitted content appears while the request is held and a newer draft survives acceptance', async ({ page }) => {
  await mockViewerApi(page);
  let held;
  await page.route('**/api/actions', route => { held = route; });
  const prompt = page.locator('#prompt-text');
  await prompt.fill('show this immediately');
  await page.keyboard.press('Enter');
  const pending = page.locator('#pending-submissions');
  await expect(pending).toContainText('show this immediately');
  await expect(pending).toContainText('Sending');
  await expect(prompt).toHaveText('');
  await prompt.fill('a newer draft');
  await expect.poll(() => Boolean(held)).toBe(true);
  const body = held.request().postDataJSON();
  expect(body.command_id).toMatch(/^web-prompt-/);
  await held.fulfill({ status: 202, body: '' });
  await expect(pending).toContainText('Queued');
  await expect(prompt).toHaveText('a newer draft');
  await page.route(`**/api/conversations/${SESSION_ID}*`, route => {
    if (new URL(route.request().url()).pathname.endsWith('/read')) return route.fallback();
    const projected = conversation();
    projected.reset = true;
    projected.latest_seq = 2;
    projected.entries.push({ id: 2, updated_seq: 2, command_id: body.command_id, role: 'user', tone: 'user', label: 'You', glyph: '›', lines: [body.text] });
    return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(projected) });
  });
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect(pending.locator('article')).toHaveCount(0);
  await expect(page.locator('#conversation-feed')).toContainText('show this immediately');
  await expect(page.locator('#conversation-feed [data-entry-id="2"]')).toHaveCount(1);
  await expect(prompt).toHaveText('a newer draft');
});

test('projection before a lost acknowledgement reconciles only the matching identical prompt', async ({ page }) => {
  const state = await mockViewerApi(page);
  const held = [];
  await page.route('**/api/actions', route => { held.push(route); });
  const prompt = page.locator('#prompt-text');
  for (let i = 0; i < 2; i += 1) {
    await prompt.fill('same text');
    await page.keyboard.press('Enter');
    await expect.poll(() => held.length).toBe(i + 1);
  }
  const first = held[0].request().postDataJSON();
  const second = held[1].request().postDataJSON();
  expect(first.command_id).not.toBe(second.command_id);
  await expect(page.locator('#pending-submissions article')).toHaveCount(2);
  state.snapshot.sessions[0].queued_prompts = [{ id: first.command_id, text: first.text }];
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect(page.locator('#pending-submissions article')).toHaveCount(1);
  await held[0].abort('connectionclosed');
  await expect(page.locator('#pending-submissions')).not.toContainText('Delivery unconfirmed');
  await held[1].fulfill({ status: 409, contentType: 'application/json', body: JSON.stringify({ error: 'session refused the prompt' }) });
  await expect(page.locator('#pending-submissions')).toContainText('Not sent: session refused the prompt');
  await page.locator('#pending-submissions button').click();
  await expect(prompt).toHaveText('same text');
});

test('a failed mode prerequisite leaves its follow-up unsent and recoverable', async ({ page }) => {
  await mockViewerApi(page);
  const actions = [];
  await page.route('**/api/actions', route => {
    actions.push(route.request().postDataJSON());
    return route.fulfill({ status: 500, contentType: 'application/json', body: JSON.stringify({ error: 'mode change failed' }) });
  });
  const prompt = page.locator('#prompt-text');
  await prompt.fill('/plan inspect deployment safety');
  await page.keyboard.press('Enter');
  await expect(page.locator('#pending-submissions')).toContainText('Not sent: mode change failed');
  expect(actions.map(action => action.action)).toEqual(['set-plan-mode']);
  await page.locator('#pending-submissions button').click();
  await expect(prompt).toHaveText('/plan inspect deployment safety');
});

test('pending content stays with its session and does not alter drafts after navigation', async ({ page }) => {
  await mockViewerApi(page);
  let held;
  await page.route('**/api/actions', route => { held = route; });
  const prompt = page.locator('#prompt-text');
  await prompt.fill('pending through navigation');
  await page.keyboard.press('Enter');
  await expect(page.locator('#pending-submissions')).toContainText('Sending');
  await expect.poll(() => Boolean(held)).toBe(true);
  await page.evaluate(() => { location.hash = '#workspace/workspace-1'; });
  await held.abort('connectionclosed');
  await page.locator('#sessions .session h3').click();
  await expect(page.locator('#pending-submissions')).toContainText('pending through navigation');
  await expect(page.locator('#pending-submissions')).toContainText('Delivery unconfirmed');
  await expect(prompt).toHaveText('');
});

function guidedQuestions(id, count = 3) {
  const request = question(id, 'Consider each answer before submitting.');
  request.fields = Array.from({ length: count }, (_, index) => ({
    ...request.fields[0],
    id: `answer_${index}`,
    title: `Decision ${index + 1}`,
    required: false,
    default: 'blue',
  }));
  return request;
}

test('guided navigation does not accept defaults and partial submission requires a separate action', async ({ page }) => {
  const state = await mockViewerApi(page, [guidedQuestions('guided-partial')]);
  const card = page.locator('#elicitations .elicitation');
  const progress = card.locator('.elicitation-progress');
  await expect(progress).toHaveText('Question 1/3 · 3 unanswered');
  await expect(card.getByRole('group')).toHaveCount(1);
  await expect(card.getByRole('button', { name: 'Submit all', exact: true })).toHaveCount(0);
  await card.getByRole('button', { name: 'Next', exact: true }).click();
  await card.getByRole('button', { name: 'Next', exact: true }).click();
  await expect(progress).toHaveText('Question 3/3 · 3 unanswered');
  await card.getByRole('button', { name: 'Submit all', exact: true }).click();
  await expect(card.getByRole('alert')).toContainText('2 unanswered questions');
  await expect(card.getByRole('alert')).toContainText('Decision 1');
  await expect(card.getByRole('alert')).toContainText('Decision 2');
  expect(state.actions).toHaveLength(0);
  await expect(card.getByRole('button', { name: 'Go back', exact: true })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(progress).toHaveText('Question 1/3 · 2 unanswered');
  expect(state.actions).toHaveLength(0);
  await card.getByRole('button', { name: 'Next', exact: true }).click();
  await card.getByRole('button', { name: 'Next', exact: true }).click();
  await card.getByRole('button', { name: 'Submit all', exact: true }).click();
  await page.keyboard.press('Escape');
  await expect(progress).toHaveText('Question 1/3 · 2 unanswered');
  await card.getByRole('button', { name: 'Next', exact: true }).click();
  await card.getByRole('button', { name: 'Next', exact: true }).click();
  await card.getByRole('button', { name: 'Submit all', exact: true }).click();
  await card.getByRole('button', { name: 'Submit anyway', exact: true }).click();
  await waitForActionCount(state, 1);
  expect(state.actions[0].response).toEqual({ action: 'accept', content: { answer_2: 'blue' } });
});

test('confirmed answers survive backtracking and refresh but edits require confirmation again', async ({ page }) => {
  const state = await mockViewerApi(page, [guidedQuestions('guided-edit', 2)]);
  const card = page.locator('#elicitations .elicitation');
  const progress = card.locator('.elicitation-progress');
  await card.getByRole('button', { name: 'Answer and next', exact: true }).click();
  await expect(progress).toHaveText('Question 2/2 · 1 unanswered');
  await card.getByRole('button', { name: 'Previous', exact: true }).click();
  await expect(progress).toHaveText('Question 1/2 · 1 unanswered');
  await card.getByRole('radio', { name: /^Green/ }).check();
  await expect(progress).toHaveText('Question 1/2 · 2 unanswered');
  const revision = state.snapshotRequests;
  state.snapshot.revision += 1;
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect.poll(() => state.snapshotRequests).toBeGreaterThan(revision);
  await expect(card.getByRole('radio', { name: /^Green/ })).toBeChecked();
  await expect(progress).toHaveText('Question 1/2 · 2 unanswered');
  await card.getByRole('button', { name: 'Next', exact: true }).click();
  await card.getByRole('button', { name: 'Submit all', exact: true }).click();
  expect(state.actions).toHaveLength(0);
  await card.getByRole('button', { name: 'Go back', exact: true }).click();
  await card.getByRole('button', { name: 'Answer and next', exact: true }).click();
  await card.getByRole('button', { name: 'Submit all', exact: true }).click();
  await waitForActionCount(state, 1);
  expect(state.actions[0].response.content).toEqual({ answer_0: 'green', answer_1: 'blue' });
});

test('skipped required defaults return to that question instead of submitting', async ({ page }) => {
  const request = guidedQuestions('guided-required', 2);
  request.fields[0].required = true;
  const state = await mockViewerApi(page, [request]);
  const card = page.locator('#elicitations .elicitation');
  await card.getByRole('button', { name: 'Next', exact: true }).click();
  await card.getByRole('button', { name: 'Submit all', exact: true }).click();
  await expect(card.locator('.elicitation-progress')).toHaveText('Question 1/2 · 1 unanswered');
  await expect(card).toContainText('Answer Decision 1 before submitting.');
  expect(state.actions).toHaveLength(0);
});

test('question replacement resets confirmation even when the request id is reused', async ({ page }) => {
  const state = await mockViewerApi(page, [guidedQuestions('reused-id', 2)]);
  const card = page.locator('#elicitations .elicitation');
  await card.getByRole('button', { name: 'Answer and next', exact: true }).click();
  state.snapshot.sessions[0].pending_elicitations[0].fields[0].title = 'Replacement decision';
  const revision = state.snapshotRequests;
  state.snapshot.revision += 1;
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect.poll(() => state.snapshotRequests).toBeGreaterThan(revision);
  await expect(card.locator('.elicitation-progress')).toHaveText('Question 1/2 · 2 unanswered');
  await expect(card.getByRole('group')).toHaveAccessibleName('Replacement decision');
  expect(state.actions).toHaveLength(0);
});

test('multiline prompts wrap on narrow phones and the full question remains reachable', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 568 });
  const request = guidedQuestions('multiline', 1);
  request.fields[0].title = 'How should we handle JPEG support given that our existing linearization differs?\n'
    + 'A long explanation of the differences. '.repeat(20) + '\nFinal authored line';
  await mockViewerApi(page, [request]);
  const card = page.locator('#elicitations .elicitation');
  const legend = card.locator('legend');
  await expect(legend).toHaveCSS('white-space', 'pre-wrap');
  const geometry = await legend.evaluate(node => ({
    width: node.getBoundingClientRect().width,
    height: node.getBoundingClientRect().height,
    scrollWidth: node.scrollWidth,
    clientWidth: node.clientWidth,
  }));
  expect(geometry.width).toBeLessThan(320);
  expect(geometry.height).toBeGreaterThan(200);
  expect(geometry.scrollWidth).toBeLessThanOrEqual(geometry.clientWidth + 1);
  await expect(legend).toContainText('Final authored line');
  const panel = page.locator('#elicitations');
  await panel.evaluate(node => { node.scrollTop = node.scrollHeight; });
  await expect(card.getByRole('button', { name: 'Submit all', exact: true })).toBeInViewport();
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(320);
});

test('keyboard submission accepts false booleans and an empty form sends only once', async ({ page }) => {
  const request = guidedQuestions('boolean-answer', 1);
  request.fields[0] = {
    ...request.fields[0], kind: 'boolean', default: false, required: true,
  };
  const state = await mockViewerApi(page, [request, { id: 'empty-form', message: 'Continue?', fields: [] }]);
  const boolean = page.locator('#elicitations .elicitation').first();
  await boolean.getByRole('button', { name: 'Submit all', exact: true }).focus();
  await page.keyboard.press('Enter');
  await waitForActionCount(state, 1);
  expect(state.actions[0].response.content).toEqual({ answer_0: false });
  const empty = page.locator('#elicitations .elicitation').filter({ hasText: 'Continue?' });
  await empty.locator('form').evaluate(form => {
    form.requestSubmit();
    form.requestSubmit();
  });
  await waitForActionCount(state, 2);
  expect(state.actions[1].response).toEqual({ action: 'accept', content: {} });
});
