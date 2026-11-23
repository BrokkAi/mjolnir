#!/usr/bin/env bash
# Show the Rust changes a Go-ported crate has not caught up with yet:
#   go/scripts/rust-changes.sh mj-core [--stat]
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
crate="$1"; shift || true
base="$(awk -v c="$crate" '$1 == c {print $2}' "$root/go/PORTED_FROM")"
[ -n "$base" ] || { echo "$crate is not in go/PORTED_FROM" >&2; exit 1; }
# Where a crate's files lived when it was ported, for crates Rust has since
# extracted, so the diff shows their changes as renames.
moved_from=()
case "$crate" in
mj-harness-install)
	moved_from=(mj-core/src/harness_install.rs mj-core/src/login_environment.rs
		mj-core/src/program_path.rs mj-core/src/session_image.rs
		mj-core/assets/antigravity mj-core/assets/grok mj-core/assets/harnesses
		mj-core/assets/kimi mj-core/assets/mise mj-core/assets/muse
		mj-core/assets/node mj-core/assets/opencode
		mj-core/tests/fixtures/bridge-version.sh mj-core/tests/fixtures/login-shell.sh) ;;
mj-microvm)
	moved_from=(mj-worker/src/microvm mj-worker/tests/microvm_reaper.rs) ;;
esac
git -C "$root" log --oneline "$base..HEAD" -- "$crate" ${moved_from[@]+"${moved_from[@]}"}
git -C "$root" diff -M "$@" "$base" HEAD -- "$crate" ${moved_from[@]+"${moved_from[@]}"}
