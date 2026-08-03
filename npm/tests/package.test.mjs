import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { nativeBinaryName, platformPackageFor } from "../lib/platform.mjs";
import { createPlatformStage, createRootStage } from "../scripts/build-packages.mjs";

const NPM_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

test("platform selection matches published release targets", () => {
  assert.equal(platformPackageFor("darwin", "arm64").target, "universal-apple-darwin");
  assert.equal(platformPackageFor("darwin", "x64").target, "universal-apple-darwin");
  assert.equal(platformPackageFor("linux", "x64").target, "x86_64-unknown-linux-gnu");
  assert.equal(platformPackageFor("linux", "arm64").target, "aarch64-unknown-linux-gnu");
  assert.equal(platformPackageFor("android", "arm64").target, "aarch64-linux-android");
  assert.equal(platformPackageFor("win32", "x64").target, "x86_64-pc-windows-msvc");
  assert.throws(() => platformPackageFor("win32", "arm64"), /does not publish/);
  assert.equal(nativeBinaryName("win32"), "mj.exe");
  assert.equal(nativeBinaryName("linux"), "mj");
});

test("root package exposes mj and pins every optional native payload", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "mjolnir-npm-root-test-"));
  try {
    const packageJson = createRootStage({ stageDir: root, version: "1.4.0" });
    assert.equal(packageJson.name, "@brokkai/mjolnir");
    assert.deepEqual(packageJson.bin, { mj: "bin/mj.js" });
    assert.equal(
      packageJson.optionalDependencies["@brokkai/mjolnir-linux-x64"],
      "npm:@brokkai/mjolnir@1.4.0-linux-x64",
    );
    assert.equal(Object.keys(packageJson.optionalDependencies).length, 5);
    assert.ok(fs.existsSync(path.join(root, "bin", "mj.js")));
    assert.ok(fs.existsSync(path.join(root, "lib", "platform.mjs")));
  } finally {
    fs.rmSync(root, { force: true, recursive: true });
  }
});

test("wrapper-only revisions can retain the released native payload version", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "mjolnir-npm-wrapper-test-"));
  try {
    const packageJson = createRootStage({
      nativeVersion: "1.4.0",
      stageDir: root,
      version: "1.4.0-npm.1",
    });
    assert.equal(packageJson.version, "1.4.0-npm.1");
    assert.equal(
      packageJson.optionalDependencies["@brokkai/mjolnir-linux-x64"],
      "npm:@brokkai/mjolnir@1.4.0-linux-x64",
    );
  } finally {
    fs.rmSync(root, { force: true, recursive: true });
  }
});

test("platform package preserves the full sibling release bundle", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "mjolnir-npm-platform-test-"));
  const bundle = path.join(root, "bundle");
  const stage = path.join(root, "stage");
  fs.mkdirSync(bundle);
  for (const name of ["mj", "anvil", "mj-voice-worker"]) {
    fs.writeFileSync(path.join(bundle, name), name);
  }

  try {
    const packageJson = createPlatformStage({
      bundleDir: bundle,
      platformKey: "linux-x64",
      stageDir: stage,
      version: "1.4.0",
    });
    assert.equal(packageJson.version, "1.4.0-linux-x64");
    assert.deepEqual(packageJson.os, ["linux"]);
    assert.deepEqual(packageJson.cpu, ["x64"]);
    for (const name of ["mj", "anvil", "mj-voice-worker"]) {
      assert.equal(
        fs.readFileSync(
          path.join(stage, "vendor", "x86_64-unknown-linux-gnu", name),
          "utf8",
        ),
        name,
      );
    }
  } finally {
    fs.rmSync(root, { force: true, recursive: true });
  }
});

test("desktop package rejects a release bundle without the voice worker", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "mjolnir-npm-voice-test-"));
  const bundle = path.join(root, "bundle");
  fs.mkdirSync(bundle);
  fs.writeFileSync(path.join(bundle, "mj"), "mj");
  fs.writeFileSync(path.join(bundle, "anvil"), "anvil");

  try {
    assert.throws(
      () =>
        createPlatformStage({
          bundleDir: bundle,
          platformKey: "linux-x64",
          stageDir: path.join(root, "stage"),
          version: "1.4.0",
        }),
      /missing required sibling binary: mj-voice-worker/,
    );
  } finally {
    fs.rmSync(root, { force: true, recursive: true });
  }
});

test("Android package does not require the unsupported voice worker", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "mjolnir-npm-android-test-"));
  const bundle = path.join(root, "bundle");
  fs.mkdirSync(bundle);
  fs.writeFileSync(path.join(bundle, "mj"), "mj");
  fs.writeFileSync(path.join(bundle, "anvil"), "anvil");

  try {
    const packageJson = createPlatformStage({
      bundleDir: bundle,
      platformKey: "android-arm64",
      stageDir: path.join(root, "stage"),
      version: "1.4.0",
    });
    assert.deepEqual(packageJson.os, ["android"]);
  } finally {
    fs.rmSync(root, { force: true, recursive: true });
  }
});

test(
  "launcher executes mj with its Anvil and voice-worker siblings",
  { skip: process.platform === "win32" },
  () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "mjolnir-npm-launcher-test-"));
    const packageRoot = path.join(root, "package");
    const selected = platformPackageFor(process.platform, process.arch);
    const nativeRoot = path.join(packageRoot, "node_modules", selected.alias);
    const vendor = path.join(nativeRoot, "vendor", selected.target);
    fs.mkdirSync(path.join(packageRoot, "bin"), { recursive: true });
    fs.mkdirSync(path.join(packageRoot, "lib"), { recursive: true });
    fs.mkdirSync(vendor, { recursive: true });
    fs.copyFileSync(path.join(NPM_DIR, "bin", "mj.js"), path.join(packageRoot, "bin", "mj.js"));
    fs.copyFileSync(
      path.join(NPM_DIR, "lib", "platform.mjs"),
      path.join(packageRoot, "lib", "platform.mjs"),
    );
    fs.writeFileSync(
      path.join(nativeRoot, "package.json"),
      JSON.stringify({
        name: "@brokkai/mjolnir",
        version: `1.4.0-${selected.npmTag}`,
      }),
    );
    fs.writeFileSync(path.join(vendor, "anvil"), "anvil");
    fs.writeFileSync(path.join(vendor, "mj-voice-worker"), "voice");
    const fakeMj = path.join(vendor, "mj");
    fs.writeFileSync(
      fakeMj,
      '#!/bin/sh\ntest -f "$(dirname "$0")/anvil" || exit 80\ntest -f "$(dirname "$0")/mj-voice-worker" || exit 81\nprintf "%s|%s\\n" "$1" "$MJOLNIR_NO_UPDATE_CHECK"\nexit 23\n',
    );
    fs.chmodSync(fakeMj, 0o755);

    try {
      const result = spawnSync(process.execPath, [path.join(packageRoot, "bin", "mj.js"), "hello"], {
        encoding: "utf8",
      });
      assert.equal(result.status, 23, result.stderr);
      assert.equal(result.stdout, "hello|true\n");
    } finally {
      fs.rmSync(root, { force: true, recursive: true });
    }
  },
);
