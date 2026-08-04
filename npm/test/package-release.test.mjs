import assert from "node:assert/strict";
import test from "node:test";

import { PLATFORMS } from "../scripts/package-release.mjs";

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
