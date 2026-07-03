# Web viewer E2E tests

Browser end-to-end tests for the Mjolnir Web viewer (`src/remote_viewer.html`
and `src/remote_assets/*.js`). They drive a real Chrome against a real
`mj server --allow-spawn`, exercising the surfaces that only exist at runtime:
token sign-in, the command palette, the New task dialog, live transcript and
markdown rendering, session-config comboboxes, the live composer, drafts,
notifications, and stop/cancel.

This is **dev-only tooling**. It is never part of the `mj` build or the shipped
binary — the frontend itself stays no-build. Node and these dev dependencies
are only used to run the tests.

## Requirements

- A built debug binary: run `cargo build` from the repo root first (the harness
  launches `./target/debug/mj`).
- Node 20+.
- Google Chrome or Chromium. The harness auto-detects the usual install
  locations; override with `MJ_TEST_CHROME=/path/to/chrome`.

## Run

```bash
cd tests/web
npm install
npm test
```

The suite enforces a minimum JavaScript line coverage across the viewer's ES
modules (default 75%). Override or disable:

```bash
COVERAGE_MIN=80 npm test   # stricter
COVERAGE_MIN=0 npm test    # measure only, no gate
```

## Artifacts

`artifacts/` (git-ignored) holds screenshots captured during the run and a
`coverage/lcov.info` plus a per-file coverage summary printed to the console.
Coverage is computed directly from Chrome's V8 byte ranges — no third-party
coverage library.

## How it fits together

- `lib/harness.js` — boots the server in an isolated `HOME`, reads its token,
  and launches Chrome via `puppeteer-core` (system Chrome, no bundled
  download).
- `lib/coverage.js` — starts/stops V8 JS coverage and reports line coverage.
- `fixtures/stub-agent.mjs` — a minimal ACP agent over stdio: advertises config
  options, emits rich markdown / thought entries, and honors cancellation.
- `viewer.test.js` — the tests.

CI runs this as the `web-tests` job in `.github/workflows/ci.yml`.
