import assert from 'node:assert/strict';
import { chmodSync, copyFileSync, existsSync, mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

function fixture() {
  const root = mkdtempSync(path.join(tmpdir(), 'mj-run-'));
  const scripts = path.join(root, 'scripts');
  const bin = path.join(root, 'bin');
  mkdirSync(scripts);
  mkdirSync(bin);
  for (const name of ['run.sh', 'build-linux-worker.sh']) {
    copyFileSync(new URL(name, import.meta.url), path.join(scripts, name));
    chmodSync(path.join(scripts, name), 0o755);
  }
  const tool = (name, body) => {
    writeFileSync(path.join(bin, name), `#!/bin/bash\nset -eu\n${body}\n`, { mode: 0o755 });
  };
  tool('uname', 'echo Darwin');
  tool('docker', `
    if [ "$1" = info ]; then exit 0; fi
    for arg in "$@"; do if [ "$arg" = uname ]; then echo aarch64; exit 0; fi; done
    if [ "\${FAIL_BUILD:-0}" = 1 ]; then exit 42; fi
    for arg in "$@"; do
      case "$arg" in
        type=bind,source=*,target=/output)
          output=\${arg#type=bind,source=}
          output=\${output%,target=/output}
          mkdir -p "$output"
          touch "$output/mj-worker"
          chmod 755 "$output/mj-worker"
          ;;
      esac
    done
  `);
  tool('cargo', `
    if [ "$1" = run ]; then
      test -x "$EXPECTED_WORKER"
      test "$MJ_DEV_RESTART_STALE_DAEMON" = 1
      touch "$RAN_CLIENT"
    fi
  `);
  return { root, bin, script: path.join(scripts, 'run.sh') };
}

for (const release of [false, true]) {
  test(`macOS ${release ? 'release' : 'debug'} launch prepares a Linux worker before running the client`, () => {
    const { root, bin, script } = fixture();
    try {
      const marker = path.join(root, 'client-ran');
      const result = spawnSync('/bin/bash', [script, ...(release ? ['--release'] : []), '--', 'doctor'], {
        encoding: 'utf8',
        env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, RAN_CLIENT: marker,
          EXPECTED_WORKER: path.join(root, 'target/worker/aarch64-unknown-linux-musl', release ? 'release' : 'debug', 'mj-worker') },
      });
      assert.equal(result.status, 0, result.stderr);
      assert.ok(existsSync(marker));
    } finally { rmSync(root, { recursive: true, force: true }); }
  });
}

test('a failed Linux worker build prevents starting a daemon with missing assets', () => {
  const { root, bin, script } = fixture();
  try {
    const marker = path.join(root, 'client-ran');
    const result = spawnSync('/bin/bash', [script], {
      encoding: 'utf8',
      env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, FAIL_BUILD: '1', RAN_CLIENT: marker },
    });
    assert.equal(result.status, 42, result.stderr);
    assert.equal(existsSync(marker), false);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
