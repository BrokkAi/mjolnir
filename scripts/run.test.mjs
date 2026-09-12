import assert from 'node:assert/strict';
import { chmodSync, copyFileSync, existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

function fixture(platform = 'Linux') {
  const root = mkdtempSync(path.join(tmpdir(), 'mj run '));
  const scripts = path.join(root, 'scripts');
  const bin = path.join(root, 'bin');
  mkdirSync(scripts);
  mkdirSync(bin);
  for (const name of ['run.sh', 'build-linux-worker.sh']) {
    copyFileSync(new URL(name, import.meta.url), path.join(scripts, name));
    chmodSync(path.join(scripts, name), 0o755);
  }
  const tool = (name, body) => writeFileSync(path.join(bin, name), `#!/bin/bash\nset -eu\n${body}\n`, { mode: 0o755 });
  tool('uname', `case "$1" in -s) echo ${platform} ;; -m) echo x86_64 ;; esac`);
  tool('rustup', 'echo x86_64-unknown-linux-musl');
  tool('docker', `
    if [ "$1" = info ]; then exit 0; fi
    for arg in "$@"; do if [ "$arg" = uname ]; then echo aarch64; exit 0; fi; done
    if [ "\${FAIL_KIND:-}" = portable ]; then echo portable-failed >&2; exit 42; fi
    for arg in "$@"; do
      case "$arg" in type=bind,source=*,target=/output)
        output=\${arg#type=bind,source=}; output=\${output%,target=/output}
        mkdir -p "$output"; echo fresh > "$output/mj-worker"; chmod 755 "$output/mj-worker" ;;
      esac
    done
  `);
  writeFileSync(path.join(bin, 'cargo'), `#!${process.execPath}
import fs from 'node:fs';
import path from 'node:path';
const args = process.argv.slice(2);
fs.appendFileSync(process.env.BUILD_LOG, JSON.stringify(args) + '\\n');
if (args[0] !== 'build') throw new Error('client must not run through Cargo');
const value = (key, fallback) => {
  const i = args.indexOf(key);
  return i >= 0 ? args[i+1] : args.find(arg => arg.startsWith(key + '='))?.slice(key.length+1) ?? fallback;
};
const cli = value('--bin') === 'mj';
const triple = value('--target', '');
const profile = args.includes('--release') ? 'release' : value('--profile', 'debug');
const kind = cli ? 'cli' : triple ? 'portable' : 'native';
if (kind === process.env.FAIL_KIND) { console.error(kind + '-failed'); process.exit(42); }
const dir = path.resolve(value('--target-dir', 'target'), triple, profile);
fs.mkdirSync(dir, { recursive: true });
const executable = path.join(dir, cli ? 'mj' : 'mj-worker');
if (!cli) { fs.writeFileSync(executable, 'fresh'); process.exit(0); }
fs.writeFileSync(executable, ${JSON.stringify(`#!${process.execPath}
import fs from 'node:fs';
fs.writeFileSync(process.env.RAN_CLIENT, JSON.stringify({ args: process.argv.slice(2), restart: process.env.MJ_DEV_RESTART_STALE_DAEMON, marker: process.env.USER_SETTING }));
`)}, {mode: 0o755});
if (process.env.NO_ARTIFACT !== '1') console.log(JSON.stringify({ reason: 'compiler-artifact', target: {name:'mj', kind:['bin']}, executable }));
`, { mode: 0o755 });
  const marker = path.join(root, 'client-ran');
  const log = path.join(root, 'builds.jsonl');
  const run = (args = [], extra = {}) => spawnSync('/bin/bash', [path.join(scripts, 'run.sh'), ...args], {
    encoding: 'utf8', env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, BUILD_LOG: log, RAN_CLIENT: marker, USER_SETTING: 'preserved', ...extra },
  });
  return { root, marker, log, run };
}

for (const platform of ['Linux', 'Darwin']) {
  for (const release of [false, true]) {
    test(`${platform} ${release ? 'release' : 'debug'} builds workers and directly executes the CLI`, () => {
      const f = fixture(platform);
      try {
        const result = f.run([...(release ? ['--release'] : []), '--', 'doctor', 'a b', '--release']);
        assert.equal(result.status, 0, result.stderr);
        const profile = release ? 'release' : 'debug';
        assert.equal(readFileSync(path.join(f.root, 'target/worker', profile, 'mj-worker'), 'utf8').trim(), 'fresh');
        const triple = platform === 'Linux' ? 'x86_64-unknown-linux-musl' : 'aarch64-unknown-linux-musl';
        assert.equal(readFileSync(path.join(f.root, 'target/worker', triple, profile, 'mj-worker'), 'utf8').trim(), 'fresh');
        assert.deepEqual(JSON.parse(readFileSync(f.marker)), { args: ['doctor', 'a b', '--release'], restart: '1', marker: 'preserved' });
        const builds = readFileSync(f.log, 'utf8').trim().split('\n').map(JSON.parse);
        assert.ok(builds.every(args => args[0] === 'build'));
        assert.equal(builds.at(-1).includes('doctor'), false);
      } finally { rmSync(f.root, { recursive: true, force: true }); }
    });
  }
}

for (const failure of ['native', 'portable', 'cli']) {
  test(`${failure} build failure is visible and prevents launching the client`, () => {
    const f = fixture();
    try {
      const result = f.run([], { FAIL_KIND: failure });
      assert.notEqual(result.status, 0);
      assert.match(result.stderr, new RegExp(`${failure}-failed`));
      assert.equal(existsSync(f.marker), false);
    } finally { rmSync(f.root, { recursive: true, force: true }); }
  });
}

test('the reported artifact supports a custom target directory and profile', () => {
  const f = fixture();
  try {
    const result = f.run(['--target-dir', path.join(f.root, 'custom artifacts'), '--profile', 'dev', '--', '--version']);
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(JSON.parse(readFileSync(f.marker)).args, ['--version']);
  } finally { rmSync(f.root, { recursive: true, force: true }); }
});

test('missing artifact fails instead of launching a stale binary', () => {
  const f = fixture();
  try {
    const result = f.run([], { NO_ARTIFACT: '1' });
    assert.notEqual(result.status, 0);
    assert.equal(existsSync(f.marker), false);
  } finally { rmSync(f.root, { recursive: true, force: true }); }
});
