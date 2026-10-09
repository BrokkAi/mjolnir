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
for (const target of ['x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'x86_64-apple-darwin', 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc']) {
  test(`archive assembly preserves binaries, modes, notices and checksum for ${target}`, t => {
    const dir = fixture(t);
    const mac = target.endsWith('-apple-darwin');
    const windows = target === 'x86_64-pc-windows-msvc';
    const binaries = windows ? ['mj.exe', 'mj-desktop.exe'] : ['mj', 'mj-desktop', 'mj-voice-worker'];
    if (mac) binaries.push('mj-worker');
    const native = join(dir, 'native');
    mkdirSync(native, { recursive: true });
    for (const binary of binaries) writeFileSync(join(native, binary), binary);
    // A Windows controller drives Linux containers and SSH hosts only.
    const workerTriples = ['x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl'];
    if (!windows) workerTriples.push('x86_64-apple-darwin', 'aarch64-apple-darwin');
    for (const triple of workerTriples) {
      const worker = `mj-worker-${triple}`;
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
    const job = `package-${target}`;
    const block = release.split(`  ${job}:\n`)[1].split(/\n  [\w-]+:\n/)[0];
    const result = spawnSync('bash', ['-euo', 'pipefail', '-c', runStep(block, 'Package archive')], {
      cwd: dir, encoding: 'utf8', env: { ...process.env, GITHUB_REF_NAME: 'v1.2.3' },
    });
    assert.equal(result.status, 0, result.stderr);
    const name = `brokk-mjolnir-v1.2.3-${target}`;
    for (const binary of binaries) {
      assert.equal(readFileSync(join(dir, name, binary), 'utf8'), binary);
      if (!windows) assert.equal(statSync(join(dir, name, binary)).mode & 0o777, 0o755);
    }
    if (windows) {
      // The updater and installer expect one top-level directory holding the binaries.
      const listing = spawnSync('unzip', ['-Z1', `${name}.zip`], { cwd: dir, encoding: 'utf8' });
      assert.equal(listing.status, 0, listing.stderr);
      const entries = listing.stdout.split('\n').filter(Boolean);
      assert.ok(entries.every(entry => entry.startsWith(`${name}/`)), listing.stdout);
      for (const binary of binaries) assert.ok(entries.includes(`${name}/${binary}`), binary);
    }
    assert.ok(readdirSync(join(dir, name, 'licenses/native')).includes('notice'));
    for (const file of ['THIRD_PARTY_LICENSES.html', 'SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt']) {
      assert.equal(readFileSync(join(dir, name, 'licenses', file), 'utf8'), `generated for this tag: ${file}`);
    }
    const checksum = spawnSync('shasum', ['-a', '256', '-c', `${name}.${windows ? 'zip' : 'tar.gz'}.sha256`], { cwd: dir, encoding: 'utf8' });
    assert.equal(checksum.status, 0, checksum.stderr);
  });
}

const npmWorkflow = readFileSync(join(root, '.github/workflows/publish-npm.yml'), 'utf8');

const publishImage = readFileSync(join(root, '.github/workflows/publish-agent-dev-image.yml'), 'utf8');

// The release's container sessions default to agent-dev:<version>, so the
// tagged workflow must publish that image and must not move the floating
// :latest tag that development builds use.
test('a release publishes its version image and leaves :latest to master', () => {
  const job = release.split('  publish-agent-dev-image:\n')[1].split(/\n  [\w-]+:\n/)[0];
  assert.match(job, /uses: \.\/\.github\/workflows\/publish-agent-dev-image\.yml/);
  assert.match(job, /extra_tags: \$\{\{ needs\.verify-version\.outputs\.version \}\}/);
  assert.match(job, /publish_latest: 'false'/);
  const releaseJob = release.split('  release:\n')[1].split('    steps:')[0];
  assert.match(releaseJob, /needs: \[[^\]]*publish-agent-dev-image\]/);
});

for (const { label, publishLatest, extraTags, expectedTags, absentTags } of [
  {
    label: 'master keeps moving :latest',
    publishLatest: 'true',
    extraTags: '',
    expectedTags: ['latest', 'sha-abcdef1'],
    absentTags: [],
  },
  {
    label: 'a release tags its version and leaves :latest alone',
    publishLatest: 'false',
    extraTags: '2.37.0',
    expectedTags: ['sha-abcdef1', '2.37.0'],
    absentTags: ['latest'],
  },
]) {
  test(`agent-dev manifest tags when ${label}`, t => {
    const dir = fixture(t);
    // The per-arch jobs upload bare digest-file names; the merge step prefixes
    // each with `sha256:`.
    writeFileSync(join(dir, 'a1'.repeat(32)), '');
    writeFileSync(join(dir, 'b2'.repeat(32)), '');
    mkdirSync(join(dir, 'bin'));
    writeFileSync(join(dir, 'bin/docker'), `#!/bin/bash
set -euo pipefail
printf '%s\\n' "$*" >> "$FIXTURE/args"
case "$*" in
  *imagetools\\ inspect*) echo 'sha256:deadbeef' ;;
esac
`, { mode: 0o755 });
    const output = join(dir, 'github-output');
    writeFileSync(output, '');
    const result = spawnSync('bash', ['-euo', 'pipefail', '-c', runStep(publishImage, 'Create multi-arch manifest')], {
      cwd: dir,
      encoding: 'utf8',
      env: {
        ...process.env,
        PATH: `${join(dir, 'bin')}:${process.env.PATH}`,
        FIXTURE: dir,
        IMAGE: 'ghcr.io/test/mjolnir/agent-dev',
        EXTRA_TAGS: extraTags,
        PUBLISH_LATEST: publishLatest,
        GITHUB_SHA: 'abcdef1234567890',
        GITHUB_OUTPUT: output,
      },
    });
    assert.equal(result.status, 0, result.stderr);
    const create = readFileSync(join(dir, 'args'), 'utf8')
      .split('\n')
      .find(line => line.includes('imagetools create'));
    for (const tag of expectedTags) {
      assert.ok(create.includes(`--tag ghcr.io/test/mjolnir/agent-dev:${tag}`), create);
    }
    for (const tag of absentTags) {
      assert.ok(!create.includes(`--tag ghcr.io/test/mjolnir/agent-dev:${tag}`), create);
    }
    assert.match(readFileSync(output, 'utf8'), /digest=sha256:deadbeef/);
  });
}

test('npm downloads assets when the release lookup omits its embedded asset list', t => {
  const dir = fixture(t);
  const work = join(dir, 'work');
  mkdirSync(work);
  mkdirSync(join(dir, 'bin'));
  const assets = [];
  for (const target of ['x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'x86_64-apple-darwin', 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc']) {
    const archive = target.endsWith('-windows-msvc') ? 'zip' : 'tar.gz';
    for (const suffix of [archive, `${archive}.sha256`]) {
      const name = `brokk-mjolnir-v1.2.3-${target}.${suffix}`;
      const id = assets.length + 1;
      const data = suffix === archive ? Buffer.alloc(70 * 1024, id) : Buffer.from(`${id}  ${name}\n`);
      writeFileSync(join(dir, String(id)), data);
      assets.push({ id, name, size: data.length });
    }
  }
  // The release also carries the agent-dev image pointer alongside the archives.
  const pointerData = Buffer.from('ghcr.io/test/mjolnir/agent-dev:v1.2.3\n');
  const pointerId = assets.length + 1;
  writeFileSync(join(dir, String(pointerId)), pointerData);
  assets.push({ id: pointerId, name: 'agent-dev-image.txt', size: pointerData.length });
  writeFileSync(join(dir, 'assets.json'), JSON.stringify(assets));
  writeFileSync(join(dir, 'bin/gh'), `#!/bin/bash
set -euo pipefail
case "$*" in
  *'/tags/'*) echo 123 ;;
  *'assets?per_page=100'*) cat "$FIXTURE/assets.json" ;;
  *'/releases/assets/'*) asset="\${@: -1}"; cat "$FIXTURE/\${asset##*/}" ;;
  *) exit 99 ;;
esac
`, { mode: 0o755 });
  const run = () => spawnSync('bash', ['-euo', 'pipefail', '-c', runStep(npmWorkflow, 'Download GitHub release assets and checksums')], {
    cwd: work, encoding: 'utf8',
    env: { ...process.env, PATH: `${join(dir, 'bin')}:${process.env.PATH}`, FIXTURE: dir, RELEASE_TAG: 'v1.2.3' },
  });
  const result = run();
  assert.equal(result.status, 0, result.stderr);
  // npm ships no Windows package and needs no image pointer, so it leaves both behind.
  const npmAssets = assets.filter(asset => !asset.name.includes('-windows-msvc.') && asset.name !== 'agent-dev-image.txt');
  assert.deepEqual(readdirSync(join(work, 'release-assets')).sort(), npmAssets.map(asset => asset.name).sort());
  for (const asset of npmAssets) {
    assert.deepEqual(readFileSync(join(work, 'release-assets', asset.name)), readFileSync(join(dir, String(asset.id))));
  }
  rmSync(join(work, 'release-assets'), { recursive: true });
  writeFileSync(join(dir, 'assets.json'), JSON.stringify(assets.slice(1)));
  const missing = run();
  assert.notEqual(missing.status, 0);
});
for (const { fail = '', neverReadable = false } of [{}, { fail: 'linux-x64-gnu' }, { neverReadable: true }]) {
  const outcome = fail
    ? 'a failure blocks the wrapper'
    : neverReadable
      ? 'the wrapper publishes while the registry still 404s the platform versions'
      : 'all platforms precede the wrapper';
  // Hard-won: 99a2e52: registry reads lagged successful publishes, leaving prior releases behind.
  test(`npm uploads run concurrently and ${outcome}`, t => {
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
    ((count >= 4)) && break
    sleep 0.02
  done
  ((count >= 4)) || exit 22
  [[ -z "$FAIL_PLATFORM" || "$name" != *"$FAIL_PLATFORM"* ]] || exit 23
else
  for platform in darwin-x64 darwin-arm64 linux-x64-gnu linux-arm64-gnu; do
    test -f "$FIXTURE/brokkai-mjolnir-$platform-1.2.3.ready"
  done
fi
touch "$FIXTURE/$name.ready"
`, { mode: 0o755 });
    const curlBody = neverReadable
      // The registry's read path lags its write path, so a just-published
      // version can keep returning 404 long after npm publish succeeded.
      ? 'exit 1\n'
      : `url="\${@: -1}"
package="\${url#*%2F}"
package="\${package%/*}"
test -f "$FIXTURE/brokkai-$package-1.2.3.ready"
`;
    writeFileSync(join(dir, 'bin/curl'), `#!/bin/bash
${curlBody}`, { mode: 0o755 });
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
    assert.ok(files.includes('brokkai-mjolnir-darwin-arm64-1.2.3.ready'));
    assert.ok(files.includes('brokkai-mjolnir-linux-arm64-gnu-1.2.3.ready'));
  });
}
