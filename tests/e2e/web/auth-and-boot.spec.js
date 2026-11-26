// Auth and boot: the signed-out probe, the 6-digit code flow, lockout, and
// the signed-in shell on phone and desktop — all under the shipped CSP.

const { test, expect } = require('@playwright/test');
const {
  PHONE,
  DESKTOP,
  startServer,
  installCommonRoutes,
  guardedPage,
  expectNoHorizontalOverflow,
} = require('./spec-helpers');
const { snapshot, liveSession } = require('./fixtures');

let server;

test.beforeAll(async () => {
  server = await startServer();
  installCommonRoutes(server);
});

test.afterAll(async () => {
  await server.close();
});

test.beforeEach(() => {
  server.log.length = 0;
  server.onLogin = null;
  server.setSnapshot(snapshot({ sessions: [liveSession('s1', { title: 'Alpha session' })] }));
});

for (const [name, viewport] of [['phone', PHONE], ['desktop', DESKTOP]]) {
  test(`signed-out boot shows the login view with no overflow (${name})`, async ({ browser }) => {
    const { context, page, guards } = await guardedPage(browser, {
      viewport,
      allowSignedOut401: true,
    });
    try {
      await page.goto(`${server.url}/`);
      await expect(page.getByRole('heading', { name: 'Unlock Mjolnir' })).toBeVisible();
      await expect(page.getByLabel('Code', { exact: true })).toBeVisible();
      // The boot probe is the snapshot request itself; no /auth/session probe.
      expect(server.requests('GET', '/auth/session')).toHaveLength(0);
      expect(server.requests('GET', '/api/v1/options')).toHaveLength(0);
      await expectNoHorizontalOverflow(page);
      await guards.assertClean();
    } finally {
      await context.close();
    }
  });
}

test('wrong code, lockout, then a successful sign-in lands on the dashboard', async ({ browser }) => {
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
    allowLoginRefusals: true,
  });
  try {
    await page.goto(`${server.url}/`);
    const codeInput = page.getByLabel('Code', { exact: true });
    const enter = page.getByRole('button', { name: 'Enter', exact: true });

    // Wrong code: the server refuses with 401 and the form explains.
    await codeInput.fill('000000');
    await enter.click();
    await expect(page.getByRole('alert')).toContainText('That code is not correct');

    // Lockout: 429 carries the server's message.
    server.onLogin = () => ({ status: 429, json: { error: 'Too many attempts; wait a minute.' } });
    await codeInput.fill('111111');
    await enter.click();
    await expect(page.getByRole('alert')).toContainText('Too many attempts; wait a minute.');

    // The right code boots straight into the signed-in shell.
    server.onLogin = null;
    await codeInput.fill('123456');
    await enter.click();
    await expect(page.getByRole('heading', { name: 'Workspaces' })).toBeVisible();
    await expect(page.getByTestId('session-card-s1')).toContainText('Alpha session');
    expect(page.url()).not.toContain('token');
    await expectNoHorizontalOverflow(page);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('signed-in boot renders the dashboard with no overflow on desktop', async ({ browser }) => {
  const { context, page, guards } = await guardedPage(browser, {
    viewport: DESKTOP,
  });
  try {
    // Start with a valid session cookie so the boot never probes signed out.
    const { name, value } = server.grantSession();
    await context.addCookies([{ name, value, url: server.url }]);
    await page.goto(`${server.url}/`);
    await expect(page.getByRole('heading', { name: 'Workspaces' })).toBeVisible();
    // The desktop sidebar navigation is visible at this width.
    await expect(page.getByRole('navigation', { name: 'Mjolnir' })).toBeVisible();
    // Exactly one snapshot fetch and one events stream; no wasted boot probes.
    expect(server.requests('GET', '/api/v1/snapshot')).toHaveLength(1);
    expect(server.requests('GET', '/auth/session')).toHaveLength(0);
    expect(server.requests('GET', '/api/v1/options')).toHaveLength(0);
    await expectNoHorizontalOverflow(page);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});

test('sign out returns to the login view', async ({ browser }) => {
  const { context, page, guards } = await guardedPage(browser, {
    viewport: PHONE,
    allowSignedOut401: true,
  });
  try {
    await page.goto(`${server.url}/`);
    await page.getByLabel('Code', { exact: true }).fill('123456');
    await page.getByRole('button', { name: 'Enter', exact: true }).click();
    await expect(page.getByRole('heading', { name: 'Workspaces' })).toBeVisible();
    await page.getByRole('button', { name: 'Open menu' }).click();
    await page.getByRole('button', { name: 'Sign out' }).click();
    await expect(page.getByRole('heading', { name: 'Unlock Mjolnir' })).toBeVisible();
    expect(server.requests('DELETE', '/auth/session')).toHaveLength(1);
    await guards.assertClean();
  } finally {
    await context.close();
  }
});
