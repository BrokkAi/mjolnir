import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../../../mj-controller/src/web/viewer.js', import.meta.url), 'utf8');
const helpers = source.slice(source.indexOf('function formatQuotaReset('), source.indexOf('function renderQuota('));
const cases = JSON.parse(readFileSync(new URL('../../../mj-client/src/quota/reset_display_cases.json', import.meta.url), 'utf8'));

test('quota countdowns match the shared Rust cases', () => {
  const context = vm.createContext({});
  vm.runInContext(helpers, context);
  for (const entry of cases) {
    assert.equal(context.formatQuotaReset(entry.now, {
      resets_at_epoch_seconds: entry.reset,
      resets_at: entry.fallback,
      reset_countdown_style: entry.style,
      banked_resets: entry.banked_resets,
    }), entry.expected, entry.name);
  }
});

test('quota clock changes only the reset text using server time without a snapshot', () => {
  let now = 1000000;
  let writes = 0;
  const node = {
    _quotaWindow: { resets_at_epoch_seconds: now + 60, banked_resets: 1 },
    get textContent() { return this.text; },
    set textContent(value) { writes++; this.text = value; },
  };
  const context = vm.createContext({
    serverClockMs: () => now * 1000,
    quotaPanel: { querySelectorAll: () => [node] },
  });
  vm.runInContext(helpers, context);
  context.updateQuotaClocks();
  assert.equal(node.textContent, 'resets 1m [1]');
  context.updateQuotaClocks();
  assert.equal(writes, 1);
  now++;
  context.updateQuotaClocks();
  assert.equal(node.textContent, 'resets <1m [1]');
  now += 59;
  context.updateQuotaClocks();
  assert.equal(node.textContent, 'resets now [1]');
});

test('a rate limited profile reads the retry time in minutes and stops when the hold ends', () => {
  const context = vm.createContext({});
  vm.runInContext(helpers, context);
  const quota = { rate_limited_until_epoch_seconds: 1000 + 5 * 60 };
  assert.equal(context.quotaHoldText(1000, quota), 'rate limited · retry in 5 min');
  assert.equal(context.quotaHoldText(1000 + 5 * 60 - 1, quota), 'rate limited · retry in 1 min');
  assert.equal(context.quotaHoldText(1000 + 5 * 60, quota), '');
  assert.equal(context.quotaHoldText(1000, {}), '');
  assert.equal(context.quotaHoldText(1000, undefined), '');
});
