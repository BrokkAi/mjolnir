const { defineConfig } = require('@playwright/test');

const { LAB_SPECS } = require('./lab-env');

module.exports = defineConfig({
  testDir: '.',
  fullyParallel: false,
  workers: 1,
  timeout: 150_000,
  expect: { timeout: 15_000 },
  reporter: [['line']],
  projects: [
    // The default selection: no daemon, no lab, no network.
    { name: 'deterministic', testMatch: '*.spec.js', testIgnore: LAB_SPECS },
    // The live TUI/browser convergence case, selected by
    // `tests/e2e/run-browser-reliability.sh`.
    { name: 'lab', testMatch: LAB_SPECS },
  ],
  use: {
    browserName: process.env.MJ_BROWSER_ENGINE || 'chromium',
    headless: true,
    ignoreHTTPSErrors: true,
    trace: 'off',
    // The lab serves the viewer over HTTPS with a self-signed certificate.
    // `ignoreHTTPSErrors` covers page and API requests, but Chromium fetches a
    // service worker script outside that path and refuses to register one over
    // a certificate it does not trust, so the launch flag is the only way to
    // exercise the offline shell against the lab's own certificate.
    launchOptions: { args: process.env.MJ_BROWSER_ENGINE === 'firefox' ? [] : ['--ignore-certificate-errors'] },
  },
});
