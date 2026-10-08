import assert from "node:assert/strict";
import test from "node:test";

import {
  PLATFORMS,
  platformManifest,
  rootManifest,
  versionFromTag,
  stagePlatform,
} from "../scripts/package-release.mjs";

test("declares every release target exactly once", () => {
  assert.deepEqual(
    PLATFORMS.map((platform) => platform.packageName),
    [
      "@brokkai/mjolnir-darwin-x64",
      "@brokkai/mjolnir-darwin-arm64",
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


test("stages each platform's declared session workers and nothing else", async (t) => {
  const { mkdtemp, mkdir, writeFile, readFile, readdir, stat, rm } = await import("node:fs/promises");
  const { tmpdir } = await import("node:os");
  const path = await import("node:path");
  const root = await mkdtemp(path.join(tmpdir(), "mj-npm-workers-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = path.join(root, "source");
  await mkdir(path.join(source, "licenses"), { recursive: true });
  const workers = [...new Set(PLATFORMS.flatMap((platform) => platform.sessionWorkers))];
  for (const name of ["README.md", "LICENSE", "mj", "mj-worker", "mj-desktop", "mj-voice-worker", ...workers]) {
    await writeFile(path.join(source, name), name, { mode: 0o755 });
  }
  for (const platform of PLATFORMS) {
    const staged = await stagePlatform(platform, "1.2.3", source, path.join(root, "stage"));
    for (const worker of platform.sessionWorkers) {
      const filename = path.join(staged, "bin", worker);
      assert.equal(await readFile(filename, "utf8"), worker);
      assert.ok((await stat(filename)).mode & 0o111);
    }
    // A macOS package carrying the other architecture's Darwin worker is the
    // regression that pushed the tarball past the registry limit.
    const stagedWorkers = (await readdir(path.join(staged, "bin")))
      .filter((name) => name.startsWith("mj-worker-"))
      .sort();
    assert.deepEqual(stagedWorkers, [...platform.sessionWorkers].sort(), platform.packageName);
  }
});
