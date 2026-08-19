import assert from "node:assert/strict";
import test from "node:test";

import {
  PLATFORMS,
  platformManifest,
  rootManifest,
  versionFromTag,
} from "../scripts/package-release.mjs";

test("declares every release target exactly once", () => {
  assert.deepEqual(
    PLATFORMS.map((platform) => platform.packageName),
    [
      "@brokkai/mjolnir-darwin-universal",
      "@brokkai/mjolnir-linux-x64-gnu",
      "@brokkai/mjolnir-linux-arm64-gnu",
      "@brokkai/mjolnir-android-arm64",
      "@brokkai/mjolnir-win32-x64",
    ],
  );
  assert.equal(new Set(PLATFORMS.map((platform) => platform.target)).size, PLATFORMS.length);
  assert.equal(PLATFORMS.filter((platform) => !platform.desktop).length, 1);
});

test("derives publishable package versions from the release tag", () => {
  assert.equal(versionFromTag("v1.5.2"), "1.5.2");
  assert.throws(() => versionFromTag("1.5.2"), /vX.Y.Z/);
});

test("generates exact platform dependency versions", () => {
  const manifest = rootManifest("1.5.2");
  assert.equal(manifest.version, "1.5.2");
  assert.deepEqual(
    Object.values(manifest.optionalDependencies),
    PLATFORMS.map(() => "1.5.2"),
  );
});

test("generates platform constraints without committed manifests", () => {
  const linux = PLATFORMS.find((platform) => platform.target === "x86_64-unknown-linux-gnu");
  const manifest = platformManifest(linux, "1.5.2");
  assert.equal(manifest.name, "@brokkai/mjolnir-linux-x64-gnu");
  assert.deepEqual(manifest.os, ["linux"]);
  assert.deepEqual(manifest.cpu, ["x64"]);
  assert.deepEqual(manifest.libc, ["glibc"]);

  const android = PLATFORMS.find((platform) => platform.target === "aarch64-linux-android");
  assert.deepEqual(android.sidecars, ["anvil"]);
  assert.ok(PLATFORMS.filter((platform) => platform !== android).every((platform) => !platform.sidecars));
});
