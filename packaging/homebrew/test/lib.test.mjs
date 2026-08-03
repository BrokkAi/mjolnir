import assert from 'node:assert/strict';
import test from 'node:test';

import {
  HOMEBREW_TARGETS,
  cargoTomlVersion,
  parseSha256Sidecar,
  releaseAssetName,
  releaseAssetUrl,
  renderFormula,
  versionFromTag,
} from '../lib.mjs';

function fakeShas() {
  return {
    'universal-apple-darwin': 'a'.repeat(64),
    'x86_64-unknown-linux-gnu': 'b'.repeat(64),
    'aarch64-unknown-linux-gnu': 'c'.repeat(64),
  };
}

test('homebrew targets cover macOS universal and the two glibc Linux targets', () => {
  assert.deepEqual([...HOMEBREW_TARGETS].sort(), [
    'aarch64-unknown-linux-gnu',
    'universal-apple-darwin',
    'x86_64-unknown-linux-gnu',
  ]);
});

test('versionFromTag accepts vX.Y.Z and rejects everything else', () => {
  assert.equal(versionFromTag('v1.4.0'), '1.4.0');
  for (const bad of ['1.4.0', 'v1.4', 'v1.4.0-rc1', 'refs/tags/v1.4.0', '']) {
    assert.throws(() => versionFromTag(bad), undefined, bad);
  }
});

test('cargoTomlVersion reads the crate version', () => {
  const toml = '[package]\nname = "brokk-mjolnir"\nversion = "1.4.0"\n';
  assert.equal(cargoTomlVersion(toml), '1.4.0');
  assert.throws(() => cargoTomlVersion('[package]\nname = "x"\n'));
});

test('release asset names and urls match release.yml packaging', () => {
  assert.equal(
    releaseAssetName('v1.4.0', 'universal-apple-darwin'),
    'brokk-mjolnir-v1.4.0-universal-apple-darwin.tar.gz',
  );
  assert.equal(
    releaseAssetUrl('v1.4.0', 'x86_64-unknown-linux-gnu'),
    'https://github.com/BrokkAi/mjolnir/releases/download/v1.4.0/brokk-mjolnir-v1.4.0-x86_64-unknown-linux-gnu.tar.gz',
  );
});

test('parseSha256Sidecar accepts shasum output and rejects malformed input', () => {
  const hex = 'ab'.repeat(32);
  assert.deepEqual(parseSha256Sidecar(`${hex}  bundle.tar.gz\n`), {
    sha256: hex,
    filename: 'bundle.tar.gz',
  });
  // Binary-mode marker and uppercase hex are tolerated; hex is normalized.
  assert.deepEqual(parseSha256Sidecar(`${hex.toUpperCase()}  *bundle.tar.gz`), {
    sha256: hex,
    filename: 'bundle.tar.gz',
  });
  for (const bad of [
    '',
    'not a checksum\n',
    `${hex}\n`, // no filename
    `${'ab'.repeat(16)}  bundle.tar.gz\n`, // hex too short
    `${'zz'.repeat(32)}  bundle.tar.gz\n`, // not hex
    `${hex}  a.tar.gz\n${hex}  b.tar.gz\n`, // more than one entry
  ]) {
    assert.throws(() => parseSha256Sidecar(bad), /unparseable/, JSON.stringify(bad));
  }
});

test('renderFormula produces the pinned multi-platform formula', () => {
  const formula = renderFormula({ version: '1.4.0', tag: 'v1.4.0', shas: fakeShas() });

  assert.match(formula, /^class Mjolnir < Formula$/m);
  assert.match(formula, /^\s*homepage "https:\/\/mjolnir\.brokk\.ai"$/m);
  assert.match(formula, /^\s*license "GPL-3\.0-only"$/m);
  assert.match(formula, /^\s*version "1\.4\.0"$/m);

  // Each target's archive URL appears exactly once, under the right block.
  for (const target of HOMEBREW_TARGETS) {
    const url = releaseAssetUrl('v1.4.0', target);
    assert.equal(formula.split(url).length - 1, 1, `${target} url appears once`);
  }
  assert.match(
    formula,
    /on_macos do\s+#[^\n]*\n\s*url "[^"]*universal-apple-darwin\.tar\.gz"\s+sha256 "a{64}"/,
  );
  assert.match(
    formula,
    /on_intel do\s+url "[^"]*x86_64-unknown-linux-gnu\.tar\.gz"\s+sha256 "b{64}"/,
  );
  assert.match(
    formula,
    /on_arm do\s+url "[^"]*aarch64-unknown-linux-gnu\.tar\.gz"\s+sha256 "c{64}"/,
  );

  // All three checksums are present and distinct.
  const shaLines = [...formula.matchAll(/^\s*sha256 "([0-9a-f]{64})"$/gm)].map((m) => m[1]);
  assert.equal(shaLines.length, 3);
  assert.equal(new Set(shaLines).size, 3);

  // All three sibling binaries are installed together.
  assert.match(formula, /^\s*bin\.install "mj", "anvil", "mj-voice-worker"$/m);

  // Caveats state what is and is not included, and that brew owns upgrades.
  assert.match(formula, /anvil and\s+mj-voice-worker/);
  assert.match(formula, /Bifrost is not included/);
  assert.match(formula, /https:\/\/mjolnir\.brokk\.ai\/install\//);
  assert.match(formula, /brew upgrade mjolnir/);
  assert.match(formula, /self-updater/);

  // The formula has a runnable test block.
  assert.match(
    formula,
    /test do\s+assert_match version\.to_s, shell_output\("#\{bin\}\/mj --version"\)\s+end/,
  );
});

test('renderFormula rejects incomplete or inconsistent input', () => {
  assert.throws(
    () => renderFormula({ version: '1.4.1', tag: 'v1.4.0', shas: fakeShas() }),
    /does not match/,
  );
  assert.throws(
    () => renderFormula({ version: '1.4.0', tag: '1.4.0', shas: fakeShas() }),
    /not of the form/,
  );

  const missing = fakeShas();
  delete missing['aarch64-unknown-linux-gnu'];
  assert.throws(
    () => renderFormula({ version: '1.4.0', tag: 'v1.4.0', shas: missing }),
    /missing or malformed sha256 for aarch64-unknown-linux-gnu/,
  );

  const malformed = fakeShas();
  malformed['universal-apple-darwin'] = 'A'.repeat(64); // must be lowercase hex
  assert.throws(
    () => renderFormula({ version: '1.4.0', tag: 'v1.4.0', shas: malformed }),
    /missing or malformed sha256 for universal-apple-darwin/,
  );
});
