// scripts/run.sh and scripts/install.sh promise in their headers that they
// reuse each other's Cargo artifacts. That holds only while every shared
// binary is built with the same arguments: a single differing profile flag
// sends the two scripts to different target directories, and each invocation
// then recompiles the whole workspace. These tests compare the Cargo command
// lines the two scripts actually produce.
import assert from 'node:assert/strict';
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

function fixture() {
  const root = mkdtempSync(path.join(tmpdir(), 'mj-alignment-'));
  const scripts = path.join(root, 'scripts');
  const bin = path.join(root, 'bin');
  mkdirSync(path.join(scripts, 'lib'), { recursive: true });
  mkdirSync(bin);
  for (const name of ['run.sh', 'install.sh', 'build-linux-worker.sh']) {
    copyFileSync(new URL(name, import.meta.url), path.join(scripts, name));
    chmodSync(path.join(scripts, name), 0o755);
  }
  copyFileSync(new URL('lib/build.sh', import.meta.url), path.join(scripts, 'lib', 'build.sh'));

  const tool = (name, body) => writeFileSync(path.join(bin, name), `#!/bin/bash\nset -eu\n${body}\n`, { mode: 0o755 });
  tool('uname', 'case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; esac');
  tool('rustup', 'echo x86_64-unknown-linux-musl');

  // Record every Cargo command line, then behave like Cargo: write the binary
  // and report its path as a compiler artifact.
  writeFileSync(path.join(bin, 'cargo'), `#!${process.execPath}
import fs from 'node:fs';
import path from 'node:path';
const args = process.argv.slice(2);
fs.appendFileSync(process.env.BUILD_LOG, JSON.stringify(args) + '\\n');
const value = (key, fallback) => {
  const i = args.indexOf(key);
  return i >= 0 ? args[i+1] : args.find(arg => arg.startsWith(key + '='))?.slice(key.length+1) ?? fallback;
};
const binaries = args.flatMap((arg, index) => arg === '--bin' ? [args[index + 1]] : []);
for (const binary of binaries) {
const triple = value('--target', '');
const profile = args.includes('--release') ? 'release' : value('--profile', 'debug').replace(/^dev$/, 'debug');
const dir = path.resolve(value('--target-dir', 'target'), triple, profile);
fs.mkdirSync(dir, { recursive: true });
const executable = path.join(dir, binary);
fs.writeFileSync(executable, binary === 'mj' ? '#!/bin/sh\\nexit 0\\n' : 'built', { mode: 0o755 });
console.log(JSON.stringify({ reason: 'compiler-artifact', target: { name: binary, kind: ['bin'] }, executable }));
}
`, { mode: 0o755 });

  const record = (script, args) => {
    const log = path.join(root, `${script}.jsonl`);
    writeFileSync(log, '');
    const result = spawnSync('/bin/bash', [path.join(scripts, `${script}.sh`), ...args], {
      encoding: 'utf8',
      env: {
        ...process.env,
        PATH: `${bin}:${process.env.PATH}`,
        BUILD_LOG: log,
        CARGO_INSTALL_ROOT: path.join(root, 'install-root'),
      },
    });
    assert.equal(result.status, 0, `${script}.sh failed: ${result.stderr}`);
    return readFileSync(log, 'utf8').trim().split('\n').map(JSON.parse);
  };
  return { root, record };
}

// Identify a build by what it produces, so the two scripts' logs line up
// regardless of the order in which each one builds its binaries.
const key = (args) => {
  const binary = args[args.indexOf('--bin') + 1];
  const target = args.includes('--target') ? args[args.indexOf('--target') + 1] : 'host';
  return `${binary}/${target}`;
};

for (const extra of [[], ['--release'], ['--profile', 'dev']]) {
  test(`install.sh and run.sh issue identical Cargo builds for [${extra}]`, () => {
    const f = fixture();
    try {
      const installed = f.record('install', extra);
      const ran = f.record('run', [...extra, '--', '--version']);

      const byKey = (builds) => new Map(builds.flatMap(args => {
        const target = args.includes('--target') ? args[args.indexOf('--target') + 1] : 'host';
        return args.flatMap((arg, index) => arg === '--bin' ? [[`${args[index + 1]}/${target}`, args]] : []);
      }));
      const fromInstall = byKey(installed);
      const fromRun = byKey(ran);
      const shared = ['mj/host', 'mj-voice-worker/host', 'mj-worker/host', 'mj-worker/x86_64-unknown-linux-musl'].sort();
      assert.deepEqual([...fromInstall.keys()].sort(), shared);
      assert.deepEqual([...fromRun.keys()].sort(), shared);
      for (const name of shared) {
        assert.deepEqual(fromRun.get(name), fromInstall.get(name), `${name} is built differently`);
      }
    } finally { rmSync(f.root, { recursive: true, force: true }); }
  });
}

test('every build is reproducible from the committed lock file', () => {
  const f = fixture();
  try {
    for (const builds of [f.record('install', []), f.record('run', ['--', '--version'])]) {
      for (const args of builds) assert.ok(args.includes('--locked'), `${key(args)} omits --locked`);
    }
  } finally { rmSync(f.root, { recursive: true, force: true }); }
});
