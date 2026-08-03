// End-to-end packaging test: fabricate release archives for every platform,
// run build.mjs, and inspect the produced tarballs and manifest. Requires
// `tar`, `zip`, and `npm` on PATH (all present on GitHub-hosted runners).

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { PLATFORMS, expectedBinaries, releaseAssetName, sha256File } from '../lib.mjs';

const buildScript = fileURLToPath(new URL('../build.mjs', import.meta.url));

// Use a private npm cache so the test is hermetic and immune to a broken or
// root-owned user-level ~/.npm.
function runBuild(dir, buildArgs) {
  return execFileSync(process.execPath, [buildScript, ...buildArgs], {
    stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, npm_config_cache: join(dir, 'npm-cache') },
  });
}

function haveCommand(cmd) {
  try {
    execFileSync(cmd, ['--version'], { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

function makeFakeBundle(dir, name, platform, { omit = [] } = {}) {
  const bundle = join(dir, name);
  mkdirSync(bundle, { recursive: true });
  for (const bin of expectedBinaries(platform)) {
    if (omit.includes(bin)) continue;
    writeFileSync(join(bundle, bin), '#!/bin/sh\nexit 0\n');
    chmodSync(join(bundle, bin), 0o755);
  }
  writeFileSync(join(bundle, 'LICENSE'), 'GPL-3.0-only\n');
  mkdirSync(join(bundle, 'licenses'), { recursive: true });
  writeFileSync(join(bundle, 'licenses', 'SOURCE.md'), 'sources\n');
  return bundle;
}

function archiveBundle(dir, name, platform) {
  const asset = join(dir, releaseAssetName('v1.4.0', platform));
  if (platform.archive === 'zip') {
    execFileSync('zip', ['-qr', asset, name], { cwd: dir });
  } else {
    execFileSync('tar', ['-czf', asset, name], { cwd: dir });
  }
  writeFileSync(`${asset}.sha256`, `${sha256File(asset)}  ${releaseAssetName('v1.4.0', platform)}\n`);
  return asset;
}

function writeCargoToml(dir, version) {
  const path = join(dir, 'Cargo.toml');
  writeFileSync(path, `[package]\nname = "brokk-mjolnir"\nversion = "${version}"\n`);
  return path;
}

function fabricateAssets(dir) {
  for (const platform of PLATFORMS) {
    const name = `brokk-mjolnir-v1.4.0-${platform.target}`;
    makeFakeBundle(dir, name, platform);
    archiveBundle(dir, name, platform);
    rmSync(join(dir, name), { recursive: true });
  }
}

const prereqs = haveCommand('zip') && haveCommand('npm');

test('build.mjs produces variant tarballs, a root tarball, and a manifest', { skip: !prereqs }, () => {
  const dir = mkdtempSync(join(tmpdir(), 'mj-npm-e2e-'));
  try {
    const assets = join(dir, 'assets');
    mkdirSync(assets);
    fabricateAssets(assets);
    const cargoToml = writeCargoToml(dir, '1.4.0');
    const out = join(dir, 'out');

    runBuild(dir, ['--tag', 'v1.4.0', '--assets', assets, '--out', out, '--cargo-toml', cargoToml]);

    const manifest = JSON.parse(readFileSync(join(out, 'manifest.json'), 'utf8'));
    assert.equal(manifest.version, '1.4.0');
    assert.equal(manifest.variants.length, PLATFORMS.length);
    for (const variant of manifest.variants) {
      const listing = execFileSync('tar', ['-tzf', join(out, variant.tarball)], {
        encoding: 'utf8',
      });
      assert.match(listing, /package\/mj(\.exe)?$/m, `${variant.target} tarball must contain mj`);
      assert.match(listing, /package\/anvil(\.exe)?$/m);
      assert.match(listing, /package\/package\.json$/m);
    }

    const rootListing = execFileSync('tar', ['-tzf', join(out, manifest.root.tarball)], {
      encoding: 'utf8',
    });
    assert.match(rootListing, /package\/bin\/mj\.js$/m);
    assert.doesNotMatch(rootListing, /package\/mj$/m, 'root package must not embed a native binary');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('build.mjs rejects a tag that does not match Cargo.toml', { skip: !prereqs }, () => {
  const dir = mkdtempSync(join(tmpdir(), 'mj-npm-e2e-'));
  try {
    const assets = join(dir, 'assets');
    mkdirSync(assets);
    const cargoToml = writeCargoToml(dir, '9.9.9');
    assert.throws(() =>
      runBuild(dir, ['--tag', 'v1.4.0', '--assets', assets, '--out', join(dir, 'out'), '--cargo-toml', cargoToml]),
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('build.mjs fails when a desktop bundle lacks a sibling binary', { skip: !prereqs }, () => {
  const dir = mkdtempSync(join(tmpdir(), 'mj-npm-e2e-'));
  try {
    const assets = join(dir, 'assets');
    mkdirSync(assets);
    fabricateAssets(assets);

    // Rebuild the Linux x86-64 archive without the voice worker.
    const linux = PLATFORMS.find((p) => p.target === 'x86_64-unknown-linux-gnu');
    const name = `brokk-mjolnir-v1.4.0-${linux.target}`;
    rmSync(join(assets, releaseAssetName('v1.4.0', linux)));
    rmSync(join(assets, `${releaseAssetName('v1.4.0', linux)}.sha256`));
    makeFakeBundle(assets, name, linux, { omit: ['mj-voice-worker'] });
    archiveBundle(assets, name, linux);
    rmSync(join(assets, name), { recursive: true });

    const cargoToml = writeCargoToml(dir, '1.4.0');
    let failed = false;
    try {
      runBuild(dir, ['--tag', 'v1.4.0', '--assets', assets, '--out', join(dir, 'out'), '--cargo-toml', cargoToml]);
    } catch (err) {
      failed = true;
      assert.match(String(err.stderr), /missing required binary mj-voice-worker/);
    }
    assert.ok(failed, 'packaging must fail when a sibling binary is absent');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
