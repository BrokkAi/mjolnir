// Phone layout and accessibility, at the three widths that matter.
//
// The reliability spec drives the whole operator flow once at a phone size.
// This one drives a much smaller flow at several sizes and asserts the things
// that are true of every page rather than of any one of them: that nothing
// scrolls sideways, that a finger can hit every control, that a reader who
// cannot see colour still learns what colour was saying, and that the header
// and composer stay where they were put while a transcript scrolls under them.

const { test, expect } = require('@playwright/test');
const fs = require('node:fs');
const path = require('node:path');

const VIEWER_CSS = fs.readFileSync(
  path.resolve(__dirname, '../../../mj-controller/src/web/viewer.css'),
  'utf8',
);

function required(name) {
  const value = process.env[name];
  if (!value) throw new Error(`missing ${name}`);
  return value;
}

/// The widths a phone actually is, plus one that is not a phone at all.
///
/// 320 is the narrowest screen still in use and the one every containment bug
/// shows up on first; 390 is an ordinary modern phone; 900 proves the wider
/// presentation stays usable without a desktop-only layout.
const VIEWPORTS = [
  { name: '320x568', width: 320, height: 568 },
  { name: '390x844', width: 390, height: 844 },
  { name: '900x900', width: 900, height: 900 },
];

/// Every route, as a hash to visit.
async function routes(page) {
  const workspaceId = await page.evaluate(() => location.hash.replace('#workspace/', ''));
  return ['', `#workspace/${workspaceId}`, `#workspace/${workspaceId}/new`, `#workspace/${workspaceId}/resume`, '#targets', '#quota'];
}

async function unlock(page, baseUrl, code) {
  await page.goto(baseUrl);
  await expect(page.locator('#login')).toBeVisible();
  await page.locator('#code').fill(code);
  await page.getByRole('button', { name: 'Enter' }).click();
  await expect(page.locator('#app')).toBeVisible();
  await expect(page).toHaveURL(/#workspace\//);
}

test('a very long unbroken dashboard title stays bounded and remains readable', async ({ page }) => {
  const title = 'handoff'.repeat(4096);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.setContent(
    `<style>${VIEWER_CSS}</style><main id="app"><div id="sessions"></div></main>`,
  );
  const metrics = await page.evaluate(longTitle => {
    // Use the same article > h3 structure as sessionCard, but keep this
    // synthetic title local to the browser so no live session can remove it.
    const card = document.createElement('article');
    card.className = 'card session';
    const heading = document.createElement('h3');
    heading.textContent = longTitle;
    card.append(heading);
    document.querySelector('#sessions').append(card);

    const style = getComputedStyle(heading);
    return {
      documentWidth: document.documentElement.scrollWidth,
      viewportWidth: document.documentElement.clientWidth,
      headingWidth: heading.getBoundingClientRect().width,
      headingScrollWidth: heading.scrollWidth,
      headingHeight: heading.getBoundingClientRect().height,
      lineHeight: Number.parseFloat(style.lineHeight),
      text: heading.textContent,
    };
  }, title);

  expect(metrics.documentWidth).toBeLessThanOrEqual(metrics.viewportWidth + 1);
  expect(metrics.headingScrollWidth).toBeLessThanOrEqual(metrics.headingWidth + 1);
  expect(metrics.headingHeight).toBeLessThanOrEqual(metrics.lineHeight * 3 + 1);
  expect(metrics.text).toBe(title);
  await expect(page.getByRole('heading', { name: title, exact: true })).toHaveCount(1);
});

test('paperclip stays hidden for sessions without image support', async ({ page }) => {
  const html = fs.readFileSync(
    path.resolve(__dirname, '../../../mj-controller/src/web/viewer.html'),
    'utf8',
  );
  await page.setContent(html.replace(/<script\b[^>]*>[\s\S]*?<\/script>/g, '').replace(/<link\b[^>]*>/g, ''));
  await page.addStyleTag({ content: VIEWER_CSS });
  await page.evaluate(() => {
    document.querySelector('#app').classList.remove('hidden');
    document.querySelector('#conversation').classList.remove('hidden');
  });
  const attach = page.getByRole('button', { name: 'Attach one or more images', includeHidden: true });
  await expect(attach).toBeHidden();
  await attach.evaluate(node => { node.hidden = false; });
  await expect(attach).toBeVisible();
  const box = await attach.boundingBox();
  expect(box.width).toBeGreaterThanOrEqual(44);
  expect(box.height).toBeGreaterThanOrEqual(44);
});

for (const viewport of VIEWPORTS) {
  test(`the viewer fits ${viewport.name} and stays reachable`, async ({ browser }) => {
    const baseUrl = required('MJ_BROWSER_BASE_URL');
    const code = required('MJ_BROWSER_CODE');

    const context = await browser.newContext({
      ignoreHTTPSErrors: true,
      viewport: { width: viewport.width, height: viewport.height },
    });
    const page = await context.newPage();
    const pageErrors = [];
    page.on('pageerror', error => pageErrors.push(error.message));

    try {
      await unlock(page, baseUrl, code);

      for (const hash of await routes(page)) {
        await page.evaluate(target => {
          location.hash = target;
        }, hash);
        // Give the router its frame.
        await page.waitForTimeout(150);

        // Nothing may push the page sideways. This is the assertion that
        // catches a flex child refusing to shrink below its content, which is
        // the most common way a phone page breaks.
        const overflow = await page.evaluate(() => ({
          scrollWidth: document.documentElement.scrollWidth,
          clientWidth: document.documentElement.clientWidth,
        }));
        expect(
          overflow.scrollWidth,
          `${hash || '(root)'} scrolls sideways at ${viewport.name}`,
        ).toBeLessThanOrEqual(overflow.clientWidth + 1);

        // Every visible control has to be big enough to hit. 44 is the
        // smallest target a finger reliably lands on.
        const targets = await page.evaluate(() => {
          const offenders = [];
          let examined = 0;
          for (const node of document.querySelectorAll('button, select, input, summary, a')) {
            if (!node.offsetParent && node.tagName !== 'SUMMARY') continue;
            const box = node.getBoundingClientRect();
            if (box.width === 0 && box.height === 0) continue;
            examined += 1;
            if (box.height < 44 - 0.5 || box.width < 44 - 0.5) {
              offenders.push(
                `${node.tagName}#${node.id || ''}.${node.className || ''} ${Math.round(box.width)}x${Math.round(box.height)}`,
              );
            }
          }
          return { offenders, examined };
        });
        expect(targets.offenders, `${hash || '(root)'} has controls too small to hit`).toEqual([]);
        // A check that measured nothing has proved nothing. Every route in
        // this list has controls on it, so finding none means the page did not
        // render rather than that it passed.
        expect(
          targets.examined,
          `${hash || '(root)'} rendered no controls to measure at ${viewport.name}`,
        ).toBeGreaterThan(0);

        // A form control below 16px makes iOS Safari zoom on focus and never
        // zoom back, which strands the person at a magnified page.
        const tiny = await page.evaluate(() => {
          const offenders = [];
          for (const node of document.querySelectorAll('input, select, textarea, [contenteditable]')) {
            if (!node.offsetParent) continue;
            const size = Number.parseFloat(getComputedStyle(node).fontSize);
            if (size < 16) offenders.push(`${node.tagName}#${node.id || ''} ${size}px`);
          }
          return offenders;
        });
        expect(tiny, `${hash || '(root)'} has form text below 16px`).toEqual([]);
      }

      // Selection is said, not only coloured: the chosen workspace carries
      // aria-current, so a reader who cannot see the border still learns which
      // one is open.
      await expect(page.locator('#workspaces [aria-current="page"]')).toHaveCount(1);

      // The menu is a menu, and Escape closes it.
      await page.locator('#menu-button').click();
      await expect(page.locator('#menu-button')).toHaveAttribute('aria-expanded', 'true');
      await page.keyboard.press('Escape');
      await expect(page.locator('#menu')).toBeHidden();

      // There is one polite announcer, and route changes speak through it.
      await expect(page.locator('#announcer')).toHaveAttribute('aria-live', 'polite');
      await page.evaluate(() => {
        location.hash = '#targets';
      });
      await expect(page.locator('#announcer')).toHaveText('Targets');

      expect(pageErrors).toEqual([]);
    } finally {
      await context.close();
    }
  });
}
