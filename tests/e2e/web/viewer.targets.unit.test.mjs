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

// A host with no Docker: `docker` is permanently unavailable here and must not
// be offered; `macbook` did not answer its last check, which is transient, so
// it stays listed.
const snapshot = {
  profiles: [{ id: 'codex' }],
  targets: [
    { id: 'docker', kind: 'local-docker', runtime_missing: true },
    { id: 'macbook', kind: 'ssh-bare', availability: 'unavailable', unavailable_reason: 'the host "macbook" did not answer its last check' },
    { id: 'podman', kind: 'local-podman' },
  ],
};

function loaded() {
  const context = vm.createContext({ snapshot, structuredClone, freshProjectPicker: () => ({}), selectedWorkspaceId: () => 'w1' });
  vm.runInContext(sourceBetween('// A target whose runtime', '\nfunction freshDraft() {'), context);
  vm.runInContext(sourceBetween('function freshDraft() {', '\n}\n') + '\n}\n', context);
  vm.runInContext(sourceBetween('function resumeTargetItems(', '\nfunction resumeCard('), context);
  vm.runInContext(sourceBetween('function freshMoveDraft(', '\n}\n') + '\n}\n', context);
  return context;
}

test('the target pickers leave out a runtime missing on this host and keep an unresponsive host', () => {
  const context = loaded();
  assert.deepEqual(
    vm.runInContext('launchableTargets(snapshot.targets).map(t => t.id)', context),
    ['macbook', 'podman'],
  );
});

test('a new session does not default to a missing runtime', () => {
  const context = loaded();
  assert.equal(vm.runInContext('freshDraft().targetId', context), 'macbook');
});

test('resume and move offer the same targets', () => {
  const context = loaded();
  context.session = {
    id: 's1',
    profile_id: 'codex',
    target_id: 'docker',
    compatible_resume_targets: ['docker', 'macbook', 'podman'],
  };
  assert.deepEqual(
    vm.runInContext('resumeTargetItems(session).map(t => t.id)', context),
    ['macbook', 'podman'],
  );
  assert.equal(vm.runInContext('freshMoveDraft(session).targetId', context), 'macbook');
});

test('a new session preselects the daemon default when it is launchable, else the first launchable target', () => {
  const context = loaded();
  context.snapshot.profiles = [{ id: 'claude' }, { id: 'codex' }];
  vm.runInContext("launchDefault = { profile_id: 'codex', target_id: 'podman' }", context);
  assert.equal(vm.runInContext('freshDraft().targetId', context), 'podman');
  assert.equal(vm.runInContext('freshDraft().profileId', context), 'codex');
  // A default that is a missing runtime, or no longer configured, is not offered.
  vm.runInContext("launchDefault = { profile_id: 'gone', target_id: 'docker' }", context);
  assert.equal(vm.runInContext('freshDraft().targetId', context), 'macbook');
  assert.equal(vm.runInContext('freshDraft().profileId', context), 'claude');
  vm.runInContext('launchDefault = null', context);
  assert.equal(vm.runInContext('freshDraft().targetId', context), 'macbook');
});

test('move preselects the session current target when it is offered, not the first', () => {
  const context = loaded();
  context.session = {
    id: 's1',
    profile_id: 'codex',
    target_id: 'podman',
    compatible_resume_targets: ['macbook', 'podman'],
  };
  assert.equal(vm.runInContext('freshMoveDraft(session).targetId', context), 'podman');
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

test('a host that did not answer is listed with a status and stays selectable', () => {
  const context = rendering();
  assert.equal(vm.runInContext('targetStatus(snapshot.targets[1])', context), 'did not answer its last check');
  // No reading yet, a ready host and a missing runtime say nothing here.
  assert.equal(vm.runInContext("targetStatus({ id: 'a', availability: 'unknown' })", context), '');
  assert.equal(vm.runInContext("targetStatus({ id: 'a', availability: 'ready' })", context), '');
  assert.equal(vm.runInContext('targetStatus(snapshot.targets[2])', context), '');

  // New and Move: the picker option carries the status and is not disabled.
  const field = vm.runInContext('pickerField("Where to run", "new-target", launchableTargets(snapshot.targets), "macbook", () => {})', context);
  const options = field.choice.options;
  assert.deepEqual(options.map(option => option.value), ['macbook', 'podman']);
  assert.equal(options[0].description, 'ssh-bare · did not answer its last check');
  assert.equal(options[1].description, 'local-podman');
  assert.ok(options.every(option => !option.disabled));
});

test('resume shows the status beside the target and keeps it selectable', () => {
  const context = rendering();
  context.session = { id: 's1', compatible_resume_targets: ['docker', 'macbook', 'podman'] };
  const items = vm.runInContext('resumeTargetItems(session)', context);
  assert.deepEqual(items.map(item => [item.id, item.status]), [['macbook', 'did not answer its last check'], ['podman', '']]);
  context.items = items;
  const field = vm.runInContext('resumeChoiceField("Target", "resume-target-s1", items, "podman", () => {})', context);
  const select = field.children.find(child => child.tag === 'select');
  const options = select.children.filter(child => child.tag === 'option' && child.value);
  assert.deepEqual(options.map(option => option.textContent), ['macbook (did not answer its last check)', 'podman']);
  assert.ok(options.every(option => !option.disabled));
});

test('the Targets page leaves out a default candidate whose runtime is missing and keeps a configured one', () => {
  const context = vm.createContext({
    snapshot: {
      targets: [
        { id: 'docker', kind: 'local-docker', runtime_missing: true, default_candidate: true },
        { id: 'sandbox', kind: 'local-docker', runtime_missing: true },
        { id: 'podman', kind: 'local-podman', default_candidate: true },
        { id: 'localhost', kind: 'local-bare', default_candidate: true },
      ],
    },
  });
  vm.runInContext(sourceBetween('function listedTargetIds(', '\nfunction renderTargets()'), context);
  assert.deepEqual(
    vm.runInContext("listedTargetIds({ target_ids: ['docker', 'localhost', 'podman', 'sandbox'] }).join(',')", context),
    'localhost,podman,sandbox',
  );
});
