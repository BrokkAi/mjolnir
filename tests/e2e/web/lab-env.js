// The environment the provisioned browser lab hands to Playwright.
//
// `tests/e2e/browser_lab.py` starts a real daemon, a real TUI and a real
// viewer, then passes their addresses and markers through these variables.
// Specs that need that lab read it here so a clean checkout, a partly
// configured CI job and a real lab run are told apart in one place.

// The specs that cannot run without that lab. Playwright's `lab` project runs
// exactly these; its `deterministic` project runs every other spec.
const LAB_SPECS = ['reliability.spec.js'];

const LAB_VARIABLES = [
  'MJ_BROWSER_BASE_URL',
  'MJ_BROWSER_CODE',
  'MJ_BROWSER_QR_URL',
  'MJ_BROWSER_TITLE',
  'MJ_BROWSER_PROJECT_DIRECTORY',
  'MJ_BROWSER_READY_MARKER',
  'MJ_TUI_CHANGED_MARKER',
  'MJ_BROWSER_TRACE',
  'MJ_BROWSER_SCREENSHOT',
];

const LAB_ABSENT_REASON =
  'the provisioned browser lab is not configured; run tests/e2e/run-browser-reliability.sh --seed N <mj binary> for this case';

/// Report whether the lab environment is fully present, fully absent, or —
/// the case worth shouting about — partly populated by a misconfiguration.
function inspectLabEnvironment(environment = process.env) {
  const missing = LAB_VARIABLES.filter(name => !environment[name]);
  if (missing.length === LAB_VARIABLES.length) return { state: 'absent', missing };
  if (missing.length > 0) return { state: 'partial', missing };
  return { state: 'complete', missing };
}

/// Return every lab value, or throw naming the variables that are missing.
function requireLabEnvironment(environment = process.env) {
  const status = inspectLabEnvironment(environment);
  if (status.state !== 'complete') throw new Error(`missing ${status.missing.join(', ')}`);
  return Object.fromEntries(LAB_VARIABLES.map(name => [name, environment[name]]));
}

module.exports = {
  LAB_SPECS,
  LAB_VARIABLES,
  LAB_ABSENT_REASON,
  inspectLabEnvironment,
  requireLabEnvironment,
};
