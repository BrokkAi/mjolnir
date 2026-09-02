#!/usr/bin/env node
// Keep the release version in one place.
//
// Every published crate inherits `[workspace.package] version`, so the
// package versions cannot drift. Cargo still requires each published path
// dependency to carry a registry version and does not let that version
// inherit, so `[workspace.dependencies]` repeats the number. This script
// checks those repeats against the workspace version (`check`, run in CI) and
// rewrites them after a bump (`sync`).

import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const manifestPath = path.join(repositoryRoot, "Cargo.toml");
const internalPackages = [
  "hel",
  "mj-worker",
  "mj-controller",
  "mj-chat",
  "hel-tui",
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
      `^(${packageName} = \\{ path = "[^"]+", (?:package = "[^"]+", )?version = ")[^"]+(" \\})$`,
      "m",
    );
    if (!dependency.test(updated)) {
      throw new Error(`could not find workspace dependency for ${packageName}`);
    }
    updated = updated.replace(dependency, `$1${version}$2`);
  }
  return { updated, version };
}

const command = process.argv[2];
const releaseTag = process.argv[3];
if (command !== "sync" && command !== "check") {
  console.error("usage: node scripts/release-version.mjs <sync|check> [vX.Y.Z]");
  process.exit(2);
}
if (command === "sync" && releaseTag) {
  console.error("sync does not accept a release tag");
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
  if (releaseTag) {
    const match = /^v(\d+\.\d+\.\d+)$/.exec(releaseTag);
    if (!match) {
      console.error(`release tag must look like vX.Y.Z, got: ${releaseTag}`);
      process.exit(1);
    }
    if (match[1] !== version) {
      console.error(
        `release tag ${releaseTag} does not match workspace version v${version}`,
      );
      process.exit(1);
    }
    console.log(`release tag ${releaseTag} matches workspace release versions`);
  } else {
    console.log(`workspace release versions match ${version}`);
  }
} else {
  fs.writeFileSync(manifestPath, updated);
  console.log(`synchronized workspace dependency versions to ${version}`);
}
