// Elicitation sheet on a phone viewport: one scroll container, rendered
// Markdown and previews, full-width options, peek/reopen draft preservation,
// and exact respond-elicitation payloads for the three companion roles.

const { test, expect } = require('@playwright/test');
const {
  PHONE,
  startServer,
  installCommonRoutes,
  guardedPage,
  signIn,
  expectNoHorizontalOverflow,
  postedActions,
} = require('./spec-helpers');
const { snapshot, liveSession } = require('./fixtures');

const ALONGSIDE = '\u001falongside';

let server;

function markdownSelectRequest(overrides = {}) {
  return {
    id: 'el-1',
    title: 'Choose an approach',
    message: '**Which approach** should the agent take?\n\n- fast\n- safe\n\nSee `plan.md` first.',
    description: 'Pick *exactly one* option.',
    fields: [
      {
        id: 'choice',
        kind: 'single_select',
        title: 'Approach',
        description: 'This decides **everything** downstream.',
        required: true,
        options: [
          { value: 'fast', title: 'Fast path', description: 'Uses the `cache` aggressively' },
          {
            value: 'safe',
            title: 'Safe path (Recommended)',
            description: 'Checks *every* step twice',
            preview: 'diff --git a/plan.md b/plan.md\n+verify each step',
          },
        ],
      },
      {
        id: 'custom',
        kind: 'text',
        title: 'Custom approach',
        required: false,
        custom_answer_for: 'choice',
      },
    ],
    ...overrides,
  };
}

function snapshotWith(request) {
  return snapshot({
    sessions: [liveSession('s1', { title: 'Asking session', pending_elicitations: [request] })],
  });
}

async function openAskingConversation(page) {
  await signIn(page, server);
  await page.getByTestId('session-card-s1').getByRole('button', { name: 'Asking session', exact: true }).click();
  // Phones auto-open the sheet for a new pending request.
  await expect(page.getByRole('dialog')).toBeVisible();
}

function elicitationResponses() {
  return postedActions(server).filter(body => body && body.action === 'respond-elicitation');
}

test.beforeAll(async () => {
  server = await startServer();
  installCommonRoutes(server);
});

test.afterAll(async () => {
  await server.close();
});

test.beforeEach(() => {
  server.log.length = 0;
});

test('the sheet is one scroll container with rendered Markdown, previews and wide options', async ({ browser }) => {
  server.setSnapshot(snapshotWith(markdownSelectRequest()));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openAskingConversation(page);
    const dialog = page.getByRole('dialog');

    await expect(dialog.getByRole('heading', { name: 'Choose an approach' })).toBeVisible();
    // The prompt renders as Markdown: bold, a list and inline code.
    await expect(dialog.locator('strong', { hasText: 'Which approach' })).toBeVisible();
    await expect(dialog.locator('ul li', { hasText: 'fast' }).first()).toBeVisible();
    await expect(dialog.locator('code', { hasText: 'plan.md' }).first()).toBeVisible();
    // Option descriptions render Markdown too, and the preview block is present.
    await expect(dialog.locator('em', { hasText: 'every' })).toBeVisible();
    await expect(dialog.locator('pre code', { hasText: 'diff --git' })).toBeVisible();
    await expect(dialog.getByText('Recommended')).toBeVisible();

    // Exactly one vertical scrollport inside the dialog: the sheet body.
    const scrollables = await dialog.evaluate(root =>
      [...root.querySelectorAll('*')]
        .filter(element => /(auto|scroll)/.test(getComputedStyle(element).overflowY))
        .map(element => element.getAttribute('data-testid') || element.tagName),
    );
    expect(scrollables).toEqual(['elicitation-sheet-body']);

    // Full-width tappable options on a 390px phone.
    for (const name of ['Fast path', 'Safe path']) {
      const box = await dialog.locator('label', { hasText: name }).first().boundingBox();
      expect(box).toBeTruthy();
      expect(box.width).toBeGreaterThan(280);
    }
    await expectNoHorizontalOverflow(page);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('peek transcript collapses to the dock and Answer restores the draft', async ({ browser }) => {
  server.setSnapshot(snapshotWith(markdownSelectRequest()));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openAskingConversation(page);
    const dialog = page.getByRole('dialog');

    // A custom answer draft: pick the free-text alternative and type.
    await dialog.getByText('Custom approach', { exact: true }).click();
    const textarea = dialog.locator('textarea');
    await expect(textarea).toBeVisible();
    await textarea.fill('my draft answer');

    await dialog.getByRole('button', { name: 'Peek transcript' }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByTestId('elicitation-dock')).toBeVisible();
    // The transcript was never hidden and still scrolls behind the dock.
    await expect(page.getByTestId('conversation-scroll')).toBeVisible();

    await page.getByTestId('elicitation-dock').getByRole('button', { name: 'Answer' }).click();
    await expect(page.getByRole('dialog')).toBeVisible();
    await expect(page.getByRole('dialog').locator('textarea')).toHaveValue('my draft answer');

    // Focusing the textarea keeps the layout intact.
    await page.getByRole('dialog').locator('textarea').focus();
    await expect(page.getByTestId('conversation-scroll')).toBeVisible();
    await expect(page.getByRole('dialog')).toBeVisible();
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('submitting an Alternative answer sends only the companion text', async ({ browser }) => {
  server.setSnapshot(snapshotWith(markdownSelectRequest()));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openAskingConversation(page);
    const dialog = page.getByRole('dialog');
    await dialog.getByText('Custom approach', { exact: true }).click();
    await dialog.locator('textarea').fill('a third way');
    await dialog.getByRole('button', { name: 'Submit' }).click();

    await expect.poll(() => elicitationResponses().length).toBe(1);
    expect(elicitationResponses()[0]).toEqual({
      action: 'respond-elicitation',
      session_id: 's1',
      elicitation_id: 'el-1',
      response: { action: 'accept', content: { custom: 'a third way' } },
    });
    // The answered request leaves the sheet and the dock.
    await expect(page.getByRole('dialog')).toBeHidden();
    await expect(page.getByTestId('elicitation-dock')).toBeHidden();
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('submitting an Alongside answer sends the pick and the companion text', async ({ browser }) => {
  server.setSnapshot(
    snapshotWith(
      markdownSelectRequest({
        id: 'el-2',
        fields: [
          {
            id: 'mode',
            kind: 'single_select',
            title: 'Mode',
            required: true,
            options: [
              { value: 'x', title: 'Mode X' },
              { value: 'y', title: 'Mode Y' },
            ],
          },
          {
            id: 'extra',
            kind: 'text',
            title: 'Extra detail',
            required: false,
            custom_answer_for: 'mode',
            custom_answer_option: ALONGSIDE,
          },
        ],
      }),
    ),
  );
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openAskingConversation(page);
    const dialog = page.getByRole('dialog');
    // The Alongside textarea rides beside the pick without choosing "Other".
    await dialog.locator('textarea').fill('ride along note');
    await dialog.getByText('Mode Y', { exact: true }).click();
    await dialog.getByRole('button', { name: 'Submit' }).click();

    await expect.poll(() => elicitationResponses().length).toBe(1);
    expect(elicitationResponses()[0]).toEqual({
      action: 'respond-elicitation',
      session_id: 's1',
      elicitation_id: 'el-2',
      response: { action: 'accept', content: { mode: 'y', extra: 'ride along note' } },
    });
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('submitting a Detail companion answer sends the option and its detail', async ({ browser }) => {
  server.setSnapshot(
    snapshotWith(
      markdownSelectRequest({
        id: 'el-3',
        fields: [
          {
            id: 'target',
            kind: 'single_select',
            title: 'Target',
            required: true,
            options: [
              { value: 'opt-a', title: 'Option A' },
              { value: 'opt-b', title: 'Option B' },
            ],
          },
          {
            id: 'note',
            kind: 'text',
            title: 'Detail for option B',
            required: false,
            custom_answer_for: 'target',
            custom_answer_option: 'opt-b',
          },
        ],
      }),
    ),
  );
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await openAskingConversation(page);
    const dialog = page.getByRole('dialog');
    // The detail textarea only appears once its option is picked.
    await expect(dialog.locator('textarea')).toHaveCount(0);
    await dialog.getByText('Option B', { exact: true }).click();
    await dialog.locator('textarea').fill('why option b');
    await dialog.getByRole('button', { name: 'Submit' }).click();

    await expect.poll(() => elicitationResponses().length).toBe(1);
    expect(elicitationResponses()[0]).toEqual({
      action: 'respond-elicitation',
      session_id: 's1',
      elicitation_id: 'el-3',
      response: { action: 'accept', content: { target: 'opt-b', note: 'why option b' } },
    });
    await guards.assertClean();
  } finally {
    await context.close();
  }
});
