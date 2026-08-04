#!/usr/bin/env node

import { createRequire } from "node:module";
import { realpathSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

const require = createRequire(import.meta.url);

export function platformPackageName(platform = process.platform, arch = process.arch) {
  if (platform === "darwin" && (arch === "arm64" || arch === "x64")) {
    return "@brokkai/mjolnir-darwin-universal";
  }
  if (platform === "linux" && arch === "x64") {
    return "@brokkai/mjolnir-linux-x64-gnu";
  }
  if (platform === "linux" && arch === "arm64") {
    return "@brokkai/mjolnir-linux-arm64-gnu";
  }
  if (platform === "android" && arch === "arm64") {
    return "@brokkai/mjolnir-android-arm64";
  }
  if (platform === "win32" && arch === "x64") {
    return "@brokkai/mjolnir-win32-x64";
  }
  throw new Error(`Mjolnir does not publish an npm bundle for ${platform}/${arch}.`);
}

export function resolveBundle(resolve = require.resolve, platform = process.platform, arch = process.arch) {
  const packageName = platformPackageName(platform, arch);
  try {
    return path.dirname(resolve(`${packageName}/package.json`));
  } catch (error) {
    throw new Error(
      `The ${packageName} native bundle was not installed. Reinstall @brokkai/mjolnir for ${platform}/${arch}.`,
      { cause: error },
    );
  }
}

export function nativeBinaryPath(bundleRoot, platform = process.platform) {
  return path.join(bundleRoot, "bin", platform === "win32" ? "mj.exe" : "mj");
}

const SIGNAL_EXIT_CODES = {
  SIGHUP: 129,
  SIGINT: 130,
  SIGTERM: 143,
};

export function launch(
  bundleRoot,
  args,
  platform = process.platform,
  spawnProcess = spawn,
  exitProcess = process.exit,
) {
  const bundleBin = path.join(bundleRoot, "bin");
  const child = spawnProcess(nativeBinaryPath(bundleRoot, platform), args, {
    stdio: "inherit",
    env: {
      ...process.env,
      PATH: `${bundleBin}${path.delimiter}${process.env.PATH ?? ""}`,
      // npm owns upgrades. Replacing files under node_modules would corrupt its
      // package database and can leave sibling binaries at different versions.
      MJOLNIR_NO_UPDATE_CHECK: "1",
    },
  });
  const signalHandlers = new Map();
  for (const signal of Object.keys(SIGNAL_EXIT_CODES)) {
    const handler = () => child.kill(signal);
    signalHandlers.set(signal, handler);
    process.on(signal, handler);
  }
  const removeSignalHandlers = () => {
    for (const [signal, handler] of signalHandlers) process.off(signal, handler);
  };
  child.on("error", (error) => {
    removeSignalHandlers();
    console.error(`mj: could not start native bundle: ${error.message}`);
    process.exitCode = 1;
  });
  child.on("exit", (code, signal) => {
    removeSignalHandlers();
    if (signal) {
      exitProcess(SIGNAL_EXIT_CODES[signal] ?? 1);
      return;
    }
    process.exitCode = code ?? 1;
  });
}

function main() {
  launch(resolveBundle(), process.argv.slice(2));
}

export function isMainModule(argvPath = process.argv[1], resolveRealPath = realpathSync) {
  return Boolean(
    argvPath && resolveRealPath(path.resolve(argvPath)) === fileURLToPath(import.meta.url),
  );
}

if (isMainModule()) {
  main();
}
