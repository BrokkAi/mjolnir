// The two Playwright selections must not drift apart: everything under
// tests/e2e/web runs either without a lab (`deterministic`, what `npm test`
// runs) or only with one (`lab`, what run-browser-reliability.sh runs).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const { LAB_SPECS, LAB_VARIABLES, inspectLabEnvironment, requireLabEnvironment } = require('./lab-env.js');

/// The files Playwright would actually run for a project, with no lab
/// environment in sight.
function listedFiles(project) {
  const environment = { ...process.env };
  for (const name of LAB_VARIABLES) delete environment[name];
  const listing = spawnSync(
    path.join(here, 'node_modules/.bin/playwright'),
    ['test', '--list', '--project', project, '--reporter', 'json'],
    { cwd: here, env: environment, encoding: 'utf8' },
  );
  assert.equal(listing.status, 0, `listing the ${project} project failed: ${listing.stderr}`);
  const report = JSON.parse(listing.stdout);
  const files = new Set();
  const walk = suite => {
    if (suite.file) files.add(path.basename(suite.file));
    for (const child of suite.suites ?? []) walk(child);
    for (const spec of suite.specs ?? []) files.add(path.basename(spec.file));
  };
  for (const suite of report.suites ?? []) walk(suite);
  return files;
}

test('every browser spec belongs to exactly one project', () => {
  const onDisk = fs.readdirSync(here).filter(name => name.endsWith('.spec.js'));
  const lab = listedFiles('lab');
  const deterministic = listedFiles('deterministic');

  assert.deepEqual([...lab].sort(), [...LAB_SPECS].sort());
  assert.deepEqual(
    [...deterministic].sort(),
    onDisk.filter(name => !LAB_SPECS.includes(name)).sort(),
  );
  assert.deepEqual(
    [...new Set([...lab, ...deterministic])].sort(),
    onDisk.slice().sort(),
    'a spec file is in neither project',
  );
  for (const name of lab) assert.ok(!deterministic.has(name), `${name} is in both projects`);
});

test('a partly populated lab environment names the missing variables', () => {
  assert.equal(inspectLabEnvironment({}).state, 'absent');
  const partial = { MJ_BROWSER_BASE_URL: 'https://127.0.0.1:1', MJ_BROWSER_CODE: '000000' };
  const status = inspectLabEnvironment(partial);
  assert.equal(status.state, 'partial');
  assert.throws(() => requireLabEnvironment(partial), /missing MJ_BROWSER_QR_URL/);

  const complete = Object.fromEntries(LAB_VARIABLES.map(name => [name, 'value']));
  assert.equal(inspectLabEnvironment(complete).state, 'complete');
  assert.deepEqual(requireLabEnvironment(complete), complete);
});
