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
    { id: 'macbook', kind: 'ssh-bare' },
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
