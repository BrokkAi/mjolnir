import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const root = resolve(import.meta.dirname, '..');
function fixture(t) {
  const dir = mkdtempSync(join(tmpdir(), 'mj-linux-release-test-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}
function script(path, text) { writeFileSync(path, `#!/bin/bash\n${text}\n`, { mode: 0o755 }); }

function installer(t, { broken = false, missing = false, corrupt = false, cached = false, cachedBroken = false } = {}) {
  const dir = fixture(t);
  const bin = join(dir, 'bin');
  const bundle = join(dir, 'bundle');
  const tools = join(dir, 'tools');
  const cache = join(dir, '.cache/mjolnir-installer/checksums');
  for (const path of [bin, bundle, tools, cache]) mkdirSync(path, { recursive: true });
  const companions = ['mj-desktop', 'mj-voice-worker', 'mj-worker-x86_64-unknown-linux-musl', 'mj-worker-aarch64-unknown-linux-musl'];
  const old = `#!/bin/bash\necho old\nexit ${cachedBroken ? 1 : 0}\n`;
  writeFileSync(join(bin, 'mj'), old, { mode: 0o755 });
  for (const name of companions) {
    script(join(bin, name), 'echo old-companion');
    if (!missing || name !== companions.at(-1)) script(join(bundle, name), 'echo new-companion');
  }
  script(join(bundle, 'mj'), broken ? 'echo "GLIBC_2.39 not found" >&2; exit 1' : 'echo "mj fixture"');
  const asset = 'brokk-mjolnir-v9.9.9-x86_64-unknown-linux-gnu.tar.gz';
  assert.equal(spawnSync('tar', ['czf', join(dir, asset), '-C', bundle, '.']).status, 0);
  const checksum = createHash('sha256').update(readFileSync(join(dir, asset))).digest('hex');
  writeFileSync(join(dir, `${asset}.sha256`), `${corrupt ? "0".repeat(64) : checksum}  ${asset}\n`);
  writeFileSync(join(cache, 'mj.sha256'), cached ? `${checksum}\n` : 'old-checksum\n');
  writeFileSync(join(dir, 'release.json'), JSON.stringify({ tag_name: 'v9.9.9', assets: [asset, `${asset}.sha256`].map(name => ({ browser_download_url: `https://fixture.invalid/${name}` })) }));
  script(join(tools, 'uname'), 'case "$1" in -s) echo Linux;; -m) echo x86_64;; -o) echo GNU/Linux;; esac');
  script(join(tools, 'curl'), `while (( $# )); do
  case "$1" in -o) dest="$2"; shift 2;; *) url="$1"; shift;; esac
done
case "$url" in
  https://api.github.com/*) cp "$FIXTURE/release.json" "$dest";;
  https://fixture.invalid/*) echo "$url" >> "$FIXTURE/downloads"; cp "$FIXTURE/\${url##*/}" "$dest";;
  *) exit 2;;
esac`);
  const result = spawnSync('bash', [join(root, 'install.sh')], { timeout: 30000, encoding: 'utf8', env: { ...process.env, HOME: dir, PROFILE: join(dir, 'profile'), PATH: `${tools}:${bin}:${process.env.PATH}`, MJOLNIR_INSTALL_DIR: bin, MJOLNIR_VERSION: 'v9.9.9', PREFIX: '', FIXTURE: dir } });
  return { result, dir, bin, cache, old, checksum, companions, asset };
}

for (const failure of ['broken', 'missing', 'corrupt']) {
  test(`installer preserves existing bundle and checksum when candidate is ${failure}`, t => {
    const f = installer(t, { [failure]: true });
    assert.notEqual(f.result.status, 0, f.result.stdout + f.result.stderr);
    assert.equal(readFileSync(join(f.bin, 'mj'), 'utf8'), f.old);
    for (const name of f.companions) assert.match(readFileSync(join(f.bin, name), 'utf8'), /old-companion/);
    assert.equal(readFileSync(join(f.cache, 'mj.sha256'), 'utf8'), 'old-checksum\n');
    assert.ok(!existsSync(join(f.dir, 'profile')));
    assert.doesNotMatch(f.result.stdout, /installer: done/);
    assert.match(f.result.stderr, failure === 'broken' ? /GLIBC_2.39 not found[\s\S]*glibc 2.28/ : failure === 'missing' ? /expected binary/ : /checksum mismatch/);
  });
}
test('installer installs a runnable bundle and records its checksum', t => {
  const f = installer(t);
  assert.equal(f.result.status, 0, f.result.stdout + f.result.stderr);
  assert.match(readFileSync(join(f.bin, 'mj'), 'utf8'), /mj fixture/);
  assert.equal(readFileSync(join(f.cache, 'mj.sha256'), 'utf8'), `${f.checksum}\n`);
  assert.match(f.result.stdout, /installer: done/);
});
test('installer skips a verified cached executable', t => {
  const f = installer(t, { cached: true });
  assert.equal(f.result.status, 0, f.result.stderr);
  assert.match(f.result.stdout, /skipping download/);
  assert.equal(readFileSync(join(f.bin, 'mj'), 'utf8'), f.old);
});
test('installer repairs an unusable executable despite a matching cached checksum', t => {
  const f = installer(t, { cached: true, cachedBroken: true });
  assert.equal(f.result.status, 0, f.result.stderr);
  assert.doesNotMatch(f.result.stdout, /skipping download/);
  assert.match(readFileSync(join(f.bin, 'mj'), 'utf8'), /mj fixture/);
});

function verify(t, { arm = false, version = '2.28', library = 'libc.so.6', machine, interpreter } = {}) {
  const dir = fixture(t);
  writeFileSync(join(dir, 'mj'), 'fixture');
  script(join(dir, 'readelf'), `case "$1" in
  -hW) echo '  Machine: ${machine ?? (arm ? 'AArch64' : 'Advanced Micro Devices X86-64')}' ;;
  -lW) echo ' [Requesting program interpreter: ${interpreter ?? (arm ? '/lib/ld-linux-aarch64.so.1' : '/lib64/ld-linux-x86-64.so.2')}]' ;;
  --version-info) echo 'Name: GLIBC_${version}' ;;
  -dW) echo ' (NEEDED) Shared library: [${library}]' ;;
esac`);
  return spawnSync('bash', [join(root, 'scripts/verify-linux-release-elf.sh'), join(dir, 'mj'), `${arm ? 'aarch64' : 'x86_64'}-unknown-linux-gnu`], { timeout: 30000, encoding: 'utf8', env: { ...process.env, PATH: `${dir}:${process.env.PATH}` } });
}
for (const arm of [false, true]) test(`ELF verifier accepts glibc 2.28 for ${arm ? 'ARM64' : 'x86-64'}`, t => {
  const r = verify(t, { arm }); assert.equal(r.status, 0, r.stderr);
});
for (const [name, options, message] of [
  ['newer glibc', { version: '2.39' }, /maximum GLIBC/],
  ['wrong architecture', { machine: 'AArch64' }, /requires machine/],
  ['wrong loader', { interpreter: '/wrong/loader' }, /requires interpreter/],
  ['unexpected dependency', { library: 'libssl.so.3' }, /unexpected dynamic dependency/],
  ['missing libc', { library: 'libm.so.6' }, /libc.so.6 is missing/],
  ['missing GLIBC versions', { version: 'PRIVATE' }, /no GLIBC symbol versions/],
]) test(`ELF verifier rejects ${name}`, t => {
  const r = verify(t, options); assert.notEqual(r.status, 0); assert.match(r.stderr, message);
});
