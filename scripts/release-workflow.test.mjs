import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync, readdirSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const root = resolve(import.meta.dirname, '..');
// Exercise the shell shipped in the workflow, so fixtures verify its observable
// behavior rather than maintaining a second copy of the publication algorithm.
function runStep(workflow, name) {
  const rest = workflow.split(`      - name: ${name}\n`)[1];
  assert.ok(rest, `missing step ${name}`);
  const body = rest.split('        run: |\n')[1];
  assert.ok(body, `missing shell for ${name}`);
  const lines = [];
  for (const line of body.split('\n')) {
    if (line && !line.startsWith('          ')) break;
    lines.push(line.slice(10));
  }
  return lines.join('\n');
}
function fixture(t) {
  const dir = mkdtempSync(join(tmpdir(), 'mj-release-workflow-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

const release = readFileSync(join(root, '.github/workflows/release.yml'), 'utf8');
for (const target of ['x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'universal-apple-darwin']) {
  test(`archive assembly preserves binaries, modes, notices and checksum for ${target}`, t => {
    const dir = fixture(t);
    const binaries = ['mj', 'mj-desktop', 'mj-voice-worker'];
    const mac = target === 'universal-apple-darwin';
    if (mac) binaries.push('mj-worker');
    const native = join(dir, mac ? 'target/universal-apple-darwin/release' : 'native');
    mkdirSync(native, { recursive: true });
    for (const binary of binaries) writeFileSync(join(native, binary), binary);
    for (const arch of ['x86_64', 'aarch64']) {
      const worker = `mj-worker-${arch}-unknown-linux-musl`;
      mkdirSync(join(dir, 'workers', worker), { recursive: true });
      writeFileSync(join(dir, 'workers', worker, 'mj-worker'), worker);
      binaries.push(worker);
    }
    mkdirSync(join(dir, 'licenses/native'), { recursive: true });
    for (const file of ['README.md', 'LICENSE', 'licenses/SOURCE.md', 'licenses/OFL-1.1.md', 'licenses/native/notice']) {
      writeFileSync(join(dir, file), file);
    }
    mkdirSync(join(dir, 'release-notices'));
    for (const file of ['THIRD_PARTY_LICENSES.html', 'SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt']) {
      writeFileSync(join(dir, 'release-notices', file), `generated for this tag: ${file}`);
    }
    const job = mac ? 'package-macos' : `package-${target}`;
    const block = release.split(`  ${job}:\n`)[1].split(/\n  [\w-]+:\n/)[0];
    const result = spawnSync('bash', ['-euo', 'pipefail', '-c', runStep(block, 'Package archive')], {
      cwd: dir, encoding: 'utf8', env: { ...process.env, GITHUB_REF_NAME: 'v1.2.3' },
    });
    assert.equal(result.status, 0, result.stderr);
    const name = `brokk-mjolnir-v1.2.3-${target}`;
    for (const binary of binaries) {
      assert.equal(readFileSync(join(dir, name, binary), 'utf8'), binary);
      assert.equal(statSync(join(dir, name, binary)).mode & 0o777, 0o755);
    }
    assert.ok(readdirSync(join(dir, name, 'licenses/native')).includes('notice'));
    for (const file of ['THIRD_PARTY_LICENSES.html', 'SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt']) {
      assert.equal(readFileSync(join(dir, name, 'licenses', file), 'utf8'), `generated for this tag: ${file}`);
    }
    const checksum = spawnSync('shasum', ['-a', '256', '-c', `${name}.tar.gz.sha256`], { cwd: dir, encoding: 'utf8' });
    assert.equal(checksum.status, 0, checksum.stderr);
  });
}

const npmWorkflow = readFileSync(join(root, '.github/workflows/publish-npm.yml'), 'utf8');
for (const fail of ['', 'linux-x64-gnu']) {
  test(`npm uploads run concurrently and ${fail ? 'a failure blocks the wrapper' : 'all platforms precede the wrapper'}`, t => {
    const dir = fixture(t);
    mkdirSync(join(dir, 'bin'));
    writeFileSync(join(dir, 'Cargo.toml'), '[workspace.package]\nversion = "1.2.3"\n');
    writeFileSync(join(dir, 'bin/npm'), `#!/bin/bash
set -eu
name="$(basename "$2" .tgz)"
touch "$FIXTURE/$name.started"
if [[ "$name" != brokkai-mjolnir-1.2.3 ]]; then
  # A sequential upload loop cannot pass this barrier.
  for ((attempt=0; attempt<100; attempt++)); do
    count=$(find "$FIXTURE" -name '*.started' | wc -l)
    ((count >= 3)) && break
    sleep 0.02
  done
  ((count >= 3)) || exit 22
  [[ -z "$FAIL_PLATFORM" || "$name" != *"$FAIL_PLATFORM"* ]] || exit 23
else
  for platform in darwin-universal linux-x64-gnu linux-arm64-gnu; do
    test -f "$FIXTURE/brokkai-mjolnir-$platform-1.2.3.ready"
  done
fi
touch "$FIXTURE/$name.ready"
`, { mode: 0o755 });
    writeFileSync(join(dir, 'bin/curl'), `#!/bin/bash
url="\${@: -1}"
package="\${url#*%2F}"
package="\${package%/*}"
test -f "$FIXTURE/brokkai-$package-1.2.3.ready"
`, { mode: 0o755 });
    const result = spawnSync('bash', ['-euo', 'pipefail', '-c', runStep(npmWorkflow, 'Publish missing platform packages before the root wrapper')], {
      cwd: dir, encoding: 'utf8', timeout: 10000,
      env: { ...process.env, PATH: `${join(dir, 'bin')}:${process.env.PATH}`, FIXTURE: dir, FAIL_PLATFORM: fail },
    });
    assert.equal(result.error, undefined, result.error?.message);
    if (fail) assert.notEqual(result.status, 0, result.stdout + result.stderr);
    else assert.equal(result.status, 0, result.stdout + result.stderr);
    const files = readdirSync(dir);
    assert.equal(files.includes('brokkai-mjolnir-1.2.3.started'), !fail);
    // Both other uploads complete even when a sibling fails.
    assert.ok(files.includes('brokkai-mjolnir-darwin-universal-1.2.3.ready'));
    assert.ok(files.includes('brokkai-mjolnir-linux-arm64-gnu-1.2.3.ready'));
  });
}
