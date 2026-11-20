#!/usr/bin/env bash
# Show the Rust changes a Go-ported crate has not caught up with yet:
#   go/scripts/rust-changes.sh mj-core [--stat]
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
crate="$1"; shift || true
base="$(awk -v c="$crate" '$1 == c {print $2}' "$root/go/PORTED_FROM")"
[ -n "$base" ] || { echo "$crate is not in go/PORTED_FROM" >&2; exit 1; }
git -C "$root" log --oneline "$base..HEAD" -- "$crate"
git -C "$root" diff "$@" "$base" HEAD -- "$crate"
