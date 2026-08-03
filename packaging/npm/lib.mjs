// Shared logic for building the brokk-mjolnir npm packages from a GitHub
// release. Pure helpers live here so packaging/npm/test can exercise them
// without touching the network or the npm registry.

import { createHash } from 'node:crypto';
import { readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';

// One entry per published native variant. `target` matches the Rust target
// triple used by release.yml asset names; `alias` is the local-only npm alias
// name the root package declares; `distTag` is the non-latest dist-tag each
// variant is published under so the registry never serves a native payload as
// `latest`.
export const PLATFORMS = [
  {
    target: 'universal-apple-darwin',
    alias: 'brokk-mjolnir-darwin-universal',
    distTag: 'platform-darwin-universal',
    os: ['darwin'],
    cpu: ['x64', 'arm64'],
    archive: 'tar.gz',
    voice: true,
  },
  {
    target: 'x86_64-unknown-linux-gnu',
    alias: 'brokk-mjolnir-linux-x64-gnu',
    distTag: 'platform-linux-x64-gnu',
    os: ['linux'],
    cpu: ['x64'],
    libc: ['glibc'],
    archive: 'tar.gz',
    voice: true,
  },
  {
    target: 'aarch64-unknown-linux-gnu',
    alias: 'brokk-mjolnir-linux-arm64-gnu',
    distTag: 'platform-linux-arm64-gnu',
    os: ['linux'],
    cpu: ['arm64'],
    libc: ['glibc'],
    archive: 'tar.gz',
    voice: true,
  },
  {
    target: 'aarch64-linux-android',
    alias: 'brokk-mjolnir-android-arm64',
    distTag: 'platform-android-arm64',
    os: ['android'],
    cpu: ['arm64'],
    archive: 'tar.gz',
    // Android intentionally omits the unsupported voice worker.
    voice: false,
  },
  {
    target: 'x86_64-pc-windows-msvc',
    alias: 'brokk-mjolnir-win32-x64',
    distTag: 'platform-win32-x64',
    os: ['win32'],
    cpu: ['x64'],
    archive: 'zip',
    voice: true,
  },
];

export const PACKAGE_NAME = 'brokk-mjolnir';

export function versionFromTag(tag) {
  const m = /^v(\d+\.\d+\.\d+)$/.exec(tag);
  if (!m) {
    throw new Error(`release tag ${JSON.stringify(tag)} is not of the form vX.Y.Z`);
  }
  return m[1];
}

export function cargoTomlVersion(cargoTomlText) {
  // The [package] table opens the file; the workspace tables come later, so
  // the first `version = "..."` at line start is the crate version.
  const m = /^version\s*=\s*"([^"]+)"/m.exec(cargoTomlText);
  if (!m) throw new Error('could not find version in Cargo.toml');
  return m[1];
}

export function variantVersion(version, platform) {
  return `${version}-${platform.target}`;
}

export function releaseAssetName(tag, platform) {
  return `${PACKAGE_NAME}-${tag}-${platform.target}.${platform.archive}`;
}

// Binary names expected inside a release bundle for a given platform. Desktop
// bundles must contain all three siblings; Android must contain mj and anvil.
export function expectedBinaries(platform) {
  const win = platform.os.includes('win32');
  const ext = win ? '.exe' : '';
  const bins = [`mj${ext}`, `anvil${ext}`];
  if (platform.voice) bins.push(`mj-voice-worker${ext}`);
  return bins;
}

export function assertBundleComplete(bundleDir, platform) {
  for (const bin of expectedBinaries(platform)) {
    let st;
    try {
      st = statSync(join(bundleDir, bin));
    } catch {
      throw new Error(
        `release bundle for ${platform.target} is missing required binary ${bin}; ` +
          'refusing to package an incomplete bundle',
      );
    }
    if (!st.isFile()) {
      throw new Error(`release bundle entry ${bin} for ${platform.target} is not a regular file`);
    }
  }
}

export function sha256File(path) {
  const hash = createHash('sha256');
  hash.update(readFileSync(path));
  return hash.digest('hex');
}

// Sidecar format written by release.yml: `<hex>  <filename>` (shasum) on
// Unix, `<hex>  <filename>` without trailing newline on Windows.
export function verifyChecksum(assetPath, sidecarPath) {
  const sidecar = readFileSync(sidecarPath, 'utf8').trim();
  const m = /^([0-9a-fA-F]{64})\s+\*?(\S+)$/.exec(sidecar);
  if (!m) throw new Error(`unparseable sha256 sidecar ${sidecarPath}`);
  const expected = m[1].toLowerCase();
  const actual = sha256File(assetPath);
  if (actual !== expected) {
    throw new Error(`checksum mismatch for ${assetPath}: expected ${expected}, got ${actual}`);
  }
}

export function variantPackageJson(version, platform, base) {
  const pkg = {
    name: PACKAGE_NAME,
    version: variantVersion(version, platform),
    description: `Native Mjolnir ${platform.target} payload for the ${PACKAGE_NAME} npm package`,
    ...base,
    os: platform.os,
    cpu: platform.cpu,
  };
  if (platform.libc) pkg.libc = platform.libc;
  return pkg;
}

export function rootPackageJson(version, base) {
  const optionalDependencies = {};
  for (const platform of PLATFORMS) {
    optionalDependencies[platform.alias] =
      `npm:${PACKAGE_NAME}@${variantVersion(version, platform)}`;
  }
  return {
    name: PACKAGE_NAME,
    version,
    description:
      'Forge-grade terminal client for any Agent Client Protocol (ACP) server',
    ...base,
    bin: { mj: 'bin/mj.js' },
    engines: { node: '>=18' },
    optionalDependencies,
  };
}

// Metadata shared by the root and variant manifests.
export function basePackageFields() {
  return {
    license: 'GPL-3.0-only',
    repository: { type: 'git', url: 'git+https://github.com/BrokkAi/mjolnir.git' },
    homepage: 'https://mjolnir.brokk.ai',
    bugs: { url: 'https://github.com/BrokkAi/mjolnir/issues' },
  };
}

// File names that must never appear in a published payload. The build fails
// hard rather than filtering, so an unexpected file is investigated instead of
// silently dropped.
const FORBIDDEN_ENTRY = /(^|\/)(\.env[^/]*|\.npmrc|\.git|id_rsa[^/]*|id_ed25519[^/]*|[^/]*\.pem|[^/]*\.p12|[^/]*\.keystore|credentials[^/]*|\.netrc)$/i;

export function forbiddenEntries(paths) {
  return paths.filter((p) => FORBIDDEN_ENTRY.test(p));
}
