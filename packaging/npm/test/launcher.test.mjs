import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

import { PLATFORMS } from '../lib.mjs';

const require = createRequire(import.meta.url);
const launcherPath = fileURLToPath(new URL('../launcher/bin/mj.js', import.meta.url));
const { selectPackage, NATIVE_PACKAGES } = require(launcherPath);

test('launcher package map stays in sync with the platform matrix', () => {
  const matrixPkgs = new Set(PLATFORMS.map((p) => p.pkg));
  const launcherPkgs = new Set(Object.values(NATIVE_PACKAGES));
  assert.deepEqual([...launcherPkgs].sort(), [...matrixPkgs].sort());

  // Every os/cpu combination declared in the matrix must be routable.
  for (const p of PLATFORMS) {
    for (const os of p.os) {
      for (const cpu of p.cpu) {
        assert.equal(
          NATIVE_PACKAGES[`${os}-${cpu}`],
          p.pkg,
          `launcher must route ${os}-${cpu} to ${p.pkg}`,
        );
      }
    }
  }
});

test('selectPackage routes supported platforms and rejects unsupported ones', () => {
  assert.equal(selectPackage('darwin', 'arm64', false).pkg, '@brokkai/mjolnir-darwin-universal');
  assert.equal(selectPackage('darwin', 'x64', false).pkg, '@brokkai/mjolnir-darwin-universal');
  assert.equal(selectPackage('linux', 'x64', false).pkg, '@brokkai/mjolnir-linux-x64-gnu');
  assert.equal(selectPackage('linux', 'arm64', false).pkg, '@brokkai/mjolnir-linux-arm64-gnu');
  assert.equal(selectPackage('android', 'arm64', false).pkg, '@brokkai/mjolnir-android-arm64');
  assert.equal(selectPackage('win32', 'x64', false).pkg, '@brokkai/mjolnir-win32-x64');

  assert.match(selectPackage('linux', 'x64', true).error, /musl/);
  assert.match(selectPackage('freebsd', 'x64', false).error, /does not ship a native build/);
  assert.match(selectPackage('win32', 'arm64', false).error, /does not ship a native build/);
});

test(
  'launcher runs the native binary with bundle dir on PATH and self-update disabled',
  { skip: process.platform === 'win32' },
  () => {
    const dir = mkdtempSync(join(tmpdir(), 'mj-launcher-'));
    try {
      const fakeMj = join(dir, 'mj');
      writeFileSync(
        fakeMj,
        '#!/bin/sh\n' +
          'echo "args:$@"\n' +
          'echo "update:$MJOLNIR_NO_UPDATE_CHECK"\n' +
          'case ":$PATH:" in *":' + dir + ':"*) echo "path:ok";; *) echo "path:missing";; esac\n' +
          'exit 7\n',
      );
      chmodSync(fakeMj, 0o755);

      let stdout = '';
      let code = 0;
      try {
        stdout = execFileSync(process.execPath, [launcherPath, '--version'], {
          env: { ...process.env, BROKK_MJOLNIR_NATIVE_DIR: dir },
          encoding: 'utf8',
        });
      } catch (err) {
        stdout = err.stdout;
        code = err.status;
      }
      assert.equal(code, 7, 'launcher must preserve the native exit code');
      assert.match(stdout, /args:--version/);
      assert.match(stdout, /update:1/);
      assert.match(stdout, /path:ok/);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  },
);

test('launcher fails clearly when the native package is absent', () => {
  let stderr = '';
  let code = 0;
  try {
    execFileSync(process.execPath, [launcherPath, '--version'], {
      env: { ...process.env, BROKK_MJOLNIR_NATIVE_DIR: '/nonexistent/native/dir' },
      encoding: 'utf8',
    });
  } catch (err) {
    stderr = err.stderr;
    code = err.status;
  }
  assert.equal(code, 1);
  assert.match(stderr, /missing directory/);
});
