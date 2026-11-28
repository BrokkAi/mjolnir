// Shared flows for the deterministic specs: common API routes, signing in,
// and opening a conversation against the mock dist server.

const { test } = require('@playwright/test');
const { createDistServer, attachStrictGuards, installCspCollector } = require('./dist-server');

const PHONE = { width: 390, height: 844 };
const DESKTOP = { width: 1280, height: 800 };

/**
 * The routes nearly every spec needs. `transcripts` maps a session id to a
 * BrowserTranscript (or a function producing one from the query). Every call
 * to `GET /api/v1/conversations/{id}` is recorded in `server.log`, so the
 * cellular specs count requests there.
 */
function installCommonRoutes(server, { transcripts = {}, extra } = {}) {
  server.handle('GET', /^\/api\/v1\/conversations\/[^/]+$/, ({ entry, url }) => {
    const id = decodeURIComponent(url.pathname.split('/').pop());
    const source = transcripts[id];
    const value = typeof source === 'function' ? source(entry.query) : source;
    return {
      json: value || {
        latest_seq: 0,
        presentation_key: 'pk-empty',
        window_start_seq: 1,
        reset: true,
        entries: [],
      },
    };
  });
  server.handle('POST', /^\/api\/v1\/conversations\/[^/]+\/read$/, () => ({}));
  server.handle('GET', /^\/api\/v1\/sessions\/[^/]+\/history$/, () => ({
    json: { items: [], before: null, frontier: 0 },
  }));
  server.handle('GET', /^\/api\/v1\/sessions\/[^/]+\/client-state$/, () => ({
    json: { draft: '', through_event_ordinal: 0 },
  }));
  server.handle('PUT', /^\/api\/v1\/sessions\/[^/]+\/draft$/, () => ({}));
  server.handle('POST', '/api/v1/actions', () => ({}));
  server.handle('GET', '/api/v1/profiles/providers', () => ({ json: { providers: {} } }));
  server.handle('POST', '/api/v1/profiles/select', () => ({ json: 'p1' }));
  server.handle('GET', /^\/api\/v1\/sessions\/[^/]+\/notification-message$/, ({ url }) => ({
    json: {
      session_id: decodeURIComponent(url.pathname.split('/')[4]),
      text: 'The full notification message.',
    },
  }));
  server.handle('GET', '/api/v1/push/key', () => ({ json: { public_key: 'BE2e' } }));
  server.handle('GET', '/api/v1/push/status', () => ({ json: { subscribed: false, error: null } }));
  if (extra) extra(server);
}

/** One mock server per test file worker; close it with the returned function. */
async function startServer(options = {}) {
  const server = await createDistServer(options);
  return server;
}

/**
 * Fresh page with strict guards and the CSP violation collector installed.
 * `guards.assertClean()` must run before the test ends.
 */
async function guardedPage(
  browser,
  { viewport = PHONE, allowSignedOut401 = false, allowLoginRefusals = false } = {},
) {
  const context = await browser.newContext({ viewport, ignoreHTTPSErrors: true });
  const page = await context.newPage();
  const guards = attachStrictGuards(page, { allowSignedOut401, allowLoginRefusals });
  await installCspCollector(page);
  return { context, page, guards };
}

/** Sign in through the real login form so the cookie flow is exercised. */
async function signIn(page, server, code = '123456') {
  await page.goto(`${server.url}/`);
  await page.getByLabel('Code', { exact: true }).fill(code);
  await page.getByRole('button', { name: 'Enter', exact: true }).click();
}

/** Assert no element spills past the viewport's right (or left) edge. */
async function expectNoHorizontalOverflow(page) {
  const overflow = await page.evaluate(() => {
    const tolerance = 1;
    const doc = document.documentElement;
    if (doc.scrollWidth - doc.clientWidth > tolerance) {
      return `document scrollWidth ${doc.scrollWidth} exceeds clientWidth ${doc.clientWidth}`;
    }
    const wide = [];
    for (const element of document.querySelectorAll('body *')) {
      // A closed drawer is hidden from users and assistive tech, not overflow.
      if (element.closest('[aria-hidden="true"], [inert]')) continue;
      const rect = element.getBoundingClientRect();
      if (rect.width === 0 || rect.height === 0) continue;
      if (rect.right > doc.clientWidth + tolerance || rect.left < -tolerance) {
        const tag = element.tagName.toLowerCase();
        const id = element.id ? `#${element.id}` : '';
        const cls = typeof element.className === 'string' && element.className.trim()
          ? `.${element.className.trim().split(/\s+/).slice(0, 3).join('.')}`
          : '';
        wide.push(`${tag}${id}${cls} right=${Math.round(rect.right)} left=${Math.round(rect.left)}`);
        if (wide.length >= 5) break;
      }
    }
    return wide.length ? `overflowing elements: ${wide.join(' | ')}` : null;
  });
  if (overflow) throw new Error(overflow);
}

/** The actions posted to /api/v1/actions, as parsed JSON bodies. */
function postedActions(server) {
  return server.requests('POST', '/api/v1/actions').map(entry => entry.body);
}

module.exports = {
  test,
  PHONE,
  DESKTOP,
  startServer,
  installCommonRoutes,
  guardedPage,
  signIn,
  expectNoHorizontalOverflow,
  postedActions,
};
