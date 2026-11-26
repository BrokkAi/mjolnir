// Deterministic mock for the Mjolnir viewer server.
//
// Serves the real compiled SPA from mj-server/src/web/dist with the exact
// Content-Security-Policy and Cache-Control headers mj-server sends, so every
// deterministic Playwright spec exercises the shipped bundle under the
// shipped security policy. Auth, REST and the SSE runtime stream are
// controllable in-process, and every request is logged so specs can assert
// exact cellular request counts.
//
// Pure Node built-ins only: the CI web-viewer job installs nothing but
// @playwright/test.

const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');

const { viewerWireSnapshot } = require('./lab-env');

const DIST_ROOT = path.resolve(__dirname, '../../../mj-server/src/web/dist');

// Exactly `CONTENT_SECURITY_POLICY` in mj-server/src/server/assets.rs.
const CONTENT_SECURITY_POLICY =
  "default-src 'none'; script-src 'self'; style-src 'self'; " +
  "img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; " +
  "manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const CACHE_IMMUTABLE = 'public, max-age=31536000, immutable';
const CACHE_NO_STORE = 'no-store';
const CACHE_NO_CACHE = 'no-cache';

const MIME_TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.woff2': 'font/woff2',
  '.webmanifest': 'application/manifest+json',
  '.json': 'application/json',
};

const COOKIE_NAME = 'mj-viewer-e2e';

function parseCookies(header) {
  const cookies = {};
  for (const part of String(header || '').split(';')) {
    const index = part.indexOf('=');
    if (index < 0) continue;
    cookies[part.slice(0, index).trim()] = part.slice(index + 1).trim();
  }
  return cookies;
}

function readBody(request) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    request.on('data', chunk => chunks.push(chunk));
    request.on('end', () => resolve(Buffer.concat(chunks)));
    request.on('error', reject);
  });
}

/**
 * A running mock server. Fields:
 * - `url`: base URL (`http://127.0.0.1:PORT`).
 * - `log`: every request, `{ method, path, query, body, response }`.
 * - `snapshot`: the wire snapshot `GET /api/v1/snapshot` serves.
 * - `cursor`: the runtime cursor the stream resumes from.
 * - `onLogin(body)`: override the POST /auth/session verdict, returning
 *   `{ status, json?, headers? }`.
 */
async function createDistServer(options = {}) {
  const code = options.code === undefined ? '123456' : options.code;
  const tokens = new Set();
  const sseClients = new Set();
  const routes = [];

  const server = {
    url: '',
    log: [],
    snapshot: options.snapshot || null,
    cursor: (options.snapshot && options.snapshot.cursor) || { incarnation: 'e2e', sequence: 1 },
    onLogin: null,

    /** Register an API route. `pattern` is an exact path string or a RegExp. */
    handle(method, pattern, handler) {
      routes.push({ method, pattern, handler });
    },

    /** Requests in the log, optionally filtered by method and path predicate. */
    requests(method, predicate) {
      return this.log.filter(entry => {
        if (method && entry.method !== method) return false;
        if (!predicate) return true;
        if (typeof predicate === 'function') return predicate(entry);
        if (predicate instanceof RegExp) return predicate.test(entry.path);
        return entry.path === predicate;
      });
    },

    /**
     * Issue a session cookie the mock accepts, for tests that start signed in
     * without exercising the login form. Returns `{ name, value }`.
     */
    grantSession() {
      const token = `test-granted-${tokens.size + 1}-${Date.now().toString(36)}`;
      tokens.add(token);
      return { name: COOKIE_NAME, value: token };
    },

    /** Install a fresh wire snapshot (as `viewerWireSnapshot` produces it). */
    setSnapshot(wireSnapshot) {
      this.snapshot = wireSnapshot;
      this.cursor = wireSnapshot.cursor;
    },

    /**
     * Broadcast a delta frame. `sessions` maps a session id to a fixture row
     * (converted to wire form, interned fields included) or `null` to remove
     * it. The cursor advances by one and the served snapshot follows.
     */
    publishDelta({ sessions = {}, metadata = {}, conversationVersions, detailChanged = [] } = {}) {
      const from = this.cursor;
      const cursor = { ...from, sequence: from.sequence + 1 };
      const changedRows = Object.values(sessions).filter(Boolean);
      const wired = changedRows.length
        ? viewerWireSnapshot({ sessions: changedRows, revision: cursor.sequence, cursor })
        : { sessions: [], interned: {} };
      const wireById = new Map(wired.sessions.map(row => [row.id, row]));
      const frameSessions = [];
      for (const [id, row] of Object.entries(sessions)) {
        frameSessions.push([id, row ? wireById.get(id) : null]);
      }
      // Keep the REST snapshot consistent with what the stream publishes.
      if (this.snapshot) {
        this.snapshot.cursor = cursor;
        this.snapshot.revision = cursor.sequence;
        Object.assign(this.snapshot.interned, wired.interned);
        const current = new Map(this.snapshot.sessions.map(row => [row.id, row]));
        for (const [id, row] of frameSessions) {
          if (row) current.set(id, row);
          else current.delete(id);
        }
        this.snapshot.sessions = [...current.values()];
      }
      this.cursor = cursor;
      const frame = {
        kind: 'delta',
        from,
        cursor,
        metadata: {
          revision: cursor.sequence,
          server_time_ms: Date.now(),
          generated_at: new Date().toISOString(),
          ...metadata,
        },
        sessions: frameSessions,
        detail_changed: detailChanged,
        interned: wired.interned,
        ...(conversationVersions ? { conversation_versions: conversationVersions } : {}),
      };
      broadcast(frame);
      return frame;
    },

    /** Broadcast a raw stream frame (for cases publishDelta cannot express). */
    broadcast,

    async close() {
      for (const response of sseClients) response.end();
      sseClients.clear();
      await new Promise(resolve => listener.close(resolve));
    },
  };

  function broadcast(frame) {
    const payload = `event: runtime\ndata: ${JSON.stringify(frame)}\n\n`;
    for (const response of sseClients) {
      try {
        response.write(payload);
      } catch {
        sseClients.delete(response);
      }
    }
  }

  function findRoute(method, pathName) {
    // Last registered wins, so a spec can override a shared default route.
    return routes.findLast(route => {
      if (route.method !== method) return false;
      if (typeof route.pattern === 'string') return route.pattern === pathName;
      return route.pattern.test(pathName);
    });
  }

  function sendJson(response, entry, status, value, headers = {}) {
    const body = JSON.stringify(value);
    const full = { 'content-type': 'application/json', 'cache-control': CACHE_NO_STORE, ...headers };
    entry.response = { status, headers: full };
    response.writeHead(status, full);
    response.end(body);
  }

  function sendEmpty(response, entry, status, headers = {}) {
    const full = { 'cache-control': CACHE_NO_STORE, ...headers };
    entry.response = { status, headers: full };
    response.writeHead(status, full);
    response.end();
  }

  function serveStatic(response, entry, pathName) {
    let relative;
    let cacheControl;
    if (pathName === '/' || pathName === '/login') {
      relative = 'index.html';
      cacheControl = CACHE_NO_CACHE;
    } else if (pathName.startsWith('/assets/')) {
      relative = pathName.slice(1);
      cacheControl = CACHE_IMMUTABLE;
    } else if (pathName === '/service-worker.js') {
      relative = 'service-worker.js';
      cacheControl = CACHE_NO_STORE;
    } else {
      relative = pathName.slice(1);
      cacheControl = CACHE_NO_CACHE;
    }
    const file = path.join(DIST_ROOT, relative);
    if (!file.startsWith(DIST_ROOT + path.sep) && file !== path.join(DIST_ROOT, 'index.html')) {
      entry.response = { status: 403, headers: {} };
      response.writeHead(403);
      response.end();
      return;
    }
    let body;
    try {
      body = fs.readFileSync(file);
    } catch {
      entry.response = { status: 404, headers: {} };
      response.writeHead(404, { 'cache-control': CACHE_NO_STORE });
      response.end('not found');
      return;
    }
    const type = MIME_TYPES[path.extname(file)] || 'application/octet-stream';
    entry.response = { status: 200, headers: { 'content-type': type, 'cache-control': cacheControl } };
    response.writeHead(200, { 'content-type': type, 'cache-control': cacheControl });
    response.end(body);
  }

  const listener = http.createServer(async (request, response) => {
    const url = new URL(request.url, 'http://127.0.0.1');
    const pathName = url.pathname;
    const entry = {
      method: request.method,
      path: pathName,
      query: Object.fromEntries(url.searchParams),
      body: undefined,
      response: undefined,
    };
    server.log.push(entry);

    // Every response carries the shipped security policy.
    response.setHeader('content-security-policy', CONTENT_SECURITY_POLICY);
    response.setHeader('x-content-type-options', 'nosniff');
    response.setHeader('referrer-policy', 'no-referrer');

    try {
      if (pathName.startsWith('/api/') || pathName.startsWith('/auth/')) {
        const raw = await readBody(request);
        if (raw.length && (request.headers['content-type'] || '').includes('application/json')) {
          try {
            entry.body = JSON.parse(raw.toString('utf8'));
          } catch {
            entry.body = raw.toString('utf8');
          }
        } else if (raw.length) {
          entry.body = raw;
        }
        await handleApi(request, response, entry, url);
        return;
      }
      if (request.method !== 'GET' && request.method !== 'HEAD') {
        entry.response = { status: 405, headers: {} };
        response.writeHead(405);
        response.end();
        return;
      }
      serveStatic(response, entry, pathName);
    } catch (error) {
      entry.response = { status: 500, headers: {} };
      if (!response.headersSent) response.writeHead(500);
      response.end(String(error && error.stack ? error.stack : error));
    }
  });

  function authenticated(request) {
    if (options.auth === false) return true;
    const cookies = parseCookies(request.headers.cookie);
    return Boolean(cookies[COOKIE_NAME] && tokens.has(cookies[COOKIE_NAME]));
  }

  async function handleApi(request, response, entry, url) {
    const pathName = url.pathname;

    // Auth routes are answered before the session check: login creates it.
    if (pathName === '/auth/session') {
      if (request.method === 'POST') {
        const verdict = server.onLogin
          ? server.onLogin(entry.body || {})
          : entry.body && entry.body.code === code
            ? { status: 204, session: true }
            : { status: 401, json: { error: 'That code is not correct.' } };
        const headers = { ...(verdict.headers || {}) };
        if (verdict.session) {
          const token = `test-${tokens.size + 1}-${Date.now().toString(36)}`;
          tokens.add(token);
          headers['set-cookie'] = `${COOKIE_NAME}=${token}; HttpOnly; SameSite=Strict; Path=/`;
        }
        if (verdict.json !== undefined) {
          sendJson(response, entry, verdict.status, verdict.json, headers);
        } else {
          sendEmpty(response, entry, verdict.status, headers);
        }
        return;
      }
      if (!authenticated(request)) {
        sendJson(response, entry, 401, { error: 'unauthorized' });
        return;
      }
      if (request.method === 'GET') {
        sendJson(response, entry, 200, { signed_in: true });
        return;
      }
      if (request.method === 'DELETE') {
        const cookies = parseCookies(request.headers.cookie);
        tokens.delete(cookies[COOKIE_NAME]);
        sendEmpty(response, entry, 204, {
          'set-cookie': `${COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0`,
        });
        return;
      }
    }
    if (pathName === '/auth/login') {
      // The QR/token route sets a cookie and lands on the app.
      if (url.searchParams.get('token') === (options.loginToken || 'e2e-login-token')) {
        const token = `test-qr-${tokens.size + 1}`;
        tokens.add(token);
        const headers = {
          location: '/',
          'set-cookie': `${COOKIE_NAME}=${token}; HttpOnly; SameSite=Strict; Path=/`,
          'cache-control': CACHE_NO_STORE,
        };
        entry.response = { status: 303, headers };
        response.writeHead(303, headers);
        response.end();
        return;
      }
      sendJson(response, entry, 401, { error: 'unauthorized' });
      return;
    }

    if (!authenticated(request)) {
      sendJson(response, entry, 401, { error: 'unauthorized' });
      return;
    }

    if (pathName === '/api/v1/viewer/events' && request.method === 'GET') {
      const headers = {
        'content-type': 'text/event-stream',
        'cache-control': CACHE_NO_STORE,
        connection: 'keep-alive',
      };
      entry.response = { status: 200, headers };
      response.writeHead(200, headers);
      response.write(': connected\n\n');
      sseClients.add(response);
      request.on('close', () => sseClients.delete(response));
      return;
    }

    if (pathName === '/api/v1/snapshot' && request.method === 'GET') {
      if (!server.snapshot) {
        sendJson(response, entry, 500, { error: 'no snapshot configured' });
        return;
      }
      sendJson(response, entry, 200, server.snapshot);
      return;
    }

    const route = findRoute(request.method, pathName);
    if (route) {
      const result = (await route.handler({ entry, request, url, server })) || {};
      if (result.raw) {
        const headers = { 'cache-control': CACHE_NO_STORE, ...(result.headers || {}) };
        entry.response = { status: result.status || 200, headers };
        response.writeHead(result.status || 200, headers);
        response.end(result.raw);
        return;
      }
      if (result.json !== undefined) {
        sendJson(response, entry, result.status || 200, result.json, result.headers);
        return;
      }
      sendEmpty(response, entry, result.status || 202, result.headers);
      return;
    }

    sendJson(response, entry, 404, { error: `not mocked: ${request.method} ${pathName}` });
  }

  await new Promise(resolve => listener.listen(0, '127.0.0.1', resolve));
  server.url = `http://127.0.0.1:${listener.address().port}`;
  return server;
}

/**
 * Fail the test on any pageerror, CSP violation, or unexpected console.error.
 *
 * The one tolerated message is Chromium's resource log for the expected 401
 * the signed-out boot probe (`GET /api/v1/snapshot`) produces, and only when
 * `allowSignedOut401` is set, and the login form's 401/429 refusals when
 * `allowLoginRefusals` is set. Attach before the first `page.goto`.
 */
function attachStrictGuards(page, { allowSignedOut401 = false, allowLoginRefusals = false } = {}) {
  const failures = [];
  page.on('pageerror', error => {
    failures.push(`pageerror: ${error && error.message ? error.message : error}`);
  });
  page.on('console', message => {
    if (message.type() !== 'error') return;
    const text = message.text();
    if (/content security policy|violates the following/i.test(text)) {
      failures.push(`csp: ${text}`);
      return;
    }
    const expected401 =
      allowSignedOut401 &&
      text.includes('Failed to load resource') &&
      /\b401\b/.test(text) &&
      (message.location()?.url || '').includes('/api/v1/');
    if (expected401) return;
    // The login form's own refusals: wrong code (401) and lockout (429).
    const expectedLoginRefusal =
      allowLoginRefusals &&
      text.includes('Failed to load resource') &&
      /\b(401|429)\b/.test(text) &&
      (message.location()?.url || '').includes('/auth/session');
    if (expectedLoginRefusal) return;
    failures.push(`console.error: ${text}`);
  });
  return {
    failures,
    async assertClean() {
      const pageViolations = await page
        .evaluate(() => (Array.isArray(window.__mjCspViolations) ? window.__mjCspViolations : []))
        .catch(() => []);
      const all = [...failures, ...pageViolations.map(detail => `csp: ${detail}`)];
      if (all.length) {
        throw new Error(`strict console/CSP guards failed:\n${all.join('\n')}`);
      }
    },
  };
}

/** Install the CSP violation collector; call before the first navigation. */
async function installCspCollector(page) {
  await page.addInitScript(() => {
    window.__mjCspViolations = [];
    document.addEventListener('securitypolicyviolation', event => {
      window.__mjCspViolations.push(`${event.violatedDirective}: ${event.blockedURI}`);
    });
  });
}

module.exports = {
  CONTENT_SECURITY_POLICY,
  CACHE_IMMUTABLE,
  CACHE_NO_STORE,
  CACHE_NO_CACHE,
  DIST_ROOT,
  createDistServer,
  attachStrictGuards,
  installCspCollector,
};
