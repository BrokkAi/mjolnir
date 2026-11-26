// Dashboard, notifications inbox and composer: workspace switching, the
// sub-agent virtual workspace, inbox sorting/expansion/dismissal/AFK, and the
// composer's prompt, shell, plan mode, model config, attachments, queued
// prompts and session diff sheet.

const { test, expect } = require('@playwright/test');
const {
  PHONE,
  startServer,
  installCommonRoutes,
  guardedPage,
  signIn,
  postedActions,
} = require('./spec-helpers');
const { snapshot, liveSession, transcript, metadata, profile } = require('./fixtures');

let server;

const NOW = Date.parse('2026-11-25T12:00:00Z');

function attentionSession(id, badge, overrides = {}) {
  return liveSession(id, {
    attention: badge,
    notification_card: {
      since_ms: NOW - 60_000,
      badge,
      entries: [
        {
          completed_ordinal: 3,
          completed_at_ms: NOW - 60_000,
          kind: badge,
          message: { found: `msg-${id}` },
        },
      ],
      dismissal: { read_through: 3 },
      holder: null,
      form: null,
    },
    ...overrides,
  });
}

async function openConversation(page, title) {
  await page.getByTestId('session-card-s1').getByRole('button', { name: title, exact: true }).click();
  await expect(page.getByTestId('conversation-view')).toBeVisible();
}

test.beforeAll(async () => {
  server = await startServer();
  installCommonRoutes(server, {
    transcripts: {
      s1: transcript({ entries: [] }),
    },
  });
});

test.afterAll(async () => {
  await server.close();
});

test.beforeEach(() => {
  server.log.length = 0;
});

test('inbox sorts by urgency, expands the full message, dismisses and toggles AFK', async ({ browser }) => {
  server.setSnapshot(
    snapshot({
      sessions: [
        attentionSession('n1', 'needs_input', { title: 'Question session' }),
        attentionSession('n2', 'important', { title: 'Update session' }),
      ],
    }),
  );
  server.handle('GET', '/api/v1/sessions/n1/notification-message', () => ({
    json: { session_id: 'n1', text: '**Full** question body' },
  }));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await signIn(page, server);
    // With attention outstanding, the implicit route lands on the inbox.
    await expect(page.getByRole('heading', { name: 'Notifications' })).toBeVisible();
    await expect(page.getByText('2 awaiting you')).toBeVisible();

    // needs_input outranks important.
    const cards = page.locator('article[data-testid^="notification-"]');
    await expect(cards.nth(0)).toHaveAttribute('data-testid', 'notification-n1');
    await expect(cards.nth(1)).toHaveAttribute('data-testid', 'notification-n2');

    // The full message loads lazily and renders Markdown.
    await cards.nth(0).getByRole('button', { name: 'Show full message' }).click();
    await expect(cards.nth(0).locator('strong', { hasText: 'Full' })).toBeVisible();
    expect(server.requests('GET', '/api/v1/sessions/n1/notification-message')).toHaveLength(1);
    expect(server.requests('GET', '/api/v1/sessions/n1/notification-message')[0].query.message_id).toBe('msg-n1');

    // Dismiss posts the card's dismissal and removes the item.
    await cards.nth(0).getByRole('button', { name: 'Dismiss' }).click();
    await expect.poll(() =>
      postedActions(server).filter(body => body.action === 'dismiss-notification').length,
    ).toBe(1);
    expect(
      postedActions(server).find(body => body.action === 'dismiss-notification'),
    ).toEqual({ action: 'dismiss-notification', session_id: 'n1', target: { read_through: 3 } });
    await expect(page.locator('article[data-testid="notification-n1"]')).toHaveCount(0);

    // AFK is a single toggle that posts the operator state.
    await page.getByRole('button', { name: 'Go AFK' }).click();
    await expect.poll(() =>
      postedActions(server).filter(body => body.action === 'set-operator-away').length,
    ).toBe(1);
    expect(
      postedActions(server).find(body => body.action === 'set-operator-away'),
    ).toEqual({ action: 'set-operator-away', away: true });
    await expect(page.getByText(/Operator away is on/)).toBeVisible();
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('dashboard switches workspaces and opens the sub-agent virtual workspace', async ({ browser }) => {
  server.setSnapshot(
    snapshot({
      meta: {
        workspaces: [
          { id: 'w1', name: 'First workspace', last_opened_at: '2026-11-25T10:00:00Z' },
          { id: 'w2', name: 'Second workspace', last_opened_at: '2026-11-25T11:00:00Z' },
        ],
      },
      sessions: [
        liveSession('a1', { title: 'Alpha', workspace_id: 'w1' }),
        liveSession('a2', { title: 'Beta', workspace_id: 'w2' }),
        liveSession('p1', { title: 'Parent', workspace_id: 'w1', subagent_session_ids: ['c1'] }),
        liveSession('c1', { title: 'Child', workspace_id: 'w1', subagent_parent_id: 'p1' }),
      ],
    }),
  );
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await signIn(page, server);
    await expect(page.getByRole('heading', { name: 'Workspaces' })).toBeVisible();
    // The all-workspaces dashboard shows parents but hides children.
    await expect(page.getByTestId('session-card-a1')).toBeVisible();
    await expect(page.getByTestId('session-card-a2')).toBeVisible();
    await expect(page.getByTestId('session-card-p1')).toBeVisible();
    await expect(page.getByTestId('session-card-c1')).toHaveCount(0);

    const workspaceNav = page.getByRole('navigation', { name: 'Workspaces' });
    await workspaceNav.getByRole('button', { name: 'Second workspace' }).click();
    await expect(page).toHaveURL(/#workspace\/w2$/);
    await expect(page.getByTestId('session-card-a2')).toBeVisible();
    await expect(page.getByTestId('session-card-a1')).toHaveCount(0);

    await workspaceNav.getByRole('button', { name: 'All workspaces' }).click();
    await expect(page.getByTestId('session-card-a1')).toBeVisible();

    // The sub-agent pill opens the parent's virtual workspace of children.
    await page.getByTestId('session-card-p1').getByRole('button', { name: /1 sub-agent/ }).click();
    await expect(page).toHaveURL(/#subagents\/p1$/);
    await expect(page.getByRole('heading', { name: 'Sub-agents' })).toBeVisible();
    await expect(page.getByTestId('session-card-c1')).toBeVisible();
    await expect(page.getByRole('navigation', { name: 'Breadcrumb' })).toContainText('Parent');
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('composer sends prompts, shell commands, plan mode and model config', async ({ browser }) => {
  server.setSnapshot(
    snapshot({
      sessions: [
        liveSession('s1', {
          title: 'Compose session',
          available_commands: [{ name: 'help', description: 'Show available commands', source: 'mj' }],
          config_options: [
            {
              key: 'model',
              label: 'Model',
              current: 'sonnet',
              choices: [
                { value: 'sonnet', name: 'Sonnet' },
                { value: 'opus', name: 'Opus' },
              ],
            },
          ],
        }),
      ],
    }),
  );
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await signIn(page, server);
    await openConversation(page, 'Compose session');
    const composer = page.getByTestId('composer');
    const message = composer.getByLabel('Message', { exact: true });

    // Slash completion from the session's published commands.
    await message.fill('/he');
    await expect(page.getByTestId('command-palette')).toContainText('/help');
    await page.keyboard.press('Enter');
    await expect(message).toHaveValue('/help ');

    // A plain prompt posts the prompt action and shows the optimistic row.
    await message.fill('hello agent');
    await composer.getByRole('button', { name: 'Send' }).click();
    await expect(page.getByTestId('pending-prompt')).toContainText('hello agent');
    await expect.poll(() =>
      postedActions(server).filter(body => body.action === 'prompt').length,
    ).toBe(1);
    const prompt = postedActions(server).find(body => body.action === 'prompt');
    expect(prompt.session_id).toBe('s1');
    expect(prompt.text).toBe('hello agent');
    expect(prompt.command_id).toBeTruthy();

    // A ! prefix switches to the shell action.
    await message.fill('!ls -la');
    await composer.getByRole('button', { name: 'Send' }).click();
    await expect.poll(() =>
      postedActions(server).filter(body => body.action === 'run-shell').length,
    ).toBe(1);
    const shell = postedActions(server).find(body => body.action === 'run-shell');
    expect(shell).toMatchObject({ action: 'run-shell', session_id: 's1', command: 'ls -la' });

    // Plan mode and the model picker post their config actions.
    await composer.getByRole('button', { name: 'Enable plan mode' }).click();
    await expect.poll(() =>
      postedActions(server).filter(body => body.action === 'set-plan-mode').length,
    ).toBe(1);
    expect(
      postedActions(server).find(body => body.action === 'set-plan-mode'),
    ).toEqual({ action: 'set-plan-mode', session_id: 's1', active: true });

    await composer.getByRole('button', { name: 'Model and configuration' }).click();
    await page.getByRole('button', { name: 'Opus' }).click();
    await expect.poll(() =>
      postedActions(server).filter(body => body.action === 'set-config').length,
    ).toBe(1);
    expect(
      postedActions(server).find(body => body.action === 'set-config'),
    ).toEqual({ action: 'set-config', session_id: 's1', key: 'model', value: 'opus' });
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('attachments upload, queued prompts remove, and the session diff sheet lists files', async ({ browser }) => {
  server.setSnapshot(
    snapshot({
      sessions: [
        liveSession('s1', {
          title: 'Files session',
          queued_prompts: [
            { id: 'q1', text: 'remember this for later', created_at: '2026-11-25T11:00:00Z' },
          ],
        }),
      ],
    }),
  );
  server.handle('POST', '/api/v1/sessions/s1/attachments', () => ({
    json: { file: { sha256: 'abc123', filename: 'notes.txt', mime_type: 'text/plain', size: 5 } },
  }));
  server.handle('GET', '/api/v1/sessions/s1/diff', () => ({
    json: {
      diff: 'diff --git a/src/a.ts b/src/a.ts\n+added line',
      base: 'main',
      head: 'HEAD',
      head_descends_from_base: true,
      tracked_files: [{ path: 'src/a.ts', insertions: 3, deletions: 1, binary: false }],
      untracked_files: ['scratch.txt'],
      untracked_total: 1,
    },
  }));
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await signIn(page, server);
    await openConversation(page, 'Files session');

    // The file input is visually hidden but functional.
    await page.locator('input[type="file"]:not([accept])').setInputFiles({
      name: 'notes.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('hello'),
    });
    await expect(page.getByLabel('Attachments')).toContainText('notes.txt');
    expect(server.requests('POST', '/api/v1/sessions/s1/attachments')).toHaveLength(1);

    // Queued prompts render above the composer and remove individually.
    await expect(page.getByTestId('live-activity-bars')).toContainText('remember this for later');
    await page.getByRole('button', { name: 'Remove queued prompt 1' }).click();
    await expect.poll(() =>
      postedActions(server).filter(body => body.action === 'remove-queued-prompt').length,
    ).toBe(1);
    expect(
      postedActions(server).find(body => body.action === 'remove-queued-prompt'),
    ).toEqual({ action: 'remove-queued-prompt', session_id: 's1', queue_id: 'q1' });

    // The session diff sheet lists tracked and untracked files.
    await page.getByRole('button', { name: 'Session diff' }).click();
    const sheet = page.getByTestId('session-diff-sheet');
    await expect(sheet).toBeVisible();
    await expect(sheet).toContainText('src/a.ts');
    await expect(sheet).toContainText('scratch.txt');
    await expect(sheet).toContainText('+3');
    await sheet.getByRole('button', { name: 'Close' }).click();
    await expect(sheet).toBeHidden();
    await guards.assertClean();
  } finally {
    await context.close();
  }
});
