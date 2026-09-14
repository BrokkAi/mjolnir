import assert from 'node:assert/strict';
import { chmodSync, copyFileSync, existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

function fixture(os, arch) {
  const root = mkdtempSync(path.join(tmpdir(), 'mj-install-'));
  const scripts = path.join(root, 'scripts');
  const bin = path.join(root, 'bin');
  mkdirSync(path.join(scripts, 'lib'), { recursive: true });
  mkdirSync(bin);
  for (const name of ['install.sh', 'build-linux-worker.sh']) {
    copyFileSync(new URL(name, import.meta.url), path.join(scripts, name));
    chmodSync(path.join(scripts, name), 0o755);
  }
  copyFileSync(new URL('lib/build.sh', import.meta.url), path.join(scripts, 'lib', 'build.sh'));
  const tool = (name, body) => {
    writeFileSync(path.join(bin, name), `#!/bin/bash\nset -eu\n${body}\n`, { mode: 0o755 });
  };
  tool('uname', `case "$1" in -s) echo ${os} ;; -m) echo ${arch} ;; esac`);
  tool('rustup', 'echo "${INSTALLED_TARGET:-}"');
  // A built worker records its target so the test can tell which one landed
  // where. Emit the Cargo JSON artifact message consumed by install.sh.
  tool('cargo', `
    command=$1
    shift
    target_dir='' triple='' root='' path='' binary='' locked=0 profile=debug
    while [ $# -gt 0 ]; do
      case "$1" in
        --bin) binary=$2; shift ;;
        --target-dir) target_dir=$2; shift ;;
        --target) triple=$2; shift ;;
        --root) root=$2; shift ;;
        --path) path=$2; shift ;;
        --locked) locked=1 ;;
        --release|-r) profile=release ;;
        --profile) profile=$2; shift ;;
        --profile=*) profile=\${1#--profile=} ;;
      esac
      shift
    done
    test "$locked" = 1
    if [ "$profile" = dev ]; then profile=debug; fi
    if [ "$command" = build ]; then
      if [ "\${FAIL_BUILD:-0}" = 1 ] || [ "\${FAIL_TARGET:-none}" = "\${triple:-native}" ] || [ "\${FAIL_TARGET:-none}" = "$binary" ]; then exit 42; fi
      output="\${target_dir:-target}/\${triple:+$triple/}$profile"
      mkdir -p "$output"
      if [ "$binary" = mj ]; then
        printf '#!/bin/bash\necho mj test\n' > "$output/$binary"
        chmod 755 "$output/$binary"
      else
        echo "worker \${triple:-native}" > "$output/$binary"
      fi
      printf '{"reason":"compiler-artifact","target":{"name":"%s","kind":["bin"]},"executable":"%s"}\n' "$binary" "$output/$binary"
    elif [ "$command" = install ]; then
      test "$path" = mj-cli
      mkdir -p "$root/bin"
      printf '#!/bin/bash\\necho mj test\\n' > "$root/bin/mj"
      chmod 755 "$root/bin/mj"
    fi
  `);
  tool('docker', `
    if [ "$1" = info ]; then exit 0; fi
    for arg in "$@"; do if [ "$arg" = uname ]; then echo aarch64; exit 0; fi; done
    for arg in "$@"; do
      case "$arg" in
        type=bind,source=*,target=/output)
          output=\${arg#type=bind,source=}
          output=\${output%,target=/output}
          mkdir -p "$output"
          echo "worker from container" > "$output/mj-worker"
          ;;
      esac
    done
  `);
  const installRoot = path.join(root, 'cargo-root');
  const run = (env = {}, args = []) => spawnSync('/bin/bash', [path.join(scripts, 'install.sh'), ...args], {
    encoding: 'utf8',
    env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, CARGO_INSTALL_ROOT: installRoot, ...env },
  });
  return { root, installed: (name) => path.join(installRoot, 'bin', name), run };
}

test('Linux install replaces a stale native worker and installs the portable worker', () => {
  const { root, installed, run } = fixture('Linux', 'x86_64');
  try {
    mkdirSync(path.dirname(installed('mj-worker')), { recursive: true });
    writeFileSync(installed('mj-worker'), 'stale');
    const result = run({ INSTALLED_TARGET: 'x86_64-unknown-linux-musl' });
    assert.equal(readFileSync(installed('mj-worker'), 'utf8'), 'worker native\n');
    assert.equal(readFileSync(installed('mj-voice-worker'), 'utf8'), 'worker native\n');
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /mj test/);
    assert.equal(
      readFileSync(installed('mj-worker-x86_64-unknown-linux-musl'), 'utf8'),
      'worker x86_64-unknown-linux-musl\n',
    );
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('a failed worker build leaves the existing installation alone', () => {
  const { root, installed, run } = fixture('Linux', 'x86_64');
  try {
    const result = run({ INSTALLED_TARGET: 'x86_64-unknown-linux-musl', FAIL_BUILD: '1' });
    assert.notEqual(result.status, 0, result.stderr);
    assert.equal(existsSync(installed('mj')), false);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('macOS install places the native worker and the container-built Linux worker', () => {
  const { root, installed, run } = fixture('Darwin', 'arm64');
  try {
    const result = run();
    assert.equal(result.status, 0, result.stderr);
    assert.equal(readFileSync(installed('mj-worker'), 'utf8'), 'worker native\n');
    assert.equal(readFileSync(installed('mj-voice-worker'), 'utf8'), 'worker native\n');
    assert.equal(
      readFileSync(installed('mj-worker-aarch64-unknown-linux-musl'), 'utf8'),
      'worker from container\n',
    );
  } finally { rmSync(root, { recursive: true, force: true }); }
});

for (const target of ['native', 'x86_64-unknown-linux-musl', 'mj-voice-worker']) {
  test(`failed ${target} build preserves an existing installation`, () => {
    const { root, installed, run } = fixture('Linux', 'x86_64');
    try {
      mkdirSync(path.dirname(installed('mj')), { recursive: true });
      const names = ['mj', 'mj-voice-worker', 'mj-worker', 'mj-worker-x86_64-unknown-linux-musl'];
      for (const name of names) writeFileSync(installed(name), `previous ${name}`);
      const result = run({ INSTALLED_TARGET: 'x86_64-unknown-linux-musl', FAIL_TARGET: target });
      assert.notEqual(result.status, 0, result.stderr);
      for (const name of names) assert.equal(readFileSync(installed(name), 'utf8'), `previous ${name}`);
    } finally { rmSync(root, { recursive: true, force: true }); }
  });
}

// The install must land in the same profile directory scripts/run.sh uses, or
// the two scripts invalidate each other's Cargo artifacts on every run.
for (const [args, profile] of [[[], 'debug'], [['--release'], 'release'], [['--profile', 'dev'], 'debug']]) {
  test(`install with [${args}] builds every binary under target/${profile}`, () => {
    const { root, installed, run } = fixture('Linux', 'x86_64');
    try {
      const result = run({ INSTALLED_TARGET: 'x86_64-unknown-linux-musl' }, args);
      assert.equal(result.status, 0, result.stderr);
      const triple = 'x86_64-unknown-linux-musl';
      for (const built of [
        `target/worker/${profile}/mj-worker`,
        `target/worker/${triple}/${profile}/mj-worker`,
        `target/${profile}/mj-voice-worker`,
        `target/${profile}/mj`,
      ]) {
        assert.ok(existsSync(path.join(root, built)), `missing ${built}`);
      }
      assert.equal(readFileSync(installed('mj-worker'), 'utf8'), 'worker native\n');
      assert.equal(readFileSync(installed(`mj-worker-${triple}`), 'utf8'), `worker ${triple}\n`);
    } finally { rmSync(root, { recursive: true, force: true }); }
  });
}
