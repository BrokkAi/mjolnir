// Build the brokk-mjolnir npm packages from downloaded GitHub release assets.
//
// Usage:
//   node packaging/npm/build.mjs --tag v1.4.0 --assets <dir> --out <dir> [--cargo-toml Cargo.toml]
//
// For every platform in the matrix this verifies the release archive against
// its .sha256 sidecar, extracts it, checks that the required sibling binaries
// are present, and packs a platform-suffixed version of brokk-mjolnir. It then
// packs the JavaScript wrapper as the root package and writes
// <out>/manifest.json describing the publish order.

import { execFileSync } from 'node:child_process';
import {
  chmodSync,
  copyFileSync,
  cpSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

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
  variantPackageJson,
  variantVersion,
  verifyChecksum,
  versionFromTag,
} from './lib.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '..', '..');

function parseArgs(argv) {
  const args = { cargoToml: join(REPO_ROOT, 'Cargo.toml') };
  for (let i = 0; i < argv.length; i += 2) {
    const flag = argv[i];
    const value = argv[i + 1];
    if (value === undefined) throw new Error(`missing value for ${flag}`);
    switch (flag) {
      case '--tag':
        args.tag = value;
        break;
      case '--assets':
        args.assets = resolve(value);
        break;
      case '--out':
        args.out = resolve(value);
        break;
      case '--cargo-toml':
        args.cargoToml = resolve(value);
        break;
      default:
        throw new Error(`unknown flag ${flag}`);
    }
  }
  for (const required of ['tag', 'assets', 'out']) {
    if (!args[required]) throw new Error(`--${required} is required`);
  }
  return args;
}

function run(cmd, cmdArgs, opts = {}) {
  return execFileSync(cmd, cmdArgs, { stdio: ['ignore', 'pipe', 'inherit'], ...opts });
}

function listFiles(dir, base = dir) {
  const out = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...listFiles(full, base));
    else out.push(relative(base, full));
  }
  return out;
}

function extractArchive(assetPath, platform, dest) {
  mkdirSync(dest, { recursive: true });
  if (platform.archive === 'zip') {
    run('unzip', ['-q', assetPath, '-d', dest]);
  } else {
    run('tar', ['-xzf', assetPath, '-C', dest]);
  }
  // Archives contain a single top-level directory named after the release.
  const entries = readdirSync(dest);
  if (entries.length !== 1 || !statSync(join(dest, entries[0])).isDirectory()) {
    throw new Error(`unexpected archive layout for ${assetPath}: ${entries.join(', ')}`);
  }
  return join(dest, entries[0]);
}

function npmPack(pkgDir, outDir) {
  const stdout = run('npm', ['pack', '--json', '--pack-destination', outDir], {
    cwd: pkgDir,
    encoding: 'utf8',
  });
  const [info] = JSON.parse(stdout);
  return info.filename;
}

function guardPayload(pkgDir, label) {
  const files = listFiles(pkgDir);
  const bad = forbiddenEntries(files);
  if (bad.length > 0) {
    throw new Error(`refusing to package ${label}: forbidden files present: ${bad.join(', ')}`);
  }
}

function buildVariant(args, version, platform, stagingDir, outDir) {
  const assetName = releaseAssetName(args.tag, platform);
  const assetPath = join(args.assets, assetName);
  verifyChecksum(assetPath, `${assetPath}.sha256`);

  const extractDir = join(stagingDir, `extract-${platform.target}`);
  const bundleDir = extractArchive(assetPath, platform, extractDir);
  assertBundleComplete(bundleDir, platform);

  const pkgDir = join(stagingDir, `pkg-${platform.target}`);
  cpSync(bundleDir, pkgDir, { recursive: true });
  if (!platform.os.includes('win32')) {
    for (const bin of expectedBinaries(platform)) {
      chmodSync(join(pkgDir, bin), 0o755);
    }
  }
  guardPayload(pkgDir, `${PACKAGE_NAME}@${variantVersion(version, platform)}`);

  writeFileSync(
    join(pkgDir, 'package.json'),
    JSON.stringify(variantPackageJson(version, platform, basePackageFields()), null, 2) + '\n',
  );
  const tarball = npmPack(pkgDir, outDir);
  rmSync(extractDir, { recursive: true, force: true });
  return {
    target: platform.target,
    alias: platform.alias,
    version: variantVersion(version, platform),
    distTag: platform.distTag,
    tarball,
  };
}

function buildRoot(version, stagingDir, outDir) {
  const pkgDir = join(stagingDir, 'pkg-root');
  mkdirSync(join(pkgDir, 'bin'), { recursive: true });
  copyFileSync(join(HERE, 'launcher', 'bin', 'mj.js'), join(pkgDir, 'bin', 'mj.js'));
  chmodSync(join(pkgDir, 'bin', 'mj.js'), 0o755);
  copyFileSync(join(REPO_ROOT, 'LICENSE'), join(pkgDir, 'LICENSE'));
  writeFileSync(
    join(pkgDir, 'README.md'),
    `# brokk-mjolnir

Mjolnir is a forge-grade terminal client for any Agent Client Protocol (ACP)
server. This npm package installs the native \`mj\` binary for your platform —
no Rust toolchain and no first-run product download.

\`\`\`bash
npm install -g brokk-mjolnir
mj --version

# or run one-shot
npx -y brokk-mjolnir
\`\`\`

Supported platforms: macOS (universal), Linux x86-64 and ARM64 (glibc),
Windows x86-64, and Android ARM64. Desktop installs bundle the \`anvil\`
runtime and the \`mj-voice-worker\` voice sidecar next to \`mj\`; Android
bundles \`mj\` and \`anvil\`.

Upgrades are owned by npm (\`npm update -g brokk-mjolnir\`); the in-place
self-updater is disabled under this installation method.

Documentation: https://mjolnir.brokk.ai/install/
`,
  );
  writeFileSync(
    join(pkgDir, 'package.json'),
    JSON.stringify(rootPackageJson(version, basePackageFields()), null, 2) + '\n',
  );
  guardPayload(pkgDir, `${PACKAGE_NAME}@${version}`);
  return { version, tarball: npmPack(pkgDir, outDir) };
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const version = versionFromTag(args.tag);
  const cargoVersion = cargoTomlVersion(readFileSync(args.cargoToml, 'utf8'));
  if (version !== cargoVersion) {
    throw new Error(
      `release tag ${args.tag} does not match Cargo.toml version ${cargoVersion}`,
    );
  }

  rmSync(args.out, { recursive: true, force: true });
  mkdirSync(args.out, { recursive: true });
  const stagingDir = join(args.out, 'staging');
  mkdirSync(stagingDir);

  const variants = PLATFORMS.map((platform) =>
    buildVariant(args, version, platform, stagingDir, args.out),
  );
  const root = buildRoot(version, stagingDir, args.out);

  const manifest = { name: PACKAGE_NAME, tag: args.tag, version, variants, root };
  writeFileSync(join(args.out, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
  rmSync(stagingDir, { recursive: true, force: true });
  console.log(JSON.stringify(manifest, null, 2));
}

main();
