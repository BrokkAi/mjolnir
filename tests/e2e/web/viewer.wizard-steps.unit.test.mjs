import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const viewerSource = readFileSync(new URL('../../../mj-controller/src/web/viewer.js', import.meta.url), 'utf8');

function sourceBetween(from, to) {
  const start = viewerSource.indexOf(from);
  assert.notEqual(start, -1, `viewer.js is missing ${JSON.stringify(from)}`);
  const end = viewerSource.indexOf(to, start);
  assert.notEqual(end, -1, `viewer.js is missing ${JSON.stringify(to)} after ${from}`);
  return viewerSource.slice(start, end);
}

const bare = { id: 'localhost', kind: 'local', requires_project_directory: true, recent_project_directories: ['/work/a'] };
const sized = { id: 'podman', kind: 'local-podman', requires_project_directory: false };

function loaded(snapshot) {
  const context = vm.createContext({ snapshot, newDraft: { projectDirectories: {} } });
  vm.runInContext(sourceBetween('function soleProfileId', '\nfunction targetStatus'), context);
  vm.runInContext(sourceBetween('const NEW_STEPS', '\nlet newDraft'), context);
  vm.runInContext(sourceBetween('function visibleSteps', '\nfunction renderNewForm'), context);
  return context;
}

const stepKeys = context => JSON.parse(vm.runInContext('JSON.stringify(visibleSteps().map(step => step.key))', context));

test('a sole profile skips the Account step, as the terminal wizard does', () => {
  const context = loaded({ profiles: [{ id: 'fake' }], targets: [bare, sized] });
  assert.deepEqual(stepKeys(context), ['target', 'project', 'review']);
});

test('several profiles keep the Account step', () => {
  const context = loaded({ profiles: [{ id: 'a' }, { id: 'b' }], targets: [bare, sized] });
  assert.deepEqual(stepKeys(context), ['profile', 'target', 'project', 'review']);
});

test('a sole usable raw target skips Where to run; a sized target or a second usable one keeps it', () => {
  const profiles = [{ id: 'a' }, { id: 'b' }];
  // The other targets' runtime is missing or their last check failed.
  const only = loaded({ profiles, targets: [bare, { ...sized, runtime_missing: true }, { id: 'mac', requires_project_directory: true, availability: 'unavailable' }] });
  assert.deepEqual(stepKeys(only), ['profile', 'project', 'review']);
  // A target that has not been checked yet still counts as offered.
  const unknown = loaded({ profiles, targets: [bare, { ...sized, availability: 'unknown' }] });
  assert.deepEqual(stepKeys(unknown), ['profile', 'target', 'project', 'review']);
  // A lone container target is sized on that step.
  const container = loaded({ profiles, targets: [sized] });
  assert.deepEqual(stepKeys(container), ['profile', 'target', 'project', 'review']);
});

test('skipped steps still give the draft their only answer', () => {
  const context = loaded({ profiles: [{ id: 'fake' }], targets: [bare, { ...sized, runtime_missing: true }] });
  context.draft = { profileId: 'gone', targetId: 'podman', projectDirectory: '', projectDirectories: {} };
  vm.runInContext('settleSkippedNewSteps(draft)', context);
  assert.equal(context.draft.profileId, 'fake');
  assert.equal(context.draft.targetId, 'localhost');
  assert.equal(context.draft.projectDirectory, '/work/a');
});

function delegationContext() {
  const context = vm.createContext({ snapshot: { profiles: [] } });
  vm.runInContext(sourceBetween('const SUBAGENT_MODES', '\nfunction subagentDiscoveryKey'), context);
  vm.runInContext(sourceBetween('// What the review says about delegation', '\nfunction targetIsBare'), context);
  return context;
}

test('the review names the delegation policy the session will get', () => {
  const context = delegationContext();
  const label = policy => vm.runInContext(`subagentPolicyLabel(${JSON.stringify(policy)})`, context);
  assert.equal(label({ mode: 'native' }), 'Native');
  assert.equal(label({ mode: 'none' }), 'None');
  assert.equal(label({ mode: 'single_model', model: 'haiku', effort: 'low' }), 'Single model: haiku · low');
  assert.equal(label({ mode: 'single_model', model: 'haiku', effort: null }), 'Single model: haiku');
});

test('an unavailable subagent model tells a browser user to change the profile default, not to pass flags', () => {
  const context = delegationContext();
  context.err = Object.assign(new Error('Selected subagent model "fake-model" is unavailable.'), { code: 'subagent_choice_unavailable' });
  const message = vm.runInContext("newSessionFailure(err, 'fake2')", context);
  assert.match(message, /"fake-model" is unavailable/);
  assert.match(message, /Settings → Agent Profiles → fake2 → Sub-agents/);
  assert.doesNotMatch(message, /--subagent/);
  context.other = new Error('boom');
  assert.equal(vm.runInContext("newSessionFailure(other, 'fake2')", context), 'boom');
});
