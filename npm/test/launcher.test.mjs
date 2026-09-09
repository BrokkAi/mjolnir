import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import test from "node:test";

import {
  isMainModule,
  installMethodEnvironment,
  nativeBinaryPath,
  launch,
  platformPackageName,
  resolveBundle,
} from "../launcher/mj.js";

test("recognizes npm's symlinked bin entrypoint", () => {
  let resolved;
  assert.equal(
    isMainModule("/tmp/node_modules/.bin/mj", (entrypoint) => {
      resolved = entrypoint;
      return new URL("../launcher/mj.js", import.meta.url).pathname;
    }),
    true,
  );
  assert.equal(resolved, "/tmp/node_modules/.bin/mj");
});

test("selects each published native package", () => {
  assert.equal(platformPackageName("darwin", "arm64"), "@brokkai/mjolnir-darwin-universal");
  assert.equal(platformPackageName("darwin", "x64"), "@brokkai/mjolnir-darwin-universal");
  assert.equal(platformPackageName("linux", "x64"), "@brokkai/mjolnir-linux-x64-gnu");
  assert.equal(platformPackageName("linux", "arm64"), "@brokkai/mjolnir-linux-arm64-gnu");
});

test("rejects unsupported platforms before attempting to run a binary", () => {
  assert.throws(() => platformPackageName("freebsd", "x64"), /does not publish/);
});

test("resolves the native package root from its manifest", () => {
  const bundle = resolveBundle(
    (name) => {
      assert.equal(name, "@brokkai/mjolnir-linux-x64-gnu/package.json");
      return "/tmp/node_modules/@brokkai/mjolnir-linux-x64-gnu/package.json";
    },
    "linux",
    "x64",
  );
  assert.equal(bundle, "/tmp/node_modules/@brokkai/mjolnir-linux-x64-gnu");
});

test("names the platform-native executable", () => {
  assert.equal(nativeBinaryPath("/tmp/bundle", "linux"), "/tmp/bundle/bin/mj");
  assert.equal(nativeBinaryPath("C:\\bundle", "win32"), "C:\\bundle/bin/mj.exe");
});

test("declares npm as the install method for plain npm launches", () => {
  const child = new EventEmitter();
  child.kill = () => true;
  let invocation;
  launch("/tmp/bundle", ["--version"], "linux", (binary, args, options) => {
    invocation = { binary, args, options };
    return child;
  });
  assert.equal(invocation.binary, "/tmp/bundle/bin/mj");
  assert.deepEqual(invocation.args, ["--version"]);
  assert.equal(invocation.options.stdio, "inherit");
  assert.equal(invocation.options.env.MJOLNIR_NO_UPDATE_CHECK, undefined);
  assert.ok(
    invocation.options.env.MJOLNIR_MANAGED_BY_NPM === "true" ||
      invocation.options.env.MJOLNIR_MANAGED_BY_NPX === "true",
  );
  assert.ok(invocation.options.env.PATH.startsWith(`/tmp/bundle/bin${process.platform === "win32" ? ";" : ":"}`));
});

test("names npm or npx as the install method by how the launcher was invoked", () => {
  assert.deepEqual(installMethodEnvironment({ npm_command: "exec" }), {
    MJOLNIR_MANAGED_BY_NPX: "true",
  });
  assert.deepEqual(installMethodEnvironment({ npm_command: "install" }), {
    MJOLNIR_MANAGED_BY_NPM: "true",
  });
  assert.deepEqual(installMethodEnvironment({}), {
    MJOLNIR_MANAGED_BY_NPM: "true",
  });
});

test("returns the conventional exit status when the native process is signalled", () => {
  const child = new EventEmitter();
  child.kill = () => true;
  let exitCode;
  launch(
    "/tmp/bundle",
    [],
    "linux",
    () => child,
    (code) => {
      exitCode = code;
    },
  );
  child.emit("exit", null, "SIGINT");
  assert.equal(exitCode, 130);
});
