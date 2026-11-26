// Cellular request budget: the signed-in boot wastes no round trips, hashed
// assets carry immutable caching, unrelated SSE deltas cost zero fetches, a
// relevant delta costs exactly one incremental fetch, and returning to an
// unchanged conversation renders from cache.

const { test, expect } = require('@playwright/test');
const { PHONE, startServer, installCommonRoutes, guardedPage } = require('./spec-helpers');
const { snapshot, liveSession, transcriptEntry, transcript } = require('./fixtures');
const { CACHE_IMMUTABLE, attachStrictGuards, installCspCollector } = require('./dist-server');

let server;

function conversationS1(query) {
  const after = Number(query.after_seq || 0);
  if (after > 0) {
    return {
      latest_seq: 11,
      presentation_key: 'pk-1',
      window_start_seq: 1,
      reset: false,
      entries: [
        transcriptEntry(3, {
          role: 'agent',
          stable_id: 'row-3',
          updated_seq: 11,
          lines: ['incremental chunk arrived'],
        }),
      ],
    };
  }
  return transcript({
    latestSeq: 10,
    presentationKey: 'pk-1',
    entries: [
      transcriptEntry(1, { role: 'user', label: 'You', stable_id: 'row-1', lines: ['first question'] }),
      transcriptEntry(2, { role: 'agent', stable_id: 'row-2', updated_seq: 10, lines: ['first answer'] }),
    ],
  });
}

test.beforeAll(async () => {
  server = await startServer();
  installCommonRoutes(server, {
    transcripts: { s1: conversationS1 },
  });
});

test.afterAll(async () => {
  await server.close();
});

test.beforeEach(() => {
  server.log.length = 0;
  server.setSnapshot(
    snapshot({
      sessions: [
        liveSession('s1', { title: 'Cellular one' }),
        liveSession('s2', { title: 'Cellular two' }),
      ],
    }),
  );
});

/** A signed-in page: the cookie is planted before the first navigation. */
async function signedInPage(browser, hash = '') {
  const context = await browser.newContext({ viewport: PHONE, ignoreHTTPSErrors: true });
  const { name, value } = server.grantSession();
  await context.addCookies([{ name, value, url: server.url }]);
  const page = await context.newPage();
  const guards = attachStrictGuards(page);
  await installCspCollector(page);
  await page.goto(`${server.url}/${hash}`);
  return { context, page, guards };
}

test('signed-in boot fetches the snapshot only, with immutable asset caching', async ({ browser }) => {
  const { context, page, guards } = await signedInPage(browser);
  try {
    await expect(page.getByRole('heading', { name: 'Workspaces' })).toBeVisible();

    // No /auth/session probe and no /api/v1/options on boot.
    expect(server.requests('GET', '/auth/session')).toHaveLength(0);
    expect(server.requests('GET', '/api/v1/options')).toHaveLength(0);
    expect(server.requests('GET', '/api/v1/snapshot')).toHaveLength(1);

    // Hashed assets are immutable; the shell and service worker revalidate.
    const assetEntries = server.requests('GET', /^\/assets\/.+\.(js|css)$/);
    expect(assetEntries.length).toBeGreaterThan(0);
    for (const entry of assetEntries) {
      expect(entry.response.headers['cache-control']).toBe(CACHE_IMMUTABLE);
    }
    for (const entry of server.requests('GET', '/')) {
      expect(entry.response.headers['cache-control']).toBe('no-cache');
    }
    for (const entry of server.requests('GET', '/service-worker.js')) {
      expect(entry.response.headers['cache-control']).toBe('no-store');
    }
    for (const entry of server.log.filter(item => item.path.startsWith('/api/'))) {
      expect(entry.response.headers['cache-control']).toBe('no-store');
    }
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('SSE deltas fetch exactly what changed, and revisits render from cache', async ({ browser }) => {
  const { context, page, guards } = await signedInPage(browser, '#conversation/s1');
  try {
    // Booting straight into a conversation fetches it once, alongside the snapshot.
    await expect(page.getByTestId('transcript-rows')).toContainText('first answer');
    const conversationFetches = () =>
      server.requests('GET', '/api/v1/conversations/s1').filter(entry => entry.response.status === 200);
    expect(conversationFetches()).toHaveLength(1);
    expect(server.requests('GET', '/api/v1/snapshot')).toHaveLength(1);

    // A delta about another session costs zero conversation fetches.
    server.publishDelta({ conversationSeqs: { s2: 5 } });
    await page.waitForTimeout(400);
    expect(conversationFetches()).toHaveLength(1);

    // A delta advancing this conversation costs exactly one incremental fetch.
    server.publishDelta({ conversationSeqs: { s1: 11 } });
    await expect(page.getByTestId('transcript-rows')).toContainText('incremental chunk arrived');
    expect(conversationFetches()).toHaveLength(2);
    const incremental = conversationFetches()[1];
    expect(incremental.query.after_seq).toBe('10');
    expect(incremental.query.presentation_key).toBe('pk-1');

    // Away and back: the cached transcript renders with zero new fetches.
    await page.getByRole('button', { name: 'Back' }).click();
    await expect(page.getByRole('heading', { name: 'Workspaces' })).toBeVisible();
    await page.getByTestId('session-card-s1').getByRole('button', { name: 'Cellular one', exact: true }).click();
    await expect(page.getByTestId('transcript-rows')).toContainText('incremental chunk arrived');
    expect(conversationFetches()).toHaveLength(2);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});
