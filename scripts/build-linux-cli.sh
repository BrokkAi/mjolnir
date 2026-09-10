#!/usr/bin/env bash
set -euo pipefail
target="${1:?usage: build-linux-cli.sh GNU_TARGET_TRIPLE}"
case "$target" in
  x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu) ;;
  *) echo "Unsupported Linux CLI target: $target" >&2; exit 1 ;;
esac
cargo zigbuild --release --locked -p brokk-mjolnir --bin mj \
  --target "$target.2.28" --target-dir target/release-cli
bash scripts/verify-linux-release-elf.sh "target/release-cli/$target/release/mj" "$target"
