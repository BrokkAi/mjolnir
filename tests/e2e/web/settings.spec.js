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
const ASSETS = new Set(['viewer.html', 'viewer.js', 'viewer.css', 'markdown.js', 'tool-output.js', 'manifest.webmanifest']);

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

  await page.locator('#back').click();
  await page.locator('.settings-link', { hasText: 'Sub-agents' }).click();
  const limit = page.getByLabel('Maximum concurrent children');
  await limit.fill('99');
  await limit.press('Enter');
  await expect(page.locator('#settings .error')).toHaveText('Enter a whole number from 1 to 64.');
  await capture('a refused edit keeps what was typed beside the reason');

  const subtleModel = page.getByRole('combobox', { name: 'Subtle model', exact: true });
  const straightforwardModel = page.getByRole('combobox', { name: 'Straightforward model', exact: true });
  const straightforwardEffort = page.getByRole('combobox', { name: 'Straightforward effort', exact: true });
  await expect(page.locator('#settings-heading')).toHaveText('Sub-agents');
  await expect(subtleModel.locator('option:checked')).toHaveText('None');
  await expect(straightforwardModel.locator('option:checked')).toHaveText('None');
  await expect(page.getByRole('combobox', { name: 'Subtle effort', exact: true })).toBeHidden();
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
