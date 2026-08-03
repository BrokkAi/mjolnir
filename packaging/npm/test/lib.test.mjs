import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import {
  PACKAGE_NAME,
  PLATFORMS,
  assertBundleComplete,
  basePackageFields,
  cargoTomlVersion,
  expectedBinaries,
  forbiddenEntries,
  releaseAssetName,
  rootPackageJson,
  sha256File,
  variantPackageJson,
  verifyChecksum,
  versionFromTag,
} from '../lib.mjs';

test('platform matrix covers the five declared release targets', () => {
  const targets = PLATFORMS.map((p) => p.target).sort();
  assert.deepEqual(targets, [
    'aarch64-linux-android',
    'aarch64-unknown-linux-gnu',
    'universal-apple-darwin',
    'x86_64-pc-windows-msvc',
    'x86_64-unknown-linux-gnu',
  ]);
});

test('platform matrix invariants', () => {
  const pkgs = new Set();
  for (const p of PLATFORMS) {
    assert.ok(!pkgs.has(p.pkg), `duplicate package ${p.pkg}`);
    pkgs.add(p.pkg);
    assert.match(
      p.pkg,
      /^@brokkai\/mjolnir-[a-z0-9-]+$/,
      'platform packages live under the brokkai org scope',
    );
    assert.ok(p.os.length > 0 && p.cpu.length > 0);
    if (p.os.includes('linux')) {
      assert.deepEqual(p.libc, ['glibc'], 'linux variants are glibc-only');
    }
  }
  const android = PLATFORMS.find((p) => p.target === 'aarch64-linux-android');
  assert.equal(android.voice, false, 'android must not expect the voice worker');
  const mac = PLATFORMS.find((p) => p.target === 'universal-apple-darwin');
  assert.deepEqual(mac.cpu.sort(), ['arm64', 'x64'], 'macOS build is universal');
});

test('versionFromTag accepts vX.Y.Z and rejects everything else', () => {
  assert.equal(versionFromTag('v1.4.0'), '1.4.0');
  for (const bad of ['1.4.0', 'v1.4', 'v1.4.0-rc1', 'refs/tags/v1.4.0']) {
    assert.throws(() => versionFromTag(bad), undefined, bad);
  }
});

test('cargoTomlVersion reads the crate version', () => {
  const toml = '[package]\nname = "brokk-mjolnir"\nversion = "1.4.0"\n';
  assert.equal(cargoTomlVersion(toml), '1.4.0');
  assert.throws(() => cargoTomlVersion('[package]\nname = "x"\n'));
});

test('release asset names match release.yml packaging', () => {
  const mac = PLATFORMS.find((p) => p.target === 'universal-apple-darwin');
  const win = PLATFORMS.find((p) => p.target === 'x86_64-pc-windows-msvc');
  assert.equal(
    releaseAssetName('v1.4.0', mac),
    'brokk-mjolnir-v1.4.0-universal-apple-darwin.tar.gz',
  );
  assert.equal(
    releaseAssetName('v1.4.0', win),
    'brokk-mjolnir-v1.4.0-x86_64-pc-windows-msvc.zip',
  );
});

test('variant package.json carries platform constraints at the release version', () => {
  const linux = PLATFORMS.find((p) => p.target === 'x86_64-unknown-linux-gnu');
  const pkg = variantPackageJson('1.4.0', linux, basePackageFields());
  assert.equal(pkg.name, '@brokkai/mjolnir-linux-x64-gnu');
  assert.equal(pkg.version, '1.4.0');
  assert.deepEqual(pkg.os, ['linux']);
  assert.deepEqual(pkg.cpu, ['x64']);
  assert.deepEqual(pkg.libc, ['glibc']);
  assert.equal(pkg.license, 'GPL-3.0-only');
  assert.equal(pkg.bin, undefined, 'variants expose no bin; the root launcher does');
});

test('root package.json pins every platform package as an optional dependency', () => {
  const pkg = rootPackageJson('1.4.0', basePackageFields());
  assert.equal(pkg.name, PACKAGE_NAME);
  assert.equal(pkg.version, '1.4.0');
  assert.deepEqual(pkg.bin, { mj: 'bin/mj.js' });
  assert.equal(pkg.os, undefined, 'root package must install everywhere');
  assert.equal(Object.keys(pkg.optionalDependencies).length, PLATFORMS.length);
  for (const p of PLATFORMS) {
    assert.equal(
      pkg.optionalDependencies[p.pkg],
      '1.4.0',
      'platform payloads are pinned to the exact release version',
    );
  }
});

test('checksum verification accepts matching sidecars and rejects mismatches', () => {
  const dir = mkdtempSync(join(tmpdir(), 'mj-npm-test-'));
  try {
    const asset = join(dir, 'bundle.tar.gz');
    writeFileSync(asset, 'payload');
    const good = join(dir, 'good.sha256');
    writeFileSync(good, `${sha256File(asset)}  bundle.tar.gz\n`);
    verifyChecksum(asset, good);

    const bad = join(dir, 'bad.sha256');
    writeFileSync(bad, `${'0'.repeat(64)}  bundle.tar.gz\n`);
    assert.throws(() => verifyChecksum(asset, bad), /checksum mismatch/);

    const junk = join(dir, 'junk.sha256');
    writeFileSync(junk, 'not a checksum\n');
    assert.throws(() => verifyChecksum(asset, junk), /unparseable/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('bundle completeness requires every sibling binary', () => {
  const desktop = PLATFORMS.find((p) => p.target === 'x86_64-unknown-linux-gnu');
  const android = PLATFORMS.find((p) => p.target === 'aarch64-linux-android');
  assert.deepEqual(expectedBinaries(desktop), ['mj', 'anvil', 'mj-voice-worker']);
  assert.deepEqual(expectedBinaries(android), ['mj', 'anvil']);

  const dir = mkdtempSync(join(tmpdir(), 'mj-npm-bundle-'));
  try {
    writeFileSync(join(dir, 'mj'), 'bin');
    writeFileSync(join(dir, 'anvil'), 'bin');
    assert.throws(
      () => assertBundleComplete(dir, desktop),
      /missing required binary mj-voice-worker/,
    );
    assertBundleComplete(dir, android);
    writeFileSync(join(dir, 'mj-voice-worker'), 'bin');
    assertBundleComplete(dir, desktop);

    // A directory where a binary should be is also a failure.
    rmSync(join(dir, 'anvil'));
    mkdirSync(join(dir, 'anvil'));
    assert.throws(() => assertBundleComplete(dir, android), /not a regular file/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('forbidden entry scan flags credential-shaped files and allows the bundle', () => {
  assert.deepEqual(
    forbiddenEntries([
      'mj',
      'anvil',
      'licenses/SOURCE.md',
      '.env',
      'nested/.npmrc',
      'keys/id_rsa',
      'certs/server.pem',
      '.netrc',
    ]),
    ['.env', 'nested/.npmrc', 'keys/id_rsa', 'certs/server.pem', '.netrc'],
  );
  assert.deepEqual(
    forbiddenEntries(['mj', 'anvil', 'mj-voice-worker', 'README.md', 'LICENSE']),
    [],
  );
});
