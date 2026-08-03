// Generate Formula/mjolnir.rb for the BrokkAi/homebrew-tap repository from
// downloaded GitHub release checksum sidecars.
//
// Usage:
//   node packaging/homebrew/generate.mjs --tag v1.4.0 --assets <dir> --out <file> [--cargo-toml Cargo.toml]
//
// The assets directory must contain the .sha256 sidecars for the three
// Homebrew targets (macOS universal, Linux x86-64 glibc, Linux ARM64 glibc).
// The tag must match the crate version in Cargo.toml. Any missing or
// malformed input fails the run; nothing is written on error.

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  HOMEBREW_TARGETS,
  cargoTomlVersion,
  parseSha256Sidecar,
  releaseAssetName,
  renderFormula,
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

function readSidecar(assetsDir, tag, target) {
  const assetName = releaseAssetName(tag, target);
  const sidecarPath = join(assetsDir, `${assetName}.sha256`);
  let text;
  try {
    text = readFileSync(sidecarPath, 'utf8');
  } catch {
    throw new Error(`missing sha256 sidecar for ${target}: ${sidecarPath}`);
  }
  let parsed;
  try {
    parsed = parseSha256Sidecar(text);
  } catch {
    throw new Error(`unparseable sha256 sidecar ${sidecarPath}`);
  }
  if (parsed.filename !== assetName) {
    throw new Error(
      `sidecar ${sidecarPath} is for ${parsed.filename}, expected ${assetName}`,
    );
  }
  return parsed.sha256;
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

  const shas = {};
  for (const target of HOMEBREW_TARGETS) {
    shas[target] = readSidecar(args.assets, args.tag, target);
  }

  const formula = renderFormula({ version, tag: args.tag, shas });
  mkdirSync(dirname(args.out), { recursive: true });
  writeFileSync(args.out, formula);
  console.log(`wrote ${args.out} for mjolnir ${version}`);
}

main();
