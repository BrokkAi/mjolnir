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
  'brokk-mj-client': ['brokk-mj-core'],
  'brokk-mj-worker': ['brokk-mj-core'],
  'brokk-mj-controller': ['brokk-mj-core', 'brokk-mj-client'],
  'brokk-mj-chat': ['brokk-mj-core', 'brokk-mj-client'],
  'brokk-mj-tui': ['brokk-mj-core', 'brokk-mj-client', 'brokk-mj-chat'],
  'brokk-mjolnir': ['brokk-mj-core', 'brokk-mj-client', 'brokk-mj-controller', 'brokk-mj-chat', 'brokk-mj-tui'],
  'brokk-mj-desktop': ['brokk-mj-core', 'brokk-mj-controller'],
  'brokk-mj-voice-worker': [],
};
const packages = new Set(metadata.packages.map(p => p.name));
for (const pkg of metadata.packages) {
  assert(allowed[pkg.name], `Declare the architectural owner of ${pkg.name}`);
  assert.notEqual(path.dirname(pkg.manifest_path), root, 'The root manifest must be workspace-only');
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
