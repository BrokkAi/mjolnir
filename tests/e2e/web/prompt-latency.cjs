// Real-browser latency probe for ../prompt_latency.py; excludes optimistic rows.
const fs = require('node:fs');
const { chromium } = require('@playwright/test');

(async () => {
  const [baseUrl, code, sessionId, count, output] = process.argv.slice(2);
  const browser = await chromium.launch({ headless: true });
  try {
    const context = await browser.newContext();
    const response = await context.request.post(`${baseUrl}/auth/session`, { data: { code } });
    if (response.status() !== 204) throw new Error(`Login failed: ${response.status()}`);
    const page = await context.newPage();
    await page.goto(`${baseUrl}/#conversation/${sessionId}`);
    const prompt = page.locator('#prompt-text');
    await prompt.waitFor({ state: 'visible' });
    const rows = [];
    for (let index = 0; index < Number(count); index++) {
      await page.waitForFunction(async ({ baseUrl, sessionId }) => {
        const snapshot = await (await fetch(`${baseUrl}/api/snapshot`)).json();
        return snapshot.sessions.some(s => s.id === sessionId && s.chat_phase === 'idle');
      }, { baseUrl, sessionId });
      const text = `browser-latency-${Date.now()}-${index}`;
      await prompt.fill(text);
      await page.evaluate(text => {
        window.latencyResult = null;
        const feed = document.querySelector('#conversation-feed');
        const observer = new MutationObserver(() => {
          const found = [...feed.querySelectorAll('[data-entry-id]')].some(node =>
            !node.dataset.entryId.startsWith('pending') && node.textContent.includes(text));
          if (found) {
            window.latencyResult = performance.now() - window.latencyStart;
            observer.disconnect();
          }
        });
        observer.observe(feed, { childList: true, subtree: true, characterData: true, attributes: true });
        document.querySelector('#prompt-text').addEventListener('keydown', () => {
          window.latencyStart = performance.now();
        }, { once: true, capture: true });
      }, text);
      await prompt.press('Enter');
      await page.waitForFunction(() => window.latencyResult !== null);
      rows.push({ text, render_ms: await page.evaluate(() => window.latencyResult) });
    }
    fs.writeFileSync(output, JSON.stringify(rows, null, 2));
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
