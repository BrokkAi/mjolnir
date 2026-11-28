const fs = require('node:fs');
const { test, expect } = require('@playwright/test');
const { requireLabEnvironment } = require('./lab-env');

function escapeForRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

async function codeLogin(page, baseUrl, code) {
  await page.goto(baseUrl);
  await expect(page.getByRole('heading', { name: 'Unlock Mjolnir' })).toBeVisible();
  await page.getByLabel('Code', { exact: true }).fill(code);
  await page.getByRole('button', { name: 'Enter', exact: true }).click();
  // The signed-in shell owns the screen once the snapshot lands.
  await expect(page.getByRole('navigation', { name: 'Primary' }).or(page.getByRole('navigation', { name: 'Mjolnir' })).first()).toBeVisible();
}

async function openMenu(page) {
  await page.getByRole('button', { name: 'Open menu' }).click();
  await expect(page.getByRole('navigation', { name: 'Mjolnir' })).toBeVisible();
}

test('real viewer converges with a TUI after an SSE disconnect', async ({ browser }) => {
  test.setTimeout(360_000);
  const lab = requireLabEnvironment();
  const baseUrl = lab.MJ_BROWSER_BASE_URL;
  const code = lab.MJ_BROWSER_CODE;
  const qrLoginUrl = lab.MJ_BROWSER_QR_URL;
  const title = lab.MJ_BROWSER_TITLE;
  const projectDirectory = lab.MJ_BROWSER_PROJECT_DIRECTORY;
  const readyMarker = lab.MJ_BROWSER_READY_MARKER;
  const changedMarker = lab.MJ_TUI_CHANGED_MARKER;
  const tracePath = lab.MJ_BROWSER_TRACE;
  const screenshotPath = lab.MJ_BROWSER_SCREENSHOT;
  const started = Date.now();
  const stage = value => process.stdout.write(`browser-stage: ${value} (+${Date.now() - started}ms)\n`);
  const browserErrors = [];
  const expectedAuthErrors = [];
  const expectedTransitionErrors = [];
  const transitionResponses = [];
  const authPhase = new WeakSet();
  const trackErrors = page => {
    page.on('pageerror', error => browserErrors.push(`pageerror: ${error.message}`));
    page.on('console', message => {
      if (message.type() !== 'error') return;
      const text = message.text();
      if (authPhase.has(page) && text.includes('server responded with a status of 401')) {
        expectedAuthErrors.push(text);
      } else if (text.includes('server responded with a status of 409')) {
        expectedTransitionErrors.push(text);
      } else {
        browserErrors.push(`console.error: ${text}`);
      }
    });
    page.on('response', response => {
      if (response.status() === 409) transitionResponses.push({ url: response.url(), body: response.json() });
    });
  };

  // A protected conversation route must remain a login page while its
  // snapshot request is unauthorized; route restoration cannot dereference
  // an absent snapshot.
  const lockedContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const lockedPage = await lockedContext.newPage();
  trackErrors(lockedPage);
  authPhase.add(lockedPage);
  await lockedPage.goto(`${baseUrl}/#conversation/not-authenticated`);
  await expect(lockedPage.getByRole('heading', { name: 'Unlock Mjolnir' })).toBeVisible();
  await expect.poll(() => browserErrors).toEqual([]);
  await lockedContext.close();

  // Authentication secrets are intentionally kept out of Playwright traces.
  stage('qr-login');
  const qrContext = await browser.newContext({ ignoreHTTPSErrors: true });
  const qrPage = await qrContext.newPage();
  trackErrors(qrPage);
  await qrPage.goto(qrLoginUrl);
  await expect(qrPage.getByRole('navigation', { name: 'Mjolnir' })).toBeVisible();
  // The login token must not survive in the URL.
  expect(qrPage.url()).not.toContain('token=');
  expect(qrPage.url().startsWith(`${baseUrl}/`)).toBe(true);
  await qrContext.close();

  stage('code-login');
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 390, height: 844 },
  });
  const page = await context.newPage();
  trackErrors(page);
  page.setDefaultTimeout(15_000);
  try {
    authPhase.add(page);
    await codeLogin(page, baseUrl, code);
    authPhase.delete(page);
    await context.tracing.start({ screenshots: true, snapshots: true, sources: true });

    stage('snapshot-rendered');
    // The workspace rail lists only real workspaces; Notifications is a
    // separate screen. Picking the lab's workspace records it in the URL.
    const workspaceNav = page.getByRole('navigation', { name: 'Workspaces' });
    const workspaceButtons = workspaceNav.getByRole('button');
    await expect(workspaceButtons.first()).toBeVisible();
    const names = await workspaceButtons.allTextContents();
    expect(names.some(name => name.trim() === 'All workspaces')).toBe(true);
    expect(names.some(name => /notifications/i.test(name))).toBe(false);
    expect(names.length).toBeGreaterThan(1);
    await workspaceButtons.nth(1).click();
    await expect(page).toHaveURL(/#workspace\/.+$/);
    const workspaceHash = new URL(page.url()).hash;

    // This daemon has isolated config and a fake harness. Exercise the real
    // bundle write/publish path without cloning or contacting a provider.
    stage('bundle-create');
    const bundles = await page.evaluate(async () => {
      const create = async source => {
        const response = await fetch('/api/v1/bundles', {
          method: 'POST', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ source }),
        });
        if (!response.ok) throw new Error(`bundle creation failed: ${response.status}`);
        return response.json();
      };
      const [first, parallel] = await Promise.all([
        create('example/browser-fixture'), create('example/browser-fixture-other'),
      ]);
      const reused = await create('https://github.com/example/browser-fixture.git');
      const snapshot = await fetch('/api/v1/snapshot').then(response => response.json());
      const sessions = snapshot.sessions.map(row => {
        const resolved = { ...row };
        for (const field of ['available_commands', 'capabilities', 'config_options', 'compatible_resume_targets', 'incompatible_resume_targets']) {
          const ref = resolved[`${field}_ref`];
          if (ref === undefined) continue;
          if (!Object.hasOwn(snapshot.interned, ref)) throw new Error(`missing interned ${field}`);
          resolved[field] = snapshot.interned[ref];
          delete resolved[`${field}_ref`];
        }
        return resolved;
      });
      return { first, parallel, reused, ids: snapshot.bundles.map(bundle => bundle.id), cursor: snapshot.cursor, sessions };
    });
    expect(bundles.cursor).toEqual(expect.objectContaining({ incarnation: expect.any(String), sequence: expect.any(Number) }));
    expect(bundles.reused.bundle_id).toBe(bundles.first.bundle_id);
    expect(bundles.ids).toContain(bundles.first.bundle_id);
    expect(bundles.ids).toContain(bundles.parallel.bundle_id);

    stage('quota');
    // Quota is a page reached from the menu, not a card on the dashboard.
    await openMenu(page);
    await page.getByRole('navigation', { name: 'Mjolnir' }).getByRole('button', { name: 'Quota' }).click();
    await expect(page).toHaveURL(/#quota$/);
    await expect(page.locator('main')).toContainText(/fake/i);
    // The lab's harness reports no usable quota, so the page has to say that
    // rather than draw an empty bar and imply a healthy limit.
    await expect(page.locator('main')).toContainText(/unavailable|no quota has been reported/i);
    await page.getByRole('button', { name: /Refresh quota for/ }).first().click();

    stage('targets');
    // Targets is a page too, and it says what state its readings are in.
    await openMenu(page);
    await page.getByRole('navigation', { name: 'Mjolnir' }).getByRole('button', { name: 'Targets' }).click();
    await expect(page).toHaveURL(/#targets$/);
    await expect(page.locator('main')).toContainText('localhost');
    // The keyboard-inset and meter techniques both set a custom property
    // through the CSSOM. The policy forbids inline style attributes parsed
    // from markup; this pins that a CSSOM write is still permitted, because
    // the whole design leans on it.
    const cssomWorks = await page.evaluate(() => {
      const probe = document.createElement('div');
      probe.style.setProperty('--fill', '42%');
      return probe.style.getPropertyValue('--fill');
    });
    expect(cssomWorks).toBe('42%');
    await page.getByRole('navigation', { name: 'Primary' }).getByRole('button', { name: 'Sessions' }).click();
    await workspaceButtons.nth(1).click();
    await expect(page).toHaveURL(new RegExp(escapeForRegExp(workspaceHash) + '$'));

    stage('new-session');
    // The New flow reviews the launch before committing anything.
    await page.getByRole('link', { name: 'New session' }).click();
    await expect(page).toHaveURL(/\/new$/);
    // Config::with_local_targets also offers container candidates. Select the
    // bare host explicitly so this scenario exercises the directory flow.
    const fakeAccount = page.getByRole('button', { name: /^fake \(/ });
    if (!(await fakeAccount.isVisible())) {
      await page.getByRole('button', { name: /^Expand .* accounts$/ }).first().click();
    }
    await fakeAccount.click();
    const localhostValue = await page.getByLabel('Target').evaluate(select => {
      const option = [...select.options].find(candidate => /^localhost/.test(candidate.textContent));
      return option ? option.value : null;
    });
    expect(localhostValue).toBeTruthy();
    await page.getByLabel('Target').selectOption(localhostValue);
    // The selected target is bare, so the project step asks for a directory
    // rather than offering a bundle.
    await expect(page.getByLabel('Project directory')).toBeVisible();
    await page.getByLabel('Project directory').fill(projectDirectory);
    // The preflight must settle: either it passes, or dirty repositories gate
    // the launch behind explicit acknowledgements.
    await expect(page.getByText(/Preflight passed|Uncommitted changes found/)).toBeVisible();
    const acknowledgements = page.getByRole('checkbox', { name: /uncommitted changes/ });
    for (const checkbox of await acknowledgements.all()) await checkbox.check();
    await page.getByText('Advanced options').click();
    await page.getByLabel('Session title').fill(title);
    await page.getByRole('button', { name: 'Launch' }).click();
    stage('session-requested');

    // The new session appears on the dashboard and opens its conversation.
    const sessionCard = page.getByTestId('session-card-' + '').locator('..');
    const session = page.locator('article[data-testid^="session-card-"]').filter({ hasText: title });
    await expect(session).toBeVisible();
    stage('session-running');
    await session.getByRole('button', { name: title, exact: true }).click();
    await expect(page).toHaveURL(/#conversation\//);
    await expect(page.getByRole('heading', { name: title })).toBeVisible();

    // The transcript scrolls inside itself, so the page cannot scroll away
    // from the composer while somebody is reading.
    await expect(page.getByTestId('conversation-scroll')).toBeVisible();
    // The composer offers the commands the harness can actually satisfy, and
    // /help is delivered to the selected harness.
    const message = page.getByTestId('composer').getByLabel('Message', { exact: true });
    await message.fill('/he');
    await expect(page.getByTestId('command-palette')).toBeVisible();
    await expect(page.getByTestId('command-palette')).toContainText('/help');
    await page.keyboard.press('Enter');
    await expect(message).toHaveValue('/help ');
    await message.press('Enter');
    await expect(page.getByTestId('transcript-rows')).toContainText('reliability reply: /help', { timeout: 60_000 });

    const sendPrompt = async text => {
      await message.fill(text);
      await message.press('Enter');
    };
    const rows = page.getByTestId('transcript-rows');
    stage('conversation-prompts');
    await sendPrompt('hello live');
    await expect(rows).toContainText('reliability reply: hello live');
    await sendPrompt('live tool call');
    await expect(rows).toContainText('tool call finished');
    const tool = page.getByTestId('tool-card').last();
    await expect(tool).toBeVisible();
    await tool.getByRole('button').first().click();
    await expect(tool.getByTestId('tool-card-body')).toContainText('echo live input');
    await expect(tool.getByTestId('tool-card-body')).toContainText('live tool output');
    await sendPrompt('live slow turn');
    await expect(rows).toContainText('working on slow turn...');
    await page.getByRole('button', { name: 'Interrupt', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Interrupt', exact: true })).toHaveCount(0);

    stage('elicitation-notifications');
    await sendPrompt('live elicitation');
    await expect(page.getByTestId('elicitation-dock')).toBeAttached();
    await expect(page.getByTestId('elicitation-sheet-body')).toBeVisible();
    await page.getByRole('button', { name: 'Peek transcript' }).click();
    await page.getByRole('button', { name: /^Notifications,/ }).click();
    await expect(page).toHaveURL(/#notifications$/);
    const notification = page.locator('article[data-testid^="notification-"]').filter({ hasText: title });
    await expect(notification).toContainText(/needs input|question waiting/i);
    await notification.getByRole('button', { name: title }).click();
    await expect(page).toHaveURL(/#notifications\/[^/]+$/);
    const sheet = page.getByTestId('elicitation-sheet-body');
    await expect(sheet).toBeVisible();
    await sheet.getByRole('textbox').fill('live label');
    await page.getByRole('button', { name: 'Next', exact: true }).click();
    await sheet.getByText('No', { exact: true }).click();
    await expect(sheet.getByRole('checkbox')).toBeChecked();
    await page.getByRole('button', { name: 'Next', exact: true }).click();
    await sheet.getByText('Second', { exact: true }).click();
    await expect(sheet.getByRole('radio', { name: 'Second' })).toBeChecked();
    await page.getByRole('button', { name: 'Next', exact: true }).click();
    await sheet.getByText('Alpha', { exact: true }).click();
    await sheet.getByText('Gamma', { exact: true }).click();
    await expect(sheet.getByRole('checkbox', { name: 'Alpha' })).toBeChecked();
    await expect(sheet.getByRole('checkbox', { name: 'Gamma' })).toBeChecked();
    await page.getByRole('button', { name: 'Submit', exact: true }).click();
    await expect(rows).toContainText('elicitation response:');
    for (const value of ['live label', 'true', 'second', 'Alpha', 'Gamma']) await expect(rows).toContainText(value);
    await page.evaluate(() => { location.hash = '#notifications'; });
    await expect(notification.filter({ hasText: /needs input|question waiting/i })).toHaveCount(0);
    await page.goBack();
    await sendPrompt('live permission');
    await expect(page.getByTestId('elicitation-sheet-body')).toBeVisible();
    await page.getByRole('button', { name: 'Approve', exact: true }).click();
    await expect(rows).toContainText('permission response:');
    stage('elicitation-permission-complete');

    // A draft is stored against this viewer, so it survives a reload — and a
    // second viewer with its own cookie must not see it.
    stage('draft-persistence');
    const conversationUrl = page.url();
    await message.fill('a draft that should survive');
    await page.waitForTimeout(900);
    await page.reload();
    await expect(page.getByTestId('composer').getByLabel('Message', { exact: true })).toHaveValue('a draft that should survive');

    const otherContext = await browser.newContext({ ignoreHTTPSErrors: true });
    const otherPage = await otherContext.newPage();
    trackErrors(otherPage);
    authPhase.add(otherPage);
    await codeLogin(otherPage, baseUrl, code);
    authPhase.delete(otherPage);
    await otherPage.goto(conversationUrl);
    await expect(otherPage.getByRole('heading', { name: title })).toBeVisible();
    await expect(otherPage.getByTestId('composer').getByLabel('Message', { exact: true })).toHaveValue('');
    await otherContext.close();

    await page.getByTestId('composer').getByLabel('Message', { exact: true }).fill('');
    await page.waitForTimeout(900);
    // A notification-origin conversation returns to the inbox; the Sessions
    // tab then opens the workspace dashboard.
    await page.getByRole('button', { name: 'Back', exact: true }).click();
    await expect(page).toHaveURL(/#notifications$/);
    await page.getByRole('navigation', { name: 'Primary' }).getByRole('button', { name: 'Sessions' }).click();
    await workspaceButtons.nth(1).click();
    await expect(page.getByRole('heading', { name: 'Workspaces' })).toBeVisible();

    stage('settings');
    await openMenu(page);
    await page.getByRole('navigation', { name: 'Mjolnir' }).getByRole('button', { name: 'Settings' }).click();
    await expect(page.getByRole('heading', { name: 'Sessions', exact: true })).toBeVisible();
    for (const item of ['Guardian', 'Code Review', 'Sub-agents', 'SessionWiki', 'Web Access']) {
      await expect(page.getByRole('button', { name: new RegExp(item) }).first()).toBeVisible();
    }
    await expect(page.getByText('Unsaved changes')).toHaveCount(0);
    await page.getByRole('button', { name: /Guardian/ }).first().click();
    const guardian = page.getByRole('switch', { name: 'Local classifier', exact: true });
    const initialGuardian = await guardian.getAttribute('aria-checked');
    await guardian.click();
    await expect(page.getByText('Unsaved changes')).toBeVisible();
    const settingsRoute = async route => {
      await page.evaluate(value => { location.hash = value; }, `#settings/${route}`);
      await expect(page).toHaveURL(new RegExp(`#settings/${route}$`));
    };
    await settingsRoute('review');
    await expect(page.getByRole('button', { name: 'Autoconfigure' })).toBeVisible();
    await page.getByRole('button', { name: /^reviewers/ }).click();
    await page.getByRole('button', { name: 'Add reviewer' }).click();
    await expect(page).toHaveURL(/#settings\/review\/reviewers\/\d+$/);
    await page.getByRole('button', { name: 'Remove this entry' }).click();
    await expect(page).toHaveURL(/#settings\/review\/reviewers$/);
    await settingsRoute('subagents');
    const maxChildren = page.getByLabel('Maximum concurrent children');
    await maxChildren.fill('0');
    await maxChildren.blur();
    await expect(page.getByRole('alert').first()).toBeVisible();
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByText('Fix the rejected settings before saving.')).toBeVisible();
    await maxChildren.fill('12');
    await maxChildren.blur();
    await expect(page.getByRole('alert')).toHaveCount(0);
    await page.getByRole('button', { name: /Straightforward/ }).first().click();
    await page.getByRole('button', { name: 'Add', exact: true }).click();
    await expect(page).toHaveURL(/#settings\/subagents\/straightforward\/\d+$/);
    await page.getByRole('button', { name: 'Remove this entry' }).click();
    await expect(page).toHaveURL(/#settings\/subagents\/straightforward$/);
    await settingsRoute('sessionwiki');
    await page.getByLabel('Archive after (days)').fill('30');
    await page.getByLabel('Archive after (days)').blur();
    await settingsRoute('phone');
    for (const name of ['Enabled', 'Listen address and port', 'Detect Tailscale']) await expect(page.getByText(name, { exact: true })).toBeVisible();
    await expect(page.locator('main')).toContainText(/TLS/i);
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByText('Settings saved.').first()).toBeVisible();
    await expect(page.getByText('Unsaved changes')).toHaveCount(0);
    await page.reload();
    await settingsRoute('subagents');
    await expect(page.getByLabel('Maximum concurrent children')).toHaveValue('12');
    await settingsRoute('sessionwiki');
    await expect(page.getByLabel('Archive after (days)')).toHaveValue('30');
    await settingsRoute('guardian');
    await expect(guardian).toHaveAttribute('aria-checked', initialGuardian === 'true' ? 'false' : 'true');
    await expect(page.getByText('Unsaved changes')).toHaveCount(0);
    await guardian.click();
    await expect(page.getByText('Unsaved changes')).toBeVisible();
    await page.getByRole('button', { name: 'Discard' }).click();
    await expect(guardian).toHaveAttribute('aria-checked', initialGuardian === 'true' ? 'false' : 'true');
    await expect(page.getByText('Unsaved changes')).toHaveCount(0);
    await page.evaluate(hash => { location.hash = hash; }, workspaceHash);
    await expect(session).toBeVisible();

    stage('session-move');
    await session.getByRole('button', { name: `Actions for ${title}` }).click();
    await page.getByRole('menuitem', { name: 'Move…', exact: true }).click();
    await expect(page.getByRole('heading', { name: 'Move session' })).toBeVisible();
    const moveTarget = process.env.MJ_BROWSER_MOVE_TARGET;
    expect(moveTarget).toBeTruthy();
    await page.getByLabel('Target').selectOption(moveTarget);
    await page.getByRole('button', { name: 'Prepare move' }).click();
    await page.getByRole('button', { name: 'Confirm move' }).click();
    await expect(page).toHaveURL(/#conversation\//, { timeout: 90_000 });
    await expect(message).toBeEnabled({ timeout: 90_000 });
    await sendPrompt('after move');
    await expect(rows).toContainText('reliability reply: after move', { timeout: 60_000 });
    await page.getByRole('button', { name: 'Back', exact: true }).click();
    await expect(session).toContainText(moveTarget);

    await context.setOffline(true);
    fs.writeFileSync(readyMarker, 'browser offline and ready\n');
    stage('offline-ready');
    // The TUI stop only reaches durable state after the session checkpoints and
    // its target tears down, which takes materially longer than a snapshot
    // round trip and outruns the default 15s on a loaded runner.
    await expect.poll(() => fs.existsSync(changedMarker), { timeout: 60_000 }).toBe(true);
    await context.setOffline(false);

    // A stopped session leaves the dashboard: it belongs to the resume flow,
    // which is where a person can do something about it.
    await expect(session).toHaveCount(0);
    await page.getByRole('link', { name: 'Resume' }).click();
    await expect(page).toHaveURL(/\/resume$/);
    const matchingResumeRow = page.getByRole('button', { name: new RegExp(escapeForRegExp(title)) }).first();
    await expect(matchingResumeRow).toBeVisible();
    await matchingResumeRow.click();
    await page.getByRole('button', { name: 'Resume session' }).click();
    // Resuming lands on the conversation; the dashboard holds the session again.
    await expect(page).toHaveURL(/#conversation\//);
    await page.getByRole('button', { name: 'Back', exact: true }).click();
    await expect(session).toBeVisible();
    // A stop needs the daemon's session manager to have adopted the session,
    // and adoption is asynchronous, so a stop issued moments after a resume can
    // fail with "is not managed". The dialog reports the failure in place, so
    // retrying is what a person would do. The stop itself then checkpoints and
    // tears down a target, which takes materially longer than a snapshot round
    // trip.
    for (let attempt = 0; attempt < 4; attempt += 1) {
      if ((await session.count()) === 0) break;
      await session.getByRole('button', { name: `Actions for ${title}` }).click();
      const suspend = page.getByRole('menuitem', { name: 'Suspend…', exact: true });
      if ((await suspend.count()) === 0) {
        await page.keyboard.press('Escape');
        await page.waitForTimeout(1000);
        continue;
      }
      await suspend.click();
      const dialog = page.getByRole('dialog');
      await dialog.getByRole('button', { name: 'Suspend session' }).click();
      const gone = await session.waitFor({ state: 'detached', timeout: 45_000 }).then(() => true).catch(() => false);
      if (gone) break;
      // A failed attempt leaves the dialog open with the reason; close it.
      await dialog.getByRole('button', { name: 'Cancel' }).click().catch(() => {});
    }
    await expect(session).toHaveCount(0);
    for (const response of transitionResponses) {
      expect(response.url).toMatch(/\/api\/v1\/conversations\/[^/]+$/);
      expect(await response.body).toEqual({ error: 'conversation unavailable while the session is transitioning' });
    }
    expect(expectedTransitionErrors.length).toBe(transitionResponses.length);
    expect(browserErrors).toEqual([]);

    await context.tracing.stop({ path: tracePath });

    // An expired browser cookie must reveal the login form, and explicit
    // logout must do the same after a fresh authenticated session.
    const cookies = await context.cookies();
    const sessionCookie = cookies.find(cookie => cookie.httpOnly);
    expect(sessionCookie).toBeTruthy();
    authPhase.add(page);
    await context.addCookies([{ ...sessionCookie, expires: 1 }]);
    await page.reload();
    await expect(page.getByRole('heading', { name: 'Unlock Mjolnir' })).toBeVisible();
    await codeLogin(page, baseUrl, code);
    authPhase.delete(page);
    expect(browserErrors).toEqual([]);
    await openMenu(page);
    authPhase.add(page);
    await page.getByRole('button', { name: 'Sign out' }).click();
    await expect(page.getByRole('heading', { name: 'Unlock Mjolnir' })).toBeVisible();
    expect(browserErrors).toEqual([]);
    stage(`zero-unexpected-browser-errors; expected login 401s=${expectedAuthErrors.length}; transitioning conversation 409s=${expectedTransitionErrors.length}`);
  } catch (error) {
    if (await page.getByLabel('Code', { exact: true }).isVisible().catch(() => false)) {
      await page.getByLabel('Code', { exact: true }).fill('').catch(() => {});
    }
    await page.screenshot({ path: screenshotPath, fullPage: true }).catch(() => {});
    await context.tracing.stop({ path: tracePath }).catch(() => {});
    throw error;
  } finally {
    await context.close();
  }
});
