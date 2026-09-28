const { test, expect } = require('@playwright/test');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '../../../mj-controller/src/web');
const source = fs.readFileSync(path.join(root, 'viewer.js'), 'utf8');
const css = fs.readFileSync(path.join(root, 'viewer.css'), 'utf8');
const html = fs.readFileSync(path.join(root, 'viewer.html'), 'utf8');
function between(start, end) {
  const from = source.indexOf(start);
  const to = source.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`Missing viewer section ${start}`);
  return source.slice(from, to);
}
const renderSource = [
  between('function el(', '\nconst login'),
  between('function band(', '\n/// The freshness'),
  between('function formatQuotaReset(', '\nasync function runRefresh('),
  between('async function runRefresh(', '\n// ---------------------------------------------------------------------------\n// Data'),
].join('\n');

function profiles() {
  return ['claude', 'claude2', 'codex', 'codex2', 'codex3', 'kimi'].map((id, index) => ({
    id,
    harness_kind: id.replace(/\d+$/, ''),
    quota: {
      windows: [
        { label: 'Week', percent_used: index * 20, resets_at: '12:00 Sep 12' },
        ...(index % 2 ? [] : [{ label: '5H', percent_used: 10, projects_exhaustion_before_reset: true }]),
      ],
      refreshed_at_epoch_seconds: 1788642251,
    },
  })).concat({ id: 'deepseek', harness_kind: 'deepseek', quota: { summary: 'API' } });
}

async function mount(page, data) {
  // Real shell, CSS, and rendering functions; no daemon or provider access.
  await page.setContent(html.replace(/<script\b[^>]*>[\s\S]*?<\/script>/g, '').replace(/<link\b[^>]*>/g, ''));
  await page.addStyleTag({ content: css });
  await page.evaluate(() => {
    document.querySelector('#login').classList.add('hidden');
    document.querySelector('#app').classList.remove('hidden');
    document.querySelector('#connection').classList.add('hidden');
    document.querySelector('#dashboard').classList.add('hidden');
    document.querySelector('#quota-page').classList.remove('hidden');
    document.querySelector('#menu-button').classList.remove('hidden');
    document.querySelector('#shell-title').textContent = 'Quota';
    const workspace = document.createElement('button');
    workspace.textContent = 'Workspace';
    document.querySelector('#workspaces').append(workspace);
  });
  await page.addScriptTag({ content: `const quotaPanel = document.querySelector('#quota'); function serverClockMs() { return Date.now(); } let snapshot = ${JSON.stringify({ profiles: data })};\n${renderSource}\nrenderQuota();` });
}

for (const viewport of [{ width: 320, height: 568 }, { width: 390, height: 844 }]) {
  test(`all six quotas plus API fit ${viewport.width}x${viewport.height}, with details on demand`, async ({ page }) => {
    await page.setViewportSize(viewport);
    await mount(page, profiles());
    const rows = page.locator('.quota-profile');
    await expect(rows).toHaveCount(7);
    await expect(page.locator('.quota-overview-heading span')).toHaveText(['% left', 'Week', '5H']);
    await expect(page.locator('[data-profile-id="claude"] summary .quota-value')).toHaveText(['100%', '90% !']);
    const metrics = await page.evaluate(() => ({
      width: document.documentElement.scrollWidth,
      height: document.documentElement.scrollHeight,
      rows: [...document.querySelectorAll('.quota-overview-row')].map(node => node.getBoundingClientRect().height),
    }));
    expect(metrics.width).toBeLessThanOrEqual(viewport.width);
    expect(metrics.height).toBeLessThanOrEqual(viewport.height);
    expect(metrics.rows.every(height => height >= 44)).toBe(true);
    await expect(page.locator('[data-profile-id="claude"] summary')).toHaveAttribute('aria-label', /Week: 100% left/);
    await expect(page.locator('[data-profile-id="claude"] summary')).toHaveAttribute('aria-label', /projected to run out before reset/);
    await expect(page.locator('[data-profile-id="codex2"] summary')).toHaveAttribute('aria-label', /5H: not reported/);
    const row = page.locator('[data-profile-id="claude"]');
    await row.locator('summary').focus();
    await page.keyboard.press('Enter');
    await expect(row.locator('.quota-details')).toBeVisible();
    await expect(row).toContainText('resets 12:00 Sep 12');
    await expect(row).toContainText('on course to run out first');
    await expect(row.getByRole('button', { name: 'Refresh' })).toHaveAttribute('data-payload', JSON.stringify({ profile_id: 'claude' }));
    await page.evaluate(() => { snapshot.profiles[0].quota.windows[0].percent_used = 17; renderQuota(); });
    await expect(row).toHaveAttribute('open', '');
    await expect(row.locator('summary')).toHaveAttribute('aria-label', /Week: 83% left/);
    await expect(row.locator('summary')).toBeFocused();
  });
}

test('quota keeps unknown, stale and failed readings explicit and unusual labels contained', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 568 });
  await mount(page, [
    { id: 'a-very-long-profile-identifier'.repeat(3), quota: { stale: true, windows: [{ label: 'Monthly credits', percent_used: 80 }] } },
    { id: 'failed', quota: { has_error: true, windows: [{ label: 'Monthly credits' }] } },
    { id: 'missing' },
  ]);
  await expect(page.locator('[data-profile-id="failed"] summary')).toHaveAttribute('aria-label', /probe failed; last reading.*Monthly credits: unknown/);
  await expect(page.locator('[data-profile-id="missing"] summary')).toContainText('No reading yet');
  await expect(page.locator('.quota-profile').first().locator('summary')).toContainText('stale');
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(320);
});

test('failed quota refresh is visible and leaves the control available for retry', async ({ page }) => {
  await mount(page, profiles());
  const result = await page.evaluate(async () => {
    globalThis.request = async (_url, options) => {
      globalThis.sent = JSON.parse(options.body);
      throw new Error('Refresh unavailable');
    };
    const row = document.querySelector('.quota-profile');
    row.open = true;
    const control = row.querySelector('button[data-refresh]');
    await runRefresh(control, row.querySelector('.quota-error'));
    return { sent, disabled: control.disabled };
  });
  expect(result).toEqual({ sent: { action: 'refresh-quota', profile_id: 'claude' }, disabled: false });
  await expect(page.locator('.quota-error').first()).toHaveText('Refresh unavailable');
});


test('banked reset countdowns tick without snapshots or replacing focused controls', async ({ page }) => {
  await page.clock.install({ time: new Date(1_000_000_000) });
  await page.setViewportSize({ width: 320, height: 568 });
  const data = profiles();
  data[0].quota.windows = [
    { label: 'Week', percent_used: 30, resets_at_epoch_seconds: 1_000_000 + 5 * 86400 + 7 * 3600, reset_countdown_style: 'long', banked_resets: 1 },
    { label: '5H', percent_used: 20, resets_at_epoch_seconds: 1_000_060, reset_countdown_style: 'five_hour' },
  ];
  data[1].quota.windows = [{ label: 'Week', resets_at_epoch_seconds: 1_000_060, banked_resets: 0 }];
  await mount(page, data);
  const row = page.locator('[data-profile-id="claude"]');
  await row.locator('summary').click();
  await expect(row.locator('.quota-reset')).toHaveText(['resets 5d 7h [1]', 'resets 1m']);
  await expect(page.locator('[data-profile-id="claude2"] .quota-reset')).toHaveText('resets 1m');
  await row.getByRole('button', { name: 'Refresh' }).focus();
  await page.evaluate(() => {
    window.originalReset = document.querySelector('.quota-reset');
    window.setInterval(updateQuotaClocks, 1000);
  });
  await page.clock.runFor(1000);
  await expect(row.locator('.quota-reset')).toHaveText(['resets 5d 6h [1]', 'resets <1m']);
  await page.clock.runFor(59000);
  await expect(row.locator('.quota-reset').nth(1)).toHaveText('resets now');
  await expect(row).toHaveAttribute('open', '');
  await expect(row.getByRole('button', { name: 'Refresh' })).toBeFocused();
  expect(await page.evaluate(() => document.querySelector('.quota-reset') === window.originalReset)).toBe(true);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(320);
});
