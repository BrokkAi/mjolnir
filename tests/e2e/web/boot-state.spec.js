const path = require('node:path');
const { test, expect } = require('@playwright/test');
const { viewerWireSnapshot } = require('./lab-env');

test.use({ serviceWorkers: 'block' });

const webRoot = path.resolve(__dirname, '../../../mj-controller/src/web');
const snapshot = {
  revision: 1,
  generated_at: '2026-10-08T00:00:00Z',
  server_time_ms: Date.now(),
  workspaces: [{ id: 'test', name: 'Boot test' }],
  sessions: [],
  profiles: [],
  targets: [],
  bundles: [],
  capacity: [],
  launch_failures: [],
  review_config: { enabled: false, profile: null },
};

async function mount(page, { signedIn = true, snapshotResponse }) {
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
      close() { this.closed = true; }
    };
  });
  await page.route('**/*', async route => {
    const pathname = new URL(route.request().url()).pathname;
    const json = value => route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(value),
    });
    if (pathname === '/auth/session') return json({ signed_in: signedIn });
    if (pathname === '/api/snapshot') return snapshotResponse(route);
    if (pathname === '/api/v1/options') return json({ default: null });
    if (pathname === '/api/events') {
      return route.fulfill({ contentType: 'text/event-stream', body: ': fixture\n\n' });
    }
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    const assetPath = file === 'icon.svg'
      ? path.join(webRoot, '../icons/icon.svg')
      : file === 'fonts/jetbrains-mono.woff2'
        ? path.join(webRoot, '../fonts/jetbrains-mono.woff2')
        : path.join(webRoot, file);
    if (pathname === '/' || [
      '/viewer.css', '/viewer.js', '/markdown.js', '/tool-output.js',
      '/manifest.webmanifest', '/service-worker.js', '/icon.svg',
      '/icon-192.png', '/icon-512.png', '/maskable-512.png',
      '/apple-touch-icon.png', '/fonts/jetbrains-mono.woff2',
    ].includes(pathname)) {
      return route.fulfill({ path: assetPath });
    }
    return route.fulfill({ status: 404, body: '' });
  });
}

test('signed-in boot keeps the unlock form hidden during a slow snapshot', async ({ page }) => {
  let releaseSnapshot;
  let started;
  const snapshotStarted = new Promise(resolve => { started = resolve; });
  const heldSnapshot = new Promise(resolve => { releaseSnapshot = resolve; });
  await mount(page, {
    snapshotResponse: async route => {
      started();
      await heldSnapshot;
      return route.fulfill({
        contentType: 'application/json',
        body: JSON.stringify(viewerWireSnapshot(snapshot)),
      });
    },
  });

  await page.goto('https://viewer.test/');
  await snapshotStarted;
  await expect(page.locator('#boot-status')).toBeVisible();
  await expect(page.locator('#boot-status')).toContainText(/connecting|loading/i);
  await expect(page.locator('#login')).toBeHidden();

  releaseSnapshot();
  await expect(page.locator('#app')).toBeVisible();
  await expect(page.locator('#boot-status')).toBeHidden();
});

test('signed-out boot shows the unlock form without requesting a snapshot', async ({ page }) => {
  let snapshots = 0;
  await mount(page, {
    signedIn: false,
    snapshotResponse: async route => {
      snapshots += 1;
      return route.fulfill({
        contentType: 'application/json',
        body: JSON.stringify(viewerWireSnapshot(snapshot)),
      });
    },
  });

  await page.goto('https://viewer.test/');
  await expect(page.locator('#login')).toBeVisible();
  await expect(page.locator('#boot-status')).toBeHidden();
  expect(snapshots).toBe(0);
});

test('snapshot network failure stays on a retry screen and backs off before retrying', async ({ page }) => {
  const requestTimes = [];
  await mount(page, {
    snapshotResponse: async route => {
      requestTimes.push(Date.now());
      if (requestTimes.length === 1) return route.abort();
      return route.fulfill({
        contentType: 'application/json',
        body: JSON.stringify(viewerWireSnapshot(snapshot)),
      });
    },
  });

  await page.goto('https://viewer.test/');
  await expect(page.locator('#boot-status')).toContainText(/can't be reached/i);
  await expect(page.locator('#login')).toBeHidden();
  await page.waitForTimeout(500);
  expect(requestTimes).toHaveLength(1);

  await expect(page.locator('#app')).toBeVisible({ timeout: 5000 });
  expect(requestTimes).toHaveLength(2);
  expect(requestTimes[1] - requestTimes[0]).toBeGreaterThanOrEqual(900);
});

test('change stream retries with the applied cursor and unknown references force a snapshot', async ({ page }) => {
  await mount(page, {
    snapshotResponse: route => route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(viewerWireSnapshot(snapshot)),
    }),
  });
  await page.goto('https://viewer.test/');
  await expect(page.locator('#app')).toBeVisible();
  const streamUrls = () => page.evaluate(() => window.fixtureEventSources.map(source => source.url));
  await expect.poll(streamUrls).toHaveLength(1);
  let urls = await streamUrls();
  expect(new URL(urls[0], 'https://viewer.test').searchParams.get('since')).toBe('fixture:1');

  await page.evaluate(() => {
    window.fixtureEvents.dispatchEvent(new MessageEvent('runtime', {
      data: JSON.stringify({
        kind: 'delta',
        from: { incarnation: 'fixture', sequence: 1 },
        cursor: { incarnation: 'fixture', sequence: 2 },
        metadata: { revision: 2 }, sessions: [], interned: {},
      }),
    }));
  });
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(resolve)));

  await page.evaluate(() => {
    window.fixtureEvents.dispatchEvent(new MessageEvent('runtime', {
      data: JSON.stringify({
        kind: 'delta',
        from: { incarnation: 'fixture', sequence: 1 },
        cursor: { incarnation: 'fixture', sequence: 3 },
        metadata: { revision: 3 }, sessions: [], interned: {},
      }),
    }));
  });
  await expect.poll(streamUrls).toHaveLength(2);
  urls = await streamUrls();
  expect(new URL(urls[1], 'https://viewer.test').searchParams.get('since')).toBe('fixture:2');

  await page.evaluate(() => {
    window.fixtureEvents.dispatchEvent(new MessageEvent('runtime', {
      data: JSON.stringify({
        kind: 'delta',
        from: { incarnation: 'fixture', sequence: 2 },
        cursor: { incarnation: 'fixture', sequence: 3 },
        metadata: { revision: 3 },
        sessions: [['missing', { id: 'missing', workspace_id: 'test', capabilities_ref: 'unknown' }]],
        interned: {},
      }),
    }));
  });
  await expect.poll(streamUrls).toHaveLength(3);
  urls = await streamUrls();
  expect(new URL(urls[2], 'https://viewer.test').searchParams.has('since')).toBe(false);
});
