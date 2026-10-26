// BuildKit scopes an ARG declared before the first FROM to FROM lines only;
// a stage that uses the variable later must redeclare it with a bare
// `ARG NAME` or every reference expands empty and the image build fails at
// run time. Nothing in CI builds the agent-dev image, so this check is the
// only guard short of publish-agent-dev-image.yml going red.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import test from 'node:test';

const root = resolve(import.meta.dirname, '..');
const containerfile = readFileSync(join(root, 'containers/Containerfile.agent-dev'), 'utf8');

// Hard-won: 93cd85792: a global ARG used in a RUN expanded empty because no
// stage redeclared it, so the go.dev tarball URL 404'd and every agent-dev
// image build failed before Go was installed.
test('every global ARG used inside a build stage is redeclared in that stage', () => {
  const lines = containerfile.split('\n');
  const globalArgs = new Set();
  for (const line of lines) {
    if (line.startsWith('FROM ')) break;
    const match = line.match(/^ARG\s+([A-Z][A-Z0-9_]*)/);
    if (match) globalArgs.add(match[1]);
  }

  const violations = [];
  const declared = new Set();
  let stage = 'global scope';
  for (const [index, line] of lines.entries()) {
    if (line.startsWith('FROM ')) {
      stage = line;
      declared.clear();
      continue;
    }
    if (line.startsWith('#')) continue;
    const arg = line.match(/^ARG\s+([A-Z][A-Z0-9_]*)/);
    if (arg) {
      declared.add(arg[1]);
      continue;
    }
    // ENV puts names in scope for later instructions, like MJ_WORKER_BUILD_ID
    // for the trailing LABEL.
    const env = line.match(/^ENV\s+(.*)$/);
    if (env) {
      for (const name of env[1].matchAll(/([A-Z][A-Z0-9_]*)=/g)) declared.add(name[1]);
      continue;
    }
    for (const match of line.matchAll(/\$\{([A-Z][A-Z0-9_]*)\}|\$\(([A-Z][A-Z0-9_]*)\)/g)) {
      const name = match[1] ?? match[2];
      if (globalArgs.has(name) && !declared.has(name)) {
        violations.push(`line ${index + 1}: ${stage} uses \${${name}} with no in-stage ARG`);
      }
    }
  }
  assert.deepEqual(violations, []);
});
