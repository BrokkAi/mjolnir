const { test, expect } = require('@playwright/test');
const fs = require('node:fs');
const path = require('node:path');
const { isDeepStrictEqual } = require('node:util');
const { viewerWireSnapshot } = require('./lab-env');

const WEB_ROOT = path.resolve(__dirname, '../../../mj-controller/src/web');
// The daemon's answers to these exact requests, recorded by the Rust test
// `settings_routes_hide_literal_secrets_and_save_through_the_shared_model`.
// Replaying them ties the page to what the daemon really sends, and a
// request the page makes that the daemon side never recorded fails here.
const EXCHANGE = JSON.parse(fs.readFileSync(
  path.resolve(__dirname, '../../../mj-controller/tests/golden/viewer-settings-exchange.txt'),
  'utf8',
));
const WORKSPACE_ID = 'workspace-settings';
const ASSETS = new Set(['viewer.html', 'push-sync.js', 'viewer.js', 'viewer.css', 'markdown.js', 'tool-output.js', 'manifest.webmanifest']);

test.use({ serviceWorkers: 'block' });

function snapshot() {
  return viewerWireSnapshot({
    revision: 1,
    generated_at: '2030-06-15T15:00:00Z',
    server_time_ms: Date.parse('2030-06-15T15:00:00Z'),
    workspaces: [{ id: WORKSPACE_ID, name: 'Settings' }],
    sessions: [],
    profiles: [],
    targets: [],
    bundles: [],
    review_config: { trigger: 'off', profile: null },
  });
}

/// A daemon that answers only with the recorded exchange. Drafts are opaque
/// tokens here: the page must send back exactly the one it last received.
async function mount(page) {
  const state = { requests: [], draft: null, original: null, drafts: 0, failures: [] };
  await page.addInitScript(() => {
    window.EventSource = class extends EventTarget {
      constructor(url) {
        super();
        this.url = url;
        queueMicrotask(() => this.dispatchEvent(new Event('open')));
      }

      close() {}
    };
  });
  const issueDraft = () => {
    state.drafts += 1;
    state.draft = { opaque: `draft-${state.drafts}` };
    return state.draft;
  };
  const answer = (route, recorded, extra = {}) => route.fulfill({
    status: recorded.status,
    contentType: 'application/json',
    body: JSON.stringify({ ...recorded.response, ...extra }),
  });
  await page.route('**/*', async route => {
    const request = route.request();
    const pathname = new URL(request.url()).pathname;
    if (pathname === '/api/snapshot') {
      return route.fulfill({ contentType: 'application/json', body: JSON.stringify(snapshot()) });
    }
    if (pathname === '/api/events') {
      return route.fulfill({ status: 200, headers: { 'content-type': 'text/event-stream' }, body: ': fixture\n\n' });
    }
    if (pathname.startsWith('/api/settings')) {
      const body = request.method() === 'POST' ? request.postDataJSON() : null;
      const { draft, original, ...sent } = body || {};
      state.requests.push({ uri: pathname, request: body ? sent : null });
      if (body && !isDeepStrictEqual(draft, state.draft)) {
        state.failures.push(`${pathname} sent ${JSON.stringify(draft)}, not the draft it last received`);
      }
      if (original !== undefined && !isDeepStrictEqual(original, state.original)) {
        state.failures.push(`${pathname} sent ${JSON.stringify(original)}, not the original it opened`);
      }
      // Answers are replayed strictly in the recorded order.
      const recorded = EXCHANGE[state.requests.length - 1];
      if (!recorded || recorded.uri !== pathname || !isDeepStrictEqual(recorded.request, body ? sent : null)) {
        state.failures.push(`unrecorded request ${pathname} ${JSON.stringify(sent)}`);
        return route.fulfill({ status: 500, contentType: 'application/json', body: '{"error":"unrecorded request"}' });
      }
      if ('original_environments' in recorded.response) {
        state.original = { opaque: `original-${state.drafts + 1}` };
        return answer(route, recorded, { original: state.original, draft: issueDraft() });
      }
      if ('draft_environments' in recorded.response) return answer(route, recorded, { draft: issueDraft() });
      return answer(route, recorded);
    }
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    if (ASSETS.has(file)) return route.fulfill({ path: path.join(WEB_ROOT, file) });
    return route.fulfill({ status: 404, body: '' });
  });
  return state;
}

async function settle(page) {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
}

test('golden_viewer_settings', async ({ page }) => {
  const { assertGolden } = await import('./golden.mjs');
  await page.setViewportSize({ width: 390, height: 844 });
  const state = await mount(page);
  const output = [];
  const capture = async label => {
    await settle(page);
    output.push(`=== ${label} (390x844) ===`);
    output.push((await page.locator('#settings-page').innerText()).trim());
    // What each control holds, which the page text above cannot show.
    const controls = await page.locator('#settings input, #settings select').evaluateAll(nodes => nodes.map(node => {
      const label = node.closest('label')?.querySelector('.settings-label')?.textContent;
      if (node.type === 'checkbox') return `${label}: ${node.checked ? 'checked' : 'unchecked'}`;
      if (node.tagName === 'SELECT') return `${label}: ${node.selectedOptions[0]?.textContent}`;
      return `${label}: ${JSON.stringify(node.value)} (${node.type}, placeholder ${JSON.stringify(node.placeholder)})`;
    }));
    output.push(...controls.map(control => `control ${control}`));
    const layout = await page.evaluate(() => ({
      overflowsSideways: document.documentElement.scrollWidth > innerWidth,
      savebar: !document.querySelector('#settings-savebar').classList.contains('hidden'),
      saveEnabled: !document.querySelector('#settings-save').disabled,
      menu: document.querySelector('#menu-settings').textContent,
      hash: location.hash,
    }));
    output.push(`layout: ${JSON.stringify(layout)}`);
  };

  await page.goto(`https://viewer.test/#workspace/${WORKSPACE_ID}`);
  await page.locator('#menu-button').click();
  await page.locator('#menu-settings').click();
  await expect(page.locator('#settings')).toContainText('Agent Profiles');
  await capture('the first page lists every section and what it is set to');

  // A deep link opens a page of the same draft; the secret is never drawn.
  await page.evaluate(() => { location.hash = '#settings/profiles/claude'; });
  await expect(page.locator('#settings-heading')).toHaveText('claude');
  await capture('a profile page names the terminal step for signing in');

  await page.locator('#settings-trail button', { hasText: 'Settings' }).click();
  await page.locator('.settings-link', { hasText: 'Continuation' }).click();
  await expect(page.locator('#settings-heading')).toHaveText('Continuation');
  await page.getByLabel('Enabled').click();
  await expect(page.locator('#settings-savebar')).toBeVisible();
  await capture('an accepted edit leaves the draft unsaved');

  const draftEnabled = await page.getByLabel('Enabled').isChecked();
  // Hard-won: 7a50f9da7: notification navigation reloaded away unsaved Settings.
  await page.evaluate(() => {
    window.notificationDraftMarker = 'same document';
    navigator.serviceWorker.dispatchEvent(new MessageEvent('message', {
      origin: location.origin,
      data: { type: 'mj-notification-open', url: `${location.origin}/` },
    }));
  });
  await expect(page).toHaveURL(new RegExp(`#workspace/${WORKSPACE_ID}$`));
  await page.evaluate(() => { location.hash = '#settings/continuation'; });
  await expect(page.locator('#settings-savebar')).toBeVisible();
  expect(await page.getByLabel('Enabled').isChecked()).toBe(draftEnabled);
  expect(await page.evaluate(() => window.notificationDraftMarker)).toBe('same document');


  await page.locator('#back').click();
  await page.locator('.settings-link', { hasText: 'Sub-agents' }).click();
  const limit = page.getByLabel('Maximum concurrent children');
  await limit.fill('99');
  await limit.press('Enter');
  await expect(page.locator('#settings .error')).toHaveText('Enter a whole number from 1 to 64.');
  await capture('a refused edit keeps what was typed beside the reason');

  const subtleModel = page.getByRole('combobox', { name: 'Subtle model', exact: true });
  const oracleModel = page.getByRole('combobox', { name: 'Oracle model', exact: true });
  const straightforwardModel = page.getByRole('combobox', { name: 'Straightforward model', exact: true });
  const straightforwardEffort = page.getByRole('combobox', { name: 'Straightforward effort', exact: true });
  await expect(page.locator('#settings-heading')).toHaveText('Sub-agents');
  await expect(subtleModel.locator('option:checked')).toHaveText('None');
  await expect(oracleModel.locator('option:checked')).toHaveText('None');
  await expect(straightforwardModel.locator('option:checked')).toHaveText('None');
  await expect(page.getByRole('combobox', { name: 'Subtle effort', exact: true })).toBeHidden();
  await expect(page.getByRole('combobox', { name: 'Oracle effort', exact: true })).toBeHidden();
  await expect(subtleModel.locator('option').first()).toHaveText('None');
  await expect(straightforwardModel.locator('option').first()).toHaveText('None');
  await expect(straightforwardEffort).toBeHidden();
  await capture('inline task models start unset and efforts are hidden');

  await subtleModel.selectOption({ label: 'opus' });
  await expect(subtleModel.locator('option:checked')).toHaveText('opus');
  await capture('Subtle model is edited inline');

  await page.locator('#settings-save').click();
  await expect(page.locator('#settings-error')).toContainText('[subagents.subtle] requires [subagents.straightforward]');
  await expect(page.locator('#settings')).toContainText('Subtle model');
  await expect(page.locator('#settings')).toContainText('opus');
  await capture('a refused save keeps the Subtle model in the draft');

  await straightforwardModel.selectOption({ label: 'opus' });
  await expect(straightforwardEffort).toBeVisible();
  await straightforwardEffort.selectOption({ label: 'high' });

  await page.locator('#settings-save').click();
  await expect(page.locator('#settings-status')).toHaveText('Saved.');
  await capture('saving returns the saved draft');

  expect(state.failures).toEqual([]);
  // Every recorded request was made, in the recorded order.
  expect(state.requests).toEqual(EXCHANGE.map(({ uri, request }) => ({ uri, request })));
  assertGolden('viewer_settings', output.join('\n'));
});

test('device notification enrollment starts from a tap and supports test and unsubscribe', async ({ page }) => {
  await mount(page);
  await page.addInitScript(() => {
    let subscription = null;
    window.pushPermissionTaps = 0;
    window.pushShownNotifications = [];
    window.pushClosedNotifications = [];
    window.Notification = class {
      static permission = 'default';
      static requestPermission() { window.pushPermissionTaps += 1; this.permission = 'granted'; return Promise.resolve('granted'); }
    };
    window.PushManager = class {};
    const registration = {
      active: {},
      getNotifications: async () => window.pushShownNotifications.map(data => ({ data, close: () => window.pushClosedNotifications.push(data.label) })),
      pushManager: {
        getSubscription: async () => subscription,
        subscribe: async () => {
          subscription = {
            endpoint: 'https://push.example/device',
            toJSON: () => ({ endpoint: 'https://push.example/device', keys: { p256dh: 'public', auth: 'secret' } }),
            unsubscribe: async () => { subscription = null; return true; },
          };
          return subscription;
        },
      },
    };
    Object.defineProperty(navigator, 'serviceWorker', { value: Object.assign(new EventTarget(), { register: async () => registration, getRegistration: async () => registration }) });
  });
  const actions = [];
  await page.route('**/api/push/status?*', route => route.fulfill({ json: { subscribed: true, error: null } }));
  await page.route('**/api/push/key', route => route.fulfill({ json: { public_key: 'BA' } }));
  let rejectUnsubscribe = true;
  await page.route('**/api/actions', route => {
    const action = route.request().postDataJSON();
    actions.push(action);
    if (action.action === 'push-unsubscribe' && rejectUnsubscribe) {
      rejectUnsubscribe = false;
      return route.fulfill({ status: 503, json: { error: 'Device removal failed; try again' } });
    }
    return route.fulfill({ json: { accepted: true } });
  });
  await page.goto('https://viewer.test/#settings');
  await expect(page.locator('#device-notification-state')).toContainText('Not enabled');
  expect(await page.evaluate(() => window.pushPermissionTaps)).toBe(0);
  await page.locator('#device-notification-enable').click();
  await expect(page.locator('#device-notification-state')).toHaveText('Enabled on this device');
  expect(await page.evaluate(() => window.pushPermissionTaps)).toBe(1);
  await page.locator('#device-notification-test').click();
  await expect(page.locator('#device-notification-state')).toHaveText('Test notification requested');
  // Hard-won: 82e2bb311: failed server removal lost the native endpoint needed to retry.
  await page.locator('#device-notification-disable').click();
  await expect(page.locator('#device-notification-state')).toContainText('Notification sync failed');
  await page.locator('#device-notification-disable').click();
  await expect(page.locator('#device-notification-state')).toContainText('Not enabled');
  expect(actions.map(action => action.action)).toEqual(['push-subscribe', 'push-test', 'push-unsubscribe', 'push-unsubscribe']);
  const states = { unknown: 'unknown', unreachable: 'unreachable', failed: 'failed', idle: 'idle', working: 'working' };
  await page.route('**/api/snapshot', route => {
    const next = snapshot();
    next.sessions = Object.entries(states).map(([id, attention]) => ({ id, title: id, workspace_id: WORKSPACE_ID, attention, active: true, is_idle: true, capabilities: {}, detail: false }));
    return route.fulfill({ json: viewerWireSnapshot(next) });
  });
  // Hard-won: c146a8a89: focus removed notifications for unavailable questions.
  await page.evaluate(() => {
    window.pushShownNotifications = [
      { label: 'unknown', session_id: 'unknown' }, { label: 'unreachable', session_id: 'unreachable' },
      { label: 'failed', session_id: 'failed' }, { label: 'idle', session_id: 'idle' },
      { label: 'working', session_id: 'working' },
    ];
    window.dispatchEvent(new Event('focus'));
  });
  await expect.poll(() => page.evaluate(() => [...new Set(window.pushClosedNotifications)].sort())).toEqual(['idle', 'working']);

});

// Hard-won: 7a50f9da7: notification clicks reloaded existing viewer drafts.
test('service worker push displays and clicks route an existing window to the conversation', async ({ page }) => {
  await page.goto('about:blank');
  const source = fs.readFileSync(path.join(WEB_ROOT, 'service-worker.js'), 'utf8').replace("importScripts('/push-sync.js');", '');
  const result = await page.evaluate(async source => {
    const listeners = {};
    const calls = [];
    const self = {
      location: { origin: 'https://viewer.test' },
      addEventListener: (name, callback) => { listeners[name] = callback; },
      registration: {
        showNotification: async (title, options) => { calls.push(['show', title, options]); },
        pushManager: { getSubscription: async () => currentSubscription },
      },
      clients: {
        matchAll: async () => [{ url: 'https://viewer.test/', postMessage: message => calls.push(['route', message]), focus: async () => calls.push(['focus']) }],
        openWindow: async url => calls.push(['open', url]),
      },
    };
    let currentSubscription = { toJSON: () => ({ endpoint: 'https://push.example/new', keys: { p256dh: 'key', auth: 'auth' } }) };
    const fetch = async (url, options) => { calls.push(['subscription', url, options.credentials, JSON.parse(options.body)]); return { ok: true }; };
    new Function('self', 'fetch', 'MjPush', source)(self, fetch, { reconcile: async () => {} });
    let pending;
    listeners.push({ data: { json: () => ({ title: 'Build', body: 'Which option?', tag: 'session', session_id: 'session', url: '/#conversation/session' }) }, waitUntil: promise => { pending = promise; } });
    await pending;
    listeners.notificationclick({ notification: { data: { url: '/#conversation/session' }, close: () => calls.push(['close']) }, waitUntil: promise => { pending = promise; } });
    await pending;
    return calls;
  }, source);
  expect(result).toEqual([
    ['show', 'Build', { body: 'Which option?', renotify: true, tag: 'session', data: { url: '/#conversation/session', session_id: 'session' } }],
    ['close'], ['route', { type: 'mj-notification-open', url: 'https://viewer.test/#conversation/session' }], ['focus'],
  ]);
});

// Hard-won: 437e04dc6: expiry and failed rotation lost enrollment until manual Enable.
test('device reconcile retries expiry and rotation and shares disable intent across contexts', async ({ page }) => {
  await mount(page);
  await page.addInitScript(() => {
    window.Notification = class {
      static permission = 'default';
      static requestPermission() { this.permission = 'granted'; localStorage.setItem('permission', 'granted'); return Promise.resolve('granted'); }
    };
    Notification.permission = localStorage.getItem('permission') || 'default';
    window.PushManager = class {};
    const manager = {
      getSubscription: async () => {
        const endpoint = localStorage.getItem('native');
        return endpoint ? { endpoint, toJSON: () => ({ endpoint, keys: { p256dh: 'key', auth: 'auth' } }), unsubscribe: async () => { localStorage.removeItem('native'); return true; } } : null;
      },
      subscribe: async () => { localStorage.setItem('native', 'https://push.example/recreated'); return manager.getSubscription(); },
    };
    window.registration = { active: {}, pushManager: manager, getNotifications: async () => [] };
    Object.defineProperty(navigator, 'serviceWorker', { value: Object.assign(new EventTarget(), { register: async () => registration, getRegistration: async () => registration }) });
  });
  const actions = [];
  let failRenew = false;
  await page.route('**/api/push/key', route => route.fulfill({ json: { public_key: 'BA' } }));
  await page.route('**/api/push/status?*', route => route.fulfill({ json: { subscribed: true } }));
  await page.route('**/api/actions', route => {
    const action = route.request().postDataJSON(); actions.push(action);
    return route.fulfill({ status: failRenew ? 401 : 200, json: { accepted: !failRenew } });
  });
  await page.goto('https://viewer.test/');
  await expect(page.locator('#notification-offer')).toBeVisible();
  await page.locator('#notification-offer-enable').click();
  await expect(page.locator('#notification-offer')).toBeHidden();
  const workerSource = fs.readFileSync(path.join(WEB_ROOT, 'service-worker.js'), 'utf8').replace("importScripts('/push-sync.js');", '');
  await page.evaluate(async source => {
    localStorage.removeItem('native');
    const listeners = {};
    const self = { addEventListener: (name, listener) => listeners[name] = listener, registration };
    new Function('self', source)(self);
    let pending;
    listeners.pushsubscriptionchange({ oldSubscription: { endpoint: 'https://push.example/expired' }, waitUntil: promise => pending = promise });
    await pending;
  }, workerSource);
  expect(actions.some(a => a.action === 'push-renew' && a.old_endpoint.endsWith('/expired'))).toBe(true);
  failRenew = true;
  await page.evaluate(async () => {
    localStorage.setItem('native', 'https://push.example/rotated');
    await MjPush.reconcile(registration).catch(() => {});
  });
  failRenew = false;
  await page.reload();
  await expect.poll(() => actions.filter(a => a.action === 'push-renew' && a.subscription.endpoint.endsWith('/rotated')).length).toBe(2);
  // Another JS context reads the same IndexedDB flag; it cannot recreate after Disable.
  await page.evaluate(async () => {
    await MjPush.setEnabled(false, registration);
  });
  const frame = await page.evaluateHandle(() => { const node = document.createElement('iframe'); document.body.append(node); return node; });
  const other = await (await frame.asElement().contentFrame());
  await other.evaluate(fs.readFileSync(path.join(WEB_ROOT, 'push-sync.js'), 'utf8'));
  await other.evaluate(async () => { window.Notification = class { static permission = 'granted'; }; await MjPush.reconcile(parent.registration); });
  expect(await page.evaluate(() => localStorage.getItem('native'))).toBeNull();
  await page.evaluate(async () => {
    await MjPush.setEnabled(true, registration);
    Notification.permission = 'denied';
    await MjPush.reconcile(registration);
  });
  expect(actions.at(-1).action).toBe('push-unsubscribe');
  expect(await page.evaluate(() => localStorage.getItem('native'))).toBeNull();
});

test('notification offer dismissal persists and iOS tabs show the install hint', async ({ page }) => {
  await mount(page);
  await page.addInitScript(() => {
    window.Notification = class { static permission = 'default'; };
    window.PushManager = class {};
  });
  await page.goto('https://viewer.test/');
  await expect(page.locator('#notification-offer')).toBeVisible();
  await page.locator('#notification-offer-dismiss').click();
  await page.reload();
  await expect(page.locator('#notification-offer')).toBeHidden();
  await page.evaluate(() => new Promise(resolve => { const request = indexedDB.deleteDatabase('mj-notifications'); request.onsuccess = resolve; }));
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'userAgent', { value: 'iPhone Safari' });
    window.matchMedia = () => ({ matches: false, addEventListener() {} });
  });
  await page.reload();
  await expect(page.locator('#notification-offer')).toContainText('Add Mjolnir to your Home Screen');
  await expect(page.locator('#notification-offer-enable')).toBeHidden();
  await page.evaluate(() => { Notification.permission = 'denied'; window.dispatchEvent(new Event('focus')); });
  await expect(page.locator('#notification-offer')).toBeHidden();
});
