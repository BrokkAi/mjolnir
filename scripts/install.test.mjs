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
  mkdirSync(scripts);
  mkdirSync(bin);
  for (const name of ['install.sh', 'build-linux-worker.sh']) {
    copyFileSync(new URL(name, import.meta.url), path.join(scripts, name));
    chmodSync(path.join(scripts, name), 0o755);
  }
  const tool = (name, body) => {
    writeFileSync(path.join(bin, name), `#!/bin/bash\nset -eu\n${body}\n`, { mode: 0o755 });
  };
  tool('uname', `case "$1" in -s) echo ${os} ;; -m) echo ${arch} ;; esac`);
  tool('rustup', 'echo "${INSTALLED_TARGET:-}"');
  // A built worker records its target so the test can tell which one landed
  // where; `cargo install` accepts only the locked checkout install.
  tool('cargo', `
    command=$1
    shift
    target_dir='' triple='' root='' path='' locked=0
    while [ $# -gt 0 ]; do
      case "$1" in
        --target-dir) target_dir=$2; shift ;;
        --target) triple=$2; shift ;;
        --root) root=$2; shift ;;
        --path) path=$2; shift ;;
        --locked) locked=1 ;;
      esac
      shift
    done
    test "$locked" = 1
    if [ "$command" = build ]; then
      if [ "\${FAIL_BUILD:-0}" = 1 ]; then exit 42; fi
      output="$target_dir/\${triple:+$triple/}release"
      mkdir -p "$output"
      echo "worker \${triple:-native}" > "$output/mj-worker"
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
  const run = (env = {}) => spawnSync('/bin/bash', [path.join(scripts, 'install.sh')], {
    encoding: 'utf8',
    env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, CARGO_INSTALL_ROOT: installRoot, ...env },
  });
  return { root, installed: (name) => path.join(installRoot, 'bin', name), run };
}

test('Linux install places the host musl worker beside the installed mj', () => {
  const { root, installed, run } = fixture('Linux', 'x86_64');
  try {
    const result = run({ INSTALLED_TARGET: 'x86_64-unknown-linux-musl' });
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
    assert.equal(result.status, 42, result.stderr);
    assert.equal(existsSync(installed('mj')), false);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('macOS install places the native worker and the container-built Linux worker', () => {
  const { root, installed, run } = fixture('Darwin', 'arm64');
  try {
    const result = run();
    assert.equal(result.status, 0, result.stderr);
    assert.equal(readFileSync(installed('mj-worker'), 'utf8'), 'worker native\n');
    assert.equal(
      readFileSync(installed('mj-worker-aarch64-unknown-linux-musl'), 'utf8'),
      'worker from container\n',
    );
  } finally { rmSync(root, { recursive: true, force: true }); }
});
