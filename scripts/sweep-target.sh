#!/usr/bin/env bash
# Remove build artifacts in this checkout's target dir that no build has
# touched in the last 7 days. Cargo never garbage-collects: every distinct
# flag or feature set gets its own incremental cache and rlib generation,
# and they accumulate into hundreds of GB. Adopted from bifrost's
# pre-push gate. Pass --dry-run to list what would go.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
if ! command -v cargo-sweep >/dev/null 2>&1; then
  echo "sweep-target: cargo-sweep is not installed; run: cargo install cargo-sweep --locked" >&2
  exit 1
fi
if [ ! -d "${repo_root}/target" ]; then
  echo "sweep-target: no target dir in ${repo_root}; nothing to do"
  exit 0
fi
exec cargo sweep --time 7 "$@" "${repo_root}"
