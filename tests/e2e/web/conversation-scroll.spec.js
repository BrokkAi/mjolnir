// Conversation scroll integrity on a phone viewport: streaming must not
// rebuild earlier rows or yank a detached reader, tool-card expansion and
// history prepends must keep the viewport anchored, and a reply taller than
// the viewport pauses auto-follow instead of dragging the reader down.

const { test, expect } = require('@playwright/test');
const {
  PHONE,
  startServer,
  installCommonRoutes,
  guardedPage,
  signIn,
} = require('./spec-helpers');
const { snapshot, liveSession, transcriptEntry, transcript, tallLines } = require('./fixtures');

const SCROLLER = '[data-testid="conversation-scroll"]';
const ROWS = '[data-testid="transcript-rows"]';
const PILL = '[data-testid="jump-to-bottom"]';

let server;

/** Mutable stream state: the mock conversation endpoint reads from this. */
let streamLines;
let streamSeq;

function streamEntry() {
  return transcriptEntry(99, {
    role: 'agent',
    label: 'Assistant',
    stable_id: 'stream-row',
    updated_seq: streamSeq,
    lines: [...streamLines],
  });
}

function conversationBody(query) {
  const after = Number(query.after_seq || 0);
  if (after > 0) {
    return {
      latest_seq: streamSeq,
      presentation_key: 'pk-1',
      window_start_seq: 1,
      reset: false,
      entries: [streamEntry()],
    };
  }
  return transcript({
    latestSeq: streamSeq,
    presentationKey: 'pk-1',
    entries: [
      ...Array.from({ length: 10 }, (_, index) =>
        // Live positions start above the stored history the load-earlier test pages in.
        transcriptEntry(index + 11, {
          role: index % 2 === 0 ? 'agent' : 'user',
          label: index % 2 === 0 ? 'Assistant' : 'You',
          stable_id: `row-${index + 1}`,
          lines: tallLines(6, `entry ${index + 1}`),
        }),
      ),
      streamEntry(),
    ],
  });
}

/** One streamed chunk: more lines on the same streaming row. */
function streamMore(count) {
  const start = streamLines.length;
  for (let index = 0; index < count; index += 1) {
    streamLines.push(`stream chunk ${start + index + 1}: text that keeps arriving while the agent works`);
  }
  streamSeq += 1;
  server.publishDelta({ conversationSeqs: { s1: streamSeq } });
}

async function openConversation(page) {
  await signIn(page, server);
  await page.getByTestId('session-card-s1').getByRole('button', { name: 'Stream session', exact: true }).click();
  await expect(page.getByTestId('transcript-rows').locator('article').first()).toBeVisible();
  // The initial snap lands on the end of the transcript.
  await expect.poll(() => page.evaluate(() => {
    const el = document.querySelector('[data-testid="conversation-scroll"]');
    return el.scrollHeight - el.scrollTop - el.clientHeight;
  })).toBeLessThan(4);
}

test.beforeAll(async () => {
  server = await startServer();
  installCommonRoutes(server, {
    transcripts: { s1: conversationBody },
  });
});

test.afterAll(async () => {
  await server.close();
});

test.beforeEach(() => {
  server.log.length = 0;
  streamLines = tallLines(4, 'stream opening');
  streamSeq = 50;
  server.setSnapshot(
    snapshot({
      sessions: [
        liveSession('s1', {
          title: 'Stream session',
          chat_phase: 'running',
          is_idle: false,
          verdict: { working: true, active_work: true, idle: false },
        }),
      ],
    }),
  );
});

test('streaming deltas update the streaming row without rebuilding earlier rows', async ({ browser }) => {
  const { context, page, guards } = await guardedPage(browser, { viewport: PHONE, allowSignedOut401: true });
  try {
    await openConversation(page);
    // Capture the live DOM nodes of every rendered row.
    await page.evaluate(selector => {
      window.__rowsBefore = [...document.querySelectorAll(`${selector} article`)];
    }, ROWS);

    streamMore(8);
    await expect(page.locator(ROWS)).toContainText('stream chunk 12');

    const identity = await page.evaluate(selector => {
      const now = [...document.querySelectorAll(`${selector} article`)];
      return {
        countBefore: window.__rowsBefore.length,
        countNow: now.length,
        identical: window.__rowsBefore.every((node, index) => now[index] === node),
      };
    }, ROWS);
    expect(identity.countBefore).toBeGreaterThan(5);
    expect(identity.countNow).toBe(identity.countBefore);
    expect(identity.identical).toBe(true);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('a detached reader keeps scrollTop steady while deltas arrive', async ({ browser }) => {
  const { context, page, guards } = await guardedPage(browser, { viewport: PHONE, allowSignedOut401: true });
  try {
    await openConversation(page);
    // Scroll up hard: the reader detaches and the jump pill appears.
    await page.evaluate(selector => {
      const el = document.querySelector(selector);
      el.scrollTop = 0;
      el.dispatchEvent(new Event('scroll'));
    }, SCROLLER);
    await expect(page.locator(PILL)).toBeVisible();

    streamMore(6);
    await expect(page.locator(ROWS)).toContainText('stream chunk 10');

    const scrollTop = await page.evaluate(selector => document.querySelector(selector).scrollTop, SCROLLER);
    expect(scrollTop).toBe(0);
    // The pill offers the way back and reports the streaming state.
    await expect(page.locator(PILL)).toContainText('Jump to bottom');
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('expanding a tool card releases follow and keeps the card header anchored', async ({ browser }) => {
  const { context, page, guards } = await guardedPage(browser, { viewport: PHONE, allowSignedOut401: true });
  try {
    // Add a tool card into the transcript for this test only.
    server.handle('GET', '/api/v1/conversations/s1', ({ entry }) => {
      const body = conversationBody(entry.query);
      if (Number(entry.query.after_seq || 0) === 0) {
        body.entries.splice(5, 0, transcriptEntry(50, {
          role: 'tool',
          label: 'bash npm test',
          glyph: 'bash',
          stable_id: 'tool-card-1',
          tool_status: 'done',
          lines: ['summary line'],
          disclosures: [{ summary: ['exit 0'], body: tallLines(30, 'output') }],
        }));
      }
      return { json: body };
    });
    await openConversation(page);

    const card = page.getByTestId('tool-card');
    await card.evaluate(element => element.scrollIntoView({ block: 'center' }));
    const button = card.getByRole('button');
    const beforeTop = await button.evaluate(element => element.getBoundingClientRect().top);
    await button.click();
    await expect(page.getByTestId('tool-card-body')).toBeVisible();
    await expect(page.getByTestId('tool-card-body')).toContainText('output 30');
    const afterTop = await button.evaluate(element => element.getBoundingClientRect().top);
    expect(Math.abs(afterTop - beforeTop)).toBeLessThanOrEqual(2);
    // Expansion detached the reader: the pill is offering the end again.
    await expect(page.locator(PILL)).toBeVisible();
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('loading earlier messages prepends history without moving the anchor row', async ({ browser }) => {
  const { context, page, guards } = await guardedPage(browser, { viewport: PHONE, allowSignedOut401: true });
  try {
    server.handle('GET', '/api/v1/sessions/s1/history', () => ({
      json: {
        items: Array.from({ length: 5 }, (_, index) => ({
          stable_id: `hist-${index + 1}`,
          position: index + 1,
          role: 'Agent',
          text: tallLines(6, `history ${index + 1}`).join('\n'),
          recorded_at_ms: Date.parse('2026-11-25T09:00:00Z') + index * 1000,
        })),
        before: { position: 1, stable_id: 'hist-1' },
        frontier: 0,
      },
    }));
    await openConversation(page);
    await page.evaluate(selector => {
      const el = document.querySelector(selector);
      el.scrollTop = 0;
      el.dispatchEvent(new Event('scroll'));
    }, SCROLLER);

    // The first transcript row is the visible anchor just below the slot.
    const anchor = page.locator(`${ROWS} > *`).first();
    const beforeTop = await anchor.evaluate(element => element.getBoundingClientRect().top);
    await page.getByRole('button', { name: 'Load earlier messages' }).click();
    await expect(page.getByLabel('Earlier messages')).toContainText('history 5');
    const afterTop = await anchor.evaluate(element => element.getBoundingClientRect().top);
    expect(Math.abs(afterTop - beforeTop)).toBeLessThanOrEqual(4);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('a reply taller than the viewport pauses auto-follow and offers the jump pill', async ({ browser }) => {
  const { context, page, guards } = await guardedPage(browser, { viewport: PHONE, allowSignedOut401: true });
  try {
    await openConversation(page);
    // One large streamed chunk makes the active reply much taller than the viewport.
    streamMore(80);
    await expect(page.locator(ROWS)).toContainText('stream chunk 84');
    await expect(page.locator(PILL)).toBeVisible();
    await expect(page.locator(PILL)).toContainText('Streaming');

    const pausedTop = await page.evaluate(selector => document.querySelector(selector).scrollTop, SCROLLER);
    // More streaming: the reader stays where the reply started being readable.
    streamMore(20);
    await expect(page.locator(ROWS)).toContainText('stream chunk 104');
    const laterTop = await page.evaluate(selector => document.querySelector(selector).scrollTop, SCROLLER);
    expect(laterTop).toBe(pausedTop);

    // The pill re-attaches and jumps to the live end.
    await page.locator(PILL).click();
    await expect(page.locator(PILL)).toBeHidden();
    await expect.poll(() => page.evaluate(() => {
      const el = document.querySelector('[data-testid="conversation-scroll"]');
      return el.scrollHeight - el.scrollTop - el.clientHeight;
    })).toBeLessThan(4);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});
