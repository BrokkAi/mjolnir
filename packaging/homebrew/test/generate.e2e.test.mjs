// End-to-end generation test: fabricate .sha256 sidecars for the three
// Homebrew targets, run generate.mjs, and inspect the written formula.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { HOMEBREW_TARGETS, releaseAssetName } from '../lib.mjs';

const generateScript = fileURLToPath(new URL('../generate.mjs', import.meta.url));

function runGenerate(generateArgs) {
  return execFileSync(process.execPath, [generateScript, ...generateArgs], {
    stdio: ['ignore', 'pipe', 'pipe'],
    encoding: 'utf8',
  });
}

function fakeSha(target) {
  // Deterministic, distinct, lowercase hex per target.
  return target.charCodeAt(0).toString(16).padStart(2, '0').repeat(32);
}

function fabricateSidecars(dir, tag) {
  mkdirSync(dir, { recursive: true });
  for (const target of HOMEBREW_TARGETS) {
    const assetName = releaseAssetName(tag, target);
    writeFileSync(join(dir, `${assetName}.sha256`), `${fakeSha(target)}  ${assetName}\n`);
  }
}

function writeCargoToml(dir, version) {
  const path = join(dir, 'Cargo.toml');
  writeFileSync(path, `[package]\nname = "brokk-mjolnir"\nversion = "${version}"\n`);
  return path;
}

test('generate.mjs writes a formula with each target checksum', () => {
  const dir = mkdtempSync(join(tmpdir(), 'mj-brew-e2e-'));
  try {
    const assets = join(dir, 'assets');
    fabricateSidecars(assets, 'v1.4.0');
    const cargoToml = writeCargoToml(dir, '1.4.0');
    const out = join(dir, 'out', 'mjolnir.rb');

    runGenerate(['--tag', 'v1.4.0', '--assets', assets, '--out', out, '--cargo-toml', cargoToml]);

    const formula = readFileSync(out, 'utf8');
    assert.match(formula, /^class Mjolnir < Formula$/m);
    assert.match(formula, /^\s*version "1\.4\.0"$/m);
    for (const target of HOMEBREW_TARGETS) {
      assert.ok(
        formula.includes(`releases/download/v1.4.0/${releaseAssetName('v1.4.0', target)}`),
        `formula must reference the ${target} archive`,
      );
      assert.ok(
        formula.includes(`sha256 "${fakeSha(target)}"`),
        `formula must carry the ${target} checksum`,
      );
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('generate.mjs rejects a tag that does not match Cargo.toml', () => {
  const dir = mkdtempSync(join(tmpdir(), 'mj-brew-e2e-'));
  try {
    const assets = join(dir, 'assets');
    fabricateSidecars(assets, 'v1.4.0');
    const cargoToml = writeCargoToml(dir, '9.9.9');
    const out = join(dir, 'out', 'mjolnir.rb');

    let failed = false;
    try {
      runGenerate(['--tag', 'v1.4.0', '--assets', assets, '--out', out, '--cargo-toml', cargoToml]);
    } catch (err) {
      failed = true;
      assert.match(String(err.stderr), /does not match Cargo\.toml version/);
    }
    assert.ok(failed, 'generation must fail on a tag/Cargo.toml mismatch');
    assert.ok(!existsSync(out), 'no formula may be written on failure');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('generate.mjs fails when a sidecar is missing', () => {
  const dir = mkdtempSync(join(tmpdir(), 'mj-brew-e2e-'));
  try {
    const assets = join(dir, 'assets');
    fabricateSidecars(assets, 'v1.4.0');
    rmSync(join(assets, `${releaseAssetName('v1.4.0', 'aarch64-unknown-linux-gnu')}.sha256`));
    const cargoToml = writeCargoToml(dir, '1.4.0');
    const out = join(dir, 'out', 'mjolnir.rb');

    let failed = false;
    try {
      runGenerate(['--tag', 'v1.4.0', '--assets', assets, '--out', out, '--cargo-toml', cargoToml]);
    } catch (err) {
      failed = true;
      assert.match(String(err.stderr), /missing sha256 sidecar for aarch64-unknown-linux-gnu/);
    }
    assert.ok(failed, 'generation must fail when a sidecar is absent');
    assert.ok(!existsSync(out), 'no formula may be written on failure');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('generate.mjs fails on a malformed or mislabeled sidecar', () => {
  const dir = mkdtempSync(join(tmpdir(), 'mj-brew-e2e-'));
  try {
    const assets = join(dir, 'assets');
    fabricateSidecars(assets, 'v1.4.0');
    const cargoToml = writeCargoToml(dir, '1.4.0');
    const out = join(dir, 'out', 'mjolnir.rb');
    const macSidecar = join(
      assets,
      `${releaseAssetName('v1.4.0', 'universal-apple-darwin')}.sha256`,
    );

    writeFileSync(macSidecar, 'not a checksum\n');
    assert.throws(
      () => runGenerate(['--tag', 'v1.4.0', '--assets', assets, '--out', out, '--cargo-toml', cargoToml]),
      (err) => /unparseable sha256 sidecar/.test(String(err.stderr)),
    );

    // A well-formed sidecar recorded for a different file is also rejected.
    writeFileSync(macSidecar, `${'a'.repeat(64)}  some-other-file.tar.gz\n`);
    assert.throws(
      () => runGenerate(['--tag', 'v1.4.0', '--assets', assets, '--out', out, '--cargo-toml', cargoToml]),
      (err) => /is for some-other-file\.tar\.gz, expected/.test(String(err.stderr)),
    );
    assert.ok(!existsSync(out), 'no formula may be written on failure');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
