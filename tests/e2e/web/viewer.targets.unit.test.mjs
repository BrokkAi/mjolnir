import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const viewerSource = readFileSync(new URL('../../../mj-server/src/web/viewer.js', import.meta.url), 'utf8');

function sourceBetween(from, to) {
  const start = viewerSource.indexOf(from);
  assert.notEqual(start, -1, `viewer.js is missing ${JSON.stringify(from)}`);
  const end = viewerSource.indexOf(to, start);
  assert.notEqual(end, -1, `viewer.js is missing ${JSON.stringify(to)} after ${from}`);
  return viewerSource.slice(start, end);
}

// Probe-dependent failures and absent commands are both missing. Unchecked
// entries are offered until a verdict arrives.
const snapshot = {
  profiles: [{ id: 'codex' }],
  targets: [
    { id: 'docker', kind: 'local-docker', availability: { status: 'missing', reason: 'Docker is not installed.' } },
    { id: 'macbook', kind: 'ssh-bare', availability: { status: 'missing', reason: 'macbook did not answer.' } },
    { id: 'podman', kind: 'local-podman', availability: { status: 'unknown' } },
  ],
};

function loaded() {
  const context = vm.createContext({ snapshot: structuredClone(snapshot), structuredClone, freshProjectPicker: () => ({}), selectedWorkspaceId: () => 'w1' });
  vm.runInContext(sourceBetween('function isSubagentSession(', '\nfunction liveSessions('), context);
  vm.runInContext(sourceBetween('function availabilityMissing(', '\nfunction targetResourceKind('), context);
  context.setNewTargetSizing = () => {};
  context.delegationPolicy = value => value;
  vm.runInContext(sourceBetween('function subagentChoiceApplies(', '\nfunction targetIsBare('), context);
  vm.runInContext(sourceBetween('function freshDraft() {', '\n}\n') + '\n}\n', context);
  vm.runInContext(sourceBetween('function rememberedDelegationPolicy(', '\n// Mirrors HarnessKind::supports_delegation_tools.'), context);
  vm.runInContext(sourceBetween('function resumeTargetItems(', '\nfunction resumeCard('), context);
  vm.runInContext(sourceBetween('function freshMoveDraft(', '\n}\n') + '\n}\n', context);
  return context;
}

test('a new session preselects the daemon default when it is launchable, else the first launchable target', () => {
  const context = loaded();
  context.snapshot.profiles = [{ id: 'claude' }, { id: 'codex' }];
  vm.runInContext("launchDefault = { profile_id: 'codex', target_id: 'podman' }", context);
  assert.equal(vm.runInContext('freshDraft().targetId', context), 'podman');
  assert.equal(vm.runInContext('freshDraft().profileId', context), 'codex');
  // A default that is a missing runtime, or no longer configured, is not offered.
  vm.runInContext("launchDefault = { profile_id: 'gone', target_id: 'docker' }", context);
  assert.equal(vm.runInContext('freshDraft().targetId', context), 'podman');
  assert.equal(vm.runInContext('freshDraft().profileId', context), 'claude');
  vm.runInContext('launchDefault = null', context);
  assert.equal(vm.runInContext('freshDraft().targetId', context), 'podman');
});

// A minimal element: enough for `el`, `pickerField` and `resumeChoiceField`.
function fakeNode(tag) {
  return {
    tag, children: [], dataset: {}, className: '', textContent: '', value: '', selected: false,
    append(...nodes) { this.children.push(...nodes); },
    setAttribute() {},
    querySelector() { return null; },
  };
}

function rendering() {
  const context = loaded();
  context.document = { createElement: fakeNode };
  context.choiceControl = options => ({ ...fakeNode('field'), choice: options });
  vm.runInContext(sourceBetween('function el(', '\n}\n') + '\n}\n', context);
  vm.runInContext(sourceBetween('function pickerField(', '\n}\n') + '\n}\n', context);
  vm.runInContext(sourceBetween('function resumeChoiceField(', '\n}\n') + '\n}\n', context);
  return context;
}

test('the new picker omits missing targets and still offers an unchecked one', () => {
  const context = rendering();
  const field = vm.runInContext('pickerField("Where to run", "new-target", launchableTargets(snapshot.targets), "podman", () => {})', context);
  assert.deepEqual(Array.from(field.choice.options, option => [option.value, option.description]), [['podman', 'local-podman']]);
});

test('the resume picker omits missing targets and still offers an unchecked one', () => {
  const context = rendering();
  context.session = { id: 's1', compatible_resume_targets: ['docker', 'macbook', 'podman'] };
  const items = vm.runInContext('resumeTargetItems(session)', context);
  assert.deepEqual(Array.from(items, item => item.id), ['podman']);
});

test('a queue-locked retry keeps its retained target even when the last probe failed', () => {
  const context = loaded();
  context.session = {
    id: 's1',
    workspace_id: 'w1',
    profile_id: 'codex',
    target_id: 'podman',
    compatible_resume_targets: ['macbook', 'podman'],
    move_recovery: { destination_target_template_id: 'macbook', queue_admission_started: true },
  };
  const draft = vm.runInContext('freshMoveDraft(session)', context);
  assert.equal(draft.queueLocked, true);
  assert.equal(draft.targetId, 'macbook');
});
