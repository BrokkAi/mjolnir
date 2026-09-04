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
    ],
  );
  assert.equal(new Set(PLATFORMS.map((platform) => platform.target)).size, PLATFORMS.length);
  assert.equal(PLATFORMS.filter((platform) => !platform.desktop).length, 0);
});

test("derives publishable package versions from the release tag", () => {
  assert.equal(versionFromTag("v1.5.2"), "1.5.2");
  assert.equal(versionFromTag("v2.0.0-rc.1"), "2.0.0-rc.1");
  assert.throws(() => versionFromTag("1.5.2"), /vX.Y.Z/);
  assert.throws(() => versionFromTag("v2.0.0-"), /vX.Y.Z/);
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
});
