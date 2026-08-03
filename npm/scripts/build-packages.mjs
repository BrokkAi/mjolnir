#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";

import { platformPackages } from "../lib/platform.mjs";

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const NPM_DIR = path.resolve(SCRIPT_DIR, "..");
const DEFAULT_REPOSITORY_ROOT = path.resolve(NPM_DIR, "..");
const PACKAGE_NAME = "@brokkai/mjolnir";

function copyFile(source, destination) {
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  fs.copyFileSync(source, destination);
}

function writeJson(destination, value) {
  fs.writeFileSync(destination, `${JSON.stringify(value, null, 2)}\n`);
}

function commonPackageMetadata(version) {
  return {
    name: PACKAGE_NAME,
    version,
    description: "Forge-grade terminal client for any Agent Client Protocol (ACP) server",
    license: "GPL-3.0-only",
    repository: {
      type: "git",
      url: "git+https://github.com/BrokkAi/mjolnir.git",
    },
    homepage: "https://mjolnir.brokk.ai/",
    bugs: "https://github.com/BrokkAi/mjolnir/issues",
    engines: { node: ">=18" },
  };
}

export function createPlatformStage({
  bundleDir,
  platformKey,
  repositoryRoot = DEFAULT_REPOSITORY_ROOT,
  stageDir,
  version,
}) {
  const selected = platformPackages()[platformKey];
  if (!selected) {
    throw new Error(`Unknown npm platform ${platformKey}`);
  }
  if (!fs.statSync(bundleDir, { throwIfNoEntry: false })?.isDirectory()) {
    throw new Error(`Release bundle directory does not exist: ${bundleDir}`);
  }

  const extension = selected.os.includes("win32") ? ".exe" : "";
  const requiredBinaries = [`mj${extension}`, `anvil${extension}`];
  if (!selected.os.includes("android")) {
    requiredBinaries.push(`mj-voice-worker${extension}`);
  }
  for (const binary of requiredBinaries) {
    const binaryPath = path.join(bundleDir, binary);
    if (!fs.statSync(binaryPath, { throwIfNoEntry: false })?.isFile()) {
      throw new Error(
        `Release bundle for ${platformKey} is missing required sibling binary: ${binary}`,
      );
    }
  }

  fs.mkdirSync(stageDir, { recursive: true });
  const vendorDir = path.join(stageDir, "vendor", selected.target);
  fs.cpSync(bundleDir, vendorDir, { recursive: true });
  copyFile(path.join(repositoryRoot, "README.md"), path.join(stageDir, "README.md"));
  copyFile(path.join(repositoryRoot, "LICENSE"), path.join(stageDir, "LICENSE"));

  const packageJson = {
    ...commonPackageMetadata(`${version}-${selected.npmTag}`),
    os: selected.os,
    cpu: selected.cpu,
    files: ["vendor", "README.md", "LICENSE"],
  };
  writeJson(path.join(stageDir, "package.json"), packageJson);
  return packageJson;
}

export function createRootStage({
  nativeVersion,
  repositoryRoot = DEFAULT_REPOSITORY_ROOT,
  stageDir,
  version,
}) {
  fs.mkdirSync(path.join(stageDir, "bin"), { recursive: true });
  fs.mkdirSync(path.join(stageDir, "lib"), { recursive: true });
  copyFile(path.join(NPM_DIR, "bin", "mj.js"), path.join(stageDir, "bin", "mj.js"));
  copyFile(
    path.join(NPM_DIR, "lib", "platform.mjs"),
    path.join(stageDir, "lib", "platform.mjs"),
  );
  copyFile(path.join(repositoryRoot, "README.md"), path.join(stageDir, "README.md"));
  copyFile(path.join(repositoryRoot, "LICENSE"), path.join(stageDir, "LICENSE"));

  const optionalDependencies = Object.fromEntries(
    Object.values(platformPackages()).map((selected) => [
      selected.alias,
      `npm:${PACKAGE_NAME}@${nativeVersion ?? version}-${selected.npmTag}`,
    ]),
  );
  const packageJson = {
    ...commonPackageMetadata(version),
    bin: { mj: "bin/mj.js" },
    files: ["bin", "lib", "README.md", "LICENSE"],
    optionalDependencies,
    publishConfig: { access: "public" },
  };
  writeJson(path.join(stageDir, "package.json"), packageJson);
  return packageJson;
}

function pack(stageDir, outputDir, tag) {
  fs.mkdirSync(outputDir, { recursive: true });
  execFileSync("npm", ["pack", "--pack-destination", outputDir], {
    cwd: stageDir,
    stdio: "inherit",
  });
  console.log(`Built ${tag} npm package in ${outputDir}`);
}

function parseArgs(argv) {
  const values = { bundles: new Map() };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (
      arg === "--version" ||
      arg === "--native-version" ||
      arg === "--output-dir" ||
      arg === "--repository-root"
    ) {
      values[arg.slice(2).replace("-", "_")] = argv[++index];
    } else if (arg === "--bundle") {
      const value = argv[++index];
      const separator = value.indexOf("=");
      if (separator === -1) {
        throw new Error("--bundle must use <platform>=<directory>");
      }
      values.bundles.set(value.slice(0, separator), value.slice(separator + 1));
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }
  if (!values.version || !values.output_dir) {
    throw new Error("Usage: build-packages.mjs --version <version> [--native-version <version>] --output-dir <dir> [--repository-root <dir>] [--bundle <platform>=<dir> ...]");
  }
  return values;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const outputDir = path.resolve(args.output_dir);
  const repositoryRoot = path.resolve(args.repository_root ?? DEFAULT_REPOSITORY_ROOT);
  const workDir = fs.mkdtempSync(path.join(os.tmpdir(), "mjolnir-npm-"));
  try {
    for (const [platformKey, bundleDir] of args.bundles) {
      const stageDir = path.join(workDir, platformKey);
      createPlatformStage({
        bundleDir: path.resolve(bundleDir),
        platformKey,
        repositoryRoot,
        stageDir,
        version: args.version,
      });
      pack(stageDir, outputDir, platformKey);
    }

    const rootStage = path.join(workDir, "root");
    createRootStage({
      nativeVersion: args.native_version,
      repositoryRoot,
      stageDir: rootStage,
      version: args.version,
    });
    pack(rootStage, outputDir, "root");
  } finally {
    fs.rmSync(workDir, { force: true, recursive: true });
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main();
}
