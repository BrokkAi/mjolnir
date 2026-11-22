#!/usr/bin/env node
// Check production dependencies separately from integration-test dependencies.
import { execFileSync } from 'node:child_process';
import assert from 'node:assert/strict';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps', '--locked'], { cwd: root, encoding: 'utf8' }));
const allowed = {
  'brokk-mj-core': [],
  'brokk-mj-build-id': ['brokk-mj-core'],
  'brokk-mj-review': ['brokk-mj-core'],
  'brokk-mj-transcript': ['brokk-mj-core'],
  'brokk-mj-checkpoint': ['brokk-mj-core'],
  'brokk-mj-client': ['brokk-mj-transcript', 'brokk-mj-core'],
  'brokk-mj-worker-runtime': ['brokk-mj-review', 'brokk-mj-transcript', 'brokk-mj-checkpoint', 'brokk-mj-core'],
  'brokk-mj-worker': ['brokk-mj-checkpoint', 'brokk-mj-core', 'brokk-mj-build-id', 'brokk-mj-worker-runtime'],
  'brokk-mj-targets': ['brokk-mj-core'],
  'brokk-mj-accounts': ['brokk-mj-core', 'brokk-mj-client', 'brokk-mj-targets', 'brokk-mj-store'],
  'brokk-mj-import': ['brokk-mj-transcript', 'brokk-mj-checkpoint', 'brokk-mj-core', 'brokk-mj-targets', 'brokk-mj-store', 'brokk-mj-accounts'],
  'brokk-mj-sessionwiki': ['brokk-mj-transcript', 'brokk-mj-checkpoint', 'brokk-mj-core', 'brokk-mj-client', 'brokk-mj-targets', 'brokk-mj-store', 'brokk-mj-import'],
  'brokk-mj-build-cache': ['brokk-mj-core', 'brokk-mj-targets', 'brokk-mj-store'],
  'brokk-mj-worker-staging': ['brokk-mj-core', 'brokk-mj-targets', 'brokk-mj-store', 'brokk-mj-session-types', 'brokk-mj-accounts'],
  'brokk-mj-worker-launch': ['brokk-mj-core', 'brokk-mj-targets', 'brokk-mj-store', 'brokk-mj-session-types', 'brokk-mj-build-cache', 'brokk-mj-worker-staging'],
  'brokk-mj-session-runtime': ['brokk-mj-core', 'brokk-mj-transcript', 'brokk-mj-checkpoint', 'brokk-mj-client', 'brokk-mj-targets', 'brokk-mj-store', 'brokk-mj-session-types', 'brokk-mj-accounts', 'brokk-mj-import', 'brokk-mj-sessionwiki', 'brokk-mj-worker-staging', 'brokk-mj-worker-launch'],
  'brokk-mj-review-host': ['brokk-mj-core', 'brokk-mj-targets', 'brokk-mj-client', 'brokk-mj-review', 'brokk-mj-checkpoint', 'brokk-mj-store', 'brokk-mj-session-runtime', 'brokk-mj-accounts'],
  'brokk-mj-profile-catalog': ['brokk-mj-core', 'brokk-mj-client', 'brokk-mj-targets', 'brokk-mj-store', 'brokk-mj-accounts', 'brokk-mj-worker-staging'],
  'brokk-mj-github-relevance': ['brokk-mj-transcript', 'brokk-mj-core', 'brokk-mj-targets'],
  'brokk-mj-github-watch': ['brokk-mj-core', 'brokk-mj-transcript', 'brokk-mj-store', 'brokk-mj-github-relevance', 'brokk-mj-targets'],
  'brokk-mj-session-types': ['brokk-mj-core', 'brokk-mj-targets'],
  'brokk-mj-store': ['brokk-mj-transcript', 'brokk-mj-core', 'brokk-mj-client', 'brokk-mj-targets'],
  'brokk-mj-controller': ['brokk-mj-worker-launch', 'brokk-mj-session-runtime', 'brokk-mj-profile-catalog', 'brokk-mj-review', 'brokk-mj-transcript', 'brokk-mj-checkpoint', 'brokk-mj-core', 'brokk-mj-client', 'brokk-mj-targets', 'brokk-mj-store', 'brokk-mj-accounts', 'brokk-mj-import', 'brokk-mj-sessionwiki', 'brokk-mj-build-cache', 'brokk-mj-session-types', 'brokk-mj-worker-staging'],
  'brokk-mj-setup': ['brokk-mj-checkpoint', 'brokk-mj-core', 'brokk-mj-client', 'brokk-mj-targets', 'brokk-mj-store', 'brokk-mj-accounts', 'brokk-mj-import', 'brokk-mj-controller'],
  'brokk-mj-server': ['brokk-mj-review', 'brokk-mj-transcript', 'brokk-mj-checkpoint', 'brokk-mj-core', 'brokk-mj-client', 'brokk-mj-accounts', 'brokk-mj-build-cache', 'brokk-mj-import', 'brokk-mj-profile-catalog', 'brokk-mj-session-runtime', 'brokk-mj-session-types', 'brokk-mj-sessionwiki', 'brokk-mj-store', 'brokk-mj-targets', 'brokk-mj-github-relevance'],
  'brokk-mj-daemon': ['brokk-mj-review-host', 'brokk-mj-review', 'brokk-mj-transcript', 'brokk-mj-checkpoint', 'brokk-mj-core', 'brokk-mj-client', 'brokk-mj-controller', 'brokk-mj-server', 'brokk-mj-github-relevance', 'brokk-mj-github-watch', 'brokk-mj-setup'],
  'brokk-mj-chat': ['brokk-mj-transcript', 'brokk-mj-core', 'brokk-mj-client'],
  'brokk-mj-tui': ['brokk-mj-transcript', 'brokk-mj-core', 'brokk-mj-client', 'brokk-mj-chat'],
  'brokk-mjolnir': ['brokk-mj-checkpoint', 'brokk-mj-core', 'brokk-mj-client', 'brokk-mj-controller', 'brokk-mj-daemon', 'brokk-mj-chat', 'brokk-mj-tui', 'brokk-mj-build-id', 'brokk-mj-setup'],
  'brokk-mj-voice-worker': [],
};
const packages = new Set(metadata.packages.map(p => p.name));
const testHookBinaries = new Set(['brokk-mjolnir', 'brokk-mj-worker']);
const removedFeatures = ['test-support', 'test-identity', 'golden', 'github-history'];
// Cargo metadata includes inherited dependency features and member additions.
const unifiedFeatures = {
  axum: [],
  chrono: ['serde'],
  getrandom: ['std'],
  libc: ['extra_traits'],
  'jsonc-parser': ['serde', 'cst'],
  serde_json: ['float_roundtrip'],
  url: ['serde'],
  tokio: ['full', 'test-util'],
  serde: ['derive', 'rc', 'alloc'],
  'tokio-util': ['rt', 'compat', 'io', 'codec'],
  reqwest: ['rustls-no-provider', 'blocking', 'json', 'query', 'stream'],
  image: ['png', 'jpeg', 'webp'],
};
for (const pkg of metadata.packages) {
  assert(allowed[pkg.name], `Declare the architectural owner of ${pkg.name}`);
  assert.notEqual(path.dirname(pkg.manifest_path), root, 'The root manifest must be workspace-only');
  for (const feature of removedFeatures) {
    assert(!Object.hasOwn(pkg.features, feature), `${pkg.name}: remove feature ${feature}`);
  }
  if (Object.hasOwn(pkg.features, 'test-hooks')) {
    assert(testHookBinaries.has(pkg.name), `${pkg.name}: only binary crates may declare test-hooks`);
    assert.deepEqual(pkg.features['test-hooks'], [], `${pkg.name}: test-hooks must not forward features`);
  }
  for (const dep of pkg.dependencies) {
    if (!Object.hasOwn(unifiedFeatures, dep.name)) continue;
    assert.deepEqual(dep.features.toSorted(), unifiedFeatures[dep.name].toSorted(),
      `${pkg.name}: use workspace features for ${dep.name}`);
    assert.equal(dep.uses_default_features, !['reqwest', 'image'].includes(dep.name),
      `${pkg.name}: use workspace defaults for ${dep.name}`);
  }
  for (const dep of pkg.dependencies.filter(d => d.kind === 'dev' && packages.has(d.name))) {
    if (!allowed[pkg.name].includes(dep.name)) {
      assert(dep.path && dep.req === '*', `${pkg.name}: cross-runtime test fixture ${dep.name} must remain an unpublished, versionless path dependency`);
    }
  }
  for (const dep of pkg.dependencies.filter(d => d.kind !== 'dev')) {
    if (packages.has(dep.name)) assert(allowed[pkg.name].includes(dep.name), `${pkg.name} must not depend on ${dep.name}`);
    if (pkg.name === 'brokk-mj-core') assert(!['rusqlite', 'signal-hook'].includes(dep.name), `Controller dependency ${dep.name} leaked into core`);
    assert(!['hel', 'hel-tui'].includes(dep.rename), `Legacy dependency alias in ${pkg.name}`);
  }
}
console.log('Crate ownership boundaries verified.');
