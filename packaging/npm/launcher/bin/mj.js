#!/usr/bin/env node
'use strict';

// Launcher for the brokk-mjolnir npm package. The native Mjolnir release
// bundle is installed by one of the platform-constrained optional
// dependencies; this script locates it, prepends the bundle directory to PATH
// so `mj` finds its `anvil` and `mj-voice-worker` siblings, and hands the
// terminal over to the native binary. npm owns the installation, so the
// in-place Mjolnir self-updater is disabled via MJOLNIR_NO_UPDATE_CHECK.

const { spawn } = require('node:child_process');
const { existsSync } = require('node:fs');
const path = require('node:path');

// Keep in sync with PLATFORMS in packaging/npm/lib.mjs; a packaging test
// asserts the two stay identical.
const NATIVE_ALIASES = {
  'darwin-x64': 'brokk-mjolnir-darwin-universal',
  'darwin-arm64': 'brokk-mjolnir-darwin-universal',
  'linux-x64': 'brokk-mjolnir-linux-x64-gnu',
  'linux-arm64': 'brokk-mjolnir-linux-arm64-gnu',
  'android-arm64': 'brokk-mjolnir-android-arm64',
  'win32-x64': 'brokk-mjolnir-win32-x64',
};

function isMusl() {
  // glibcVersionRuntime is absent on musl-based systems.
  try {
    const report = process.report.getReport();
    return !report.header.glibcVersionRuntime;
  } catch {
    return false;
  }
}

// Returns { alias } or { error } for the current platform. Exported for
// tests; `platform`, `arch`, and `musl` are injectable for the same reason.
function selectAlias(platform, arch, musl) {
  if (platform === 'linux' && musl) {
    return {
      error:
        'brokk-mjolnir ships glibc Linux builds only; musl-based systems are not supported. ' +
        'See https://mjolnir.brokk.ai/install/ for other installation methods.',
    };
  }
  const alias = NATIVE_ALIASES[`${platform}-${arch}`];
  if (!alias) {
    return {
      error:
        `brokk-mjolnir does not ship a native build for ${platform}-${arch}. ` +
        'See https://mjolnir.brokk.ai/install/ for supported platforms and alternatives.',
    };
  }
  return { alias };
}

function nativeBundleDir() {
  const override = process.env.BROKK_MJOLNIR_NATIVE_DIR;
  if (override) {
    if (!existsSync(override)) {
      fail(`BROKK_MJOLNIR_NATIVE_DIR points at a missing directory: ${override}`);
    }
    return override;
  }
  const selected = selectAlias(process.platform, process.arch, isMusl());
  if (selected.error) fail(selected.error);
  let manifest;
  try {
    manifest = require.resolve(`${selected.alias}/package.json`, {
      paths: [path.join(__dirname, '..')],
    });
  } catch {
    fail(
      `the native package ${selected.alias} is not installed. ` +
        'Optional dependencies may have been skipped (for example with ' +
        '`npm install --omit=optional`); reinstall brokk-mjolnir with optional ' +
        'dependencies enabled.',
    );
  }
  return path.dirname(manifest);
}

function fail(message) {
  process.stderr.write(`mj (npm launcher): ${message}\n`);
  process.exit(1);
}

function main() {
  const bundleDir = nativeBundleDir();
  const binaryName = process.platform === 'win32' ? 'mj.exe' : 'mj';
  const binary = path.join(bundleDir, binaryName);
  if (!existsSync(binary)) {
    fail(`native binary missing from installed package: ${binary}`);
  }

  const env = { ...process.env };
  env.PATH = bundleDir + path.delimiter + (env.PATH || '');
  env.MJOLNIR_NO_UPDATE_CHECK = '1';

  const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit', env });

  // Forward termination signals so wrappers (npx, CI) can stop the native
  // process; Ctrl-C reaches the child directly through the shared terminal.
  const forward = (signal) => {
    if (child.exitCode === null && child.signalCode === null) child.kill(signal);
  };
  for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
    try {
      process.on(signal, () => forward(signal));
    } catch {
      // Some signals cannot be listened for on Windows.
    }
  }

  child.on('error', (err) => fail(`failed to launch ${binary}: ${err.message}`));
  child.on('exit', (code, signal) => {
    if (signal) {
      // Re-raise so callers observe the same signal death as the native binary.
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

module.exports = { selectAlias, NATIVE_ALIASES };

if (require.main === module) main();
