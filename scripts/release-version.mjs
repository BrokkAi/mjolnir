#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = path.join(repositoryRoot, "Cargo.toml");
const internalPackages = [
  "brokk-mj-core",
  "brokk-mj-agents",
  "brokk-mj-anvil",
  "brokk-mj-tui",
  "brokk-mj-remote",
  "brokk-mj-desktop",
];

function expectedManifest(manifest) {
  const workspacePackage = manifest.match(
    /\[workspace\.package\][\s\S]*?^version = "([^"]+)"$/m,
  );
  if (!workspacePackage) {
    throw new Error("could not find [workspace.package] version in Cargo.toml");
  }

  const version = workspacePackage[1];
  let updated = manifest;
  for (const packageName of internalPackages) {
    const dependency = new RegExp(
      `(package = "${packageName}", path = "[^"]+", version = ")[^"]+(" \\})`,
    );
    if (!dependency.test(updated)) {
      throw new Error(`could not find workspace dependency for ${packageName}`);
    }
    updated = updated.replace(dependency, `$1${version}$2`);
  }
  return { updated, version };
}

const command = process.argv[2];
if (command !== "sync" && command !== "check") {
  console.error("usage: node scripts/release-version.mjs <sync|check>");
  process.exit(2);
}

const manifest = fs.readFileSync(manifestPath, "utf8");
const { updated, version } = expectedManifest(manifest);

if (command === "check") {
  if (updated !== manifest) {
    console.error(
      `Cargo.toml internal dependency versions do not match workspace version ${version}; run node scripts/release-version.mjs sync`,
    );
    process.exit(1);
  }
  console.log(`workspace release versions match ${version}`);
} else {
  fs.writeFileSync(manifestPath, updated);
  console.log(`synchronized workspace dependency versions to ${version}`);
}
