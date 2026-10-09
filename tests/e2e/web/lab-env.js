// The environment the provisioned browser lab hands to Playwright.
const { createHash } = require('node:crypto');
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

const INTERNED_FIELDS = [
  'available_commands',
  'capabilities',
  'config_options',
  'compatible_resume_targets',
  'incompatible_resume_targets',
];

const SUMMARY_FIELDS = new Set([
  'id', 'workspace_id', 'bundle_id', 'title', 'subagent_parent_id',
  'subagent_session_ids', 'publication_state', 'profile_id',
  'target_id', 'state', 'created_at', 'updated_at', 'has_error',
  'configuration_issue', 'launch_error', 'storage_problem', 'project_label',
  'display_location', 'lifecycle', 'transitioning', 'last_activity_at_ms',
  'last_message_at_ms', 'capacity_retry', 'retry_assessment_pending',
  'quota_recovery', 'operation', 'move_recovery', 'chat_phase', 'is_idle',
  'activity_details', 'activity', 'capabilities',
]);

function cloneJson(value) {
  return JSON.parse(JSON.stringify(value));
}

function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
  if (value && typeof value === 'object') {
    return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

function emptyInternedValue(value) {
  return value == null
    || (Array.isArray(value) && value.length === 0)
    || (typeof value === 'object' && !Array.isArray(value) && Object.keys(value).length === 0);
}

function viewerWireSnapshot(value) {
  const snapshot = cloneJson(value);
  const interned = {};
  snapshot.sessions = (snapshot.sessions || []).map(source => {
    const row = { ...source };
    if (row.detail === false) {
      const promptCount = row.queued_prompts?.length ?? row.queued_prompt_count ?? 0;
      const elicitationCount = row.pending_elicitations?.length ?? row.pending_elicitation_count ?? 0;
      for (const key of Object.keys(row)) if (!SUMMARY_FIELDS.has(key)) delete row[key];
      row.detail = false;
      row.queued_prompt_count = promptCount;
      row.pending_elicitation_count = elicitationCount;
    }
    if (Array.isArray(row.native_subagents)) {
      row.native_subagents = row.native_subagents.map(({ task, ...agent }) => agent);
    }
    for (const field of INTERNED_FIELDS) {
      if (!Object.hasOwn(row, field) || emptyInternedValue(row[field])) continue;
      const key = createHash('sha256').update(canonicalJson(row[field])).digest('hex').slice(0, 16);
      interned[key] = row[field];
      row[`${field}_ref`] = key;
      delete row[field];
    }
    return row;
  });
  snapshot.cursor = snapshot.cursor || {
    incarnation: 'fixture',
    sequence: Number.isInteger(snapshot.revision) ? snapshot.revision : 1,
  };
  snapshot.interned = interned;
  return snapshot;
}

function viewerDetailResponse(snapshot, sessionId) {
  const source = (snapshot.sessions || []).find(row => row.id === sessionId);
  if (!source) return null;
  const row = { ...source };
  delete row.detail;
  delete row.queued_prompt_count;
  delete row.pending_elicitation_count;
  if (Array.isArray(row.native_subagents)) {
    row.native_subagents = row.native_subagents.map(({ task, ...agent }) => agent);
  }
  for (const field of INTERNED_FIELDS) {
    const refField = `${field}_ref`;
    if (!Object.hasOwn(row, refField)) continue;
    const key = row[refField];
    if (!snapshot.interned || !Object.hasOwn(snapshot.interned, key)) {
      throw new Error(`fixture is missing interned value ${key}`);
    }
    row[field] = snapshot.interned[key];
    delete row[refField];
  }
  return { revision: snapshot.revision, row };
}

function viewerDelta(previous, snapshot, knownInternedKeys) {
  const wire = viewerWireSnapshot(snapshot);
  const known = new Set(knownInternedKeys || Object.keys(previous.interned || {}));
  const previousMetadata = { ...previous };
  delete previousMetadata.sessions;
  delete previousMetadata.cursor;
  delete previousMetadata.interned;
  const previousRows = new Map(previous.sessions.map(row => [row.id, row]));
  const sessions = [];
  const requiredKeys = new Set();
  for (const row of wire.sessions) {
    if (JSON.stringify(previousRows.get(row.id)) === JSON.stringify(row)) continue;
    sessions.push([row.id, row]);
    for (const field of INTERNED_FIELDS) {
      const key = row[`${field}_ref`];
      if (key && !known.has(key)) requiredKeys.add(key);
    }
  }
  const currentIds = new Set(wire.sessions.map(row => row.id));
  for (const row of previous.sessions) if (!currentIds.has(row.id)) sessions.push([row.id, null]);
  const interned = Object.fromEntries(Object.entries(wire.interned)
    .filter(([key]) => requiredKeys.has(key)));
  for (const key of Object.keys(wire.interned)) known.add(key);
  const currentMetadata = { ...wire };
  delete currentMetadata.sessions;
  delete currentMetadata.cursor;
  delete currentMetadata.interned;
  const alwaysSent = new Set(['revision', 'server_time_ms', 'generated_at']);
  const metadata = Object.fromEntries(Object.entries(currentMetadata).filter(([key, value]) =>
    alwaysSent.has(key)
      || !Object.hasOwn(previousMetadata, key)
      || canonicalJson(previousMetadata[key]) !== canonicalJson(value)));
  for (const key of Object.keys(previousMetadata)) {
    if (Object.hasOwn(currentMetadata, key)) continue;
    if (key === 'server_version') metadata[key] = '';
    else if (['workspaces', 'capacity', 'launch_failures'].includes(key)) metadata[key] = [];
    else metadata[key] = null;
  }
  return {
    wire,
    knownInternedKeys: known,
    frame: { kind: 'delta', from: previous.cursor, cursor: wire.cursor, metadata, sessions, interned },
  };
}

async function dispatchRuntimeFrame(page, frame) {
  await page.evaluate(value => {
    const currentSource = () => [...(window.fixtureEventSources || [])].reverse()
      .find(candidate => !candidate.closed);
    let source = currentSource();
    if (!source) {
      window.dispatchEvent(new Event('online'));
      source = currentSource();
    }
    if (!source) throw new Error('the fixture could not open a runtime event source');
    source.dispatchEvent(new MessageEvent('runtime', { data: JSON.stringify(value) }));
  }, frame);
}

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
  viewerWireSnapshot,
  viewerDetailResponse,
  viewerDelta,
  dispatchRuntimeFrame,
};
