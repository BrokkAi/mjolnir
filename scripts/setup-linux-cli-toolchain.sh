#!/usr/bin/env bash
set -euo pipefail
cargo install --locked cargo-zigbuild --version '=0.23.3'
zig_dir="${RUNNER_TEMP:-$PWD/target}/mj-ziglang"
python3 -m venv "$zig_dir"
"$zig_dir/bin/pip" install --disable-pip-version-check 'ziglang==0.15.2'
if [[ -n "${GITHUB_ENV:-}" ]]; then
  echo "CARGO_ZIGBUILD_ZIG_PATH=$zig_dir/bin/python-zig" >> "$GITHUB_ENV"
fi
printf 'For local builds, export CARGO_ZIGBUILD_ZIG_PATH=%q\n' "$zig_dir/bin/python-zig"
