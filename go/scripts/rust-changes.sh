#!/usr/bin/env bash
# List the Rust commits still to replay into the Go build, oldest first:
# those after go/RUST_TARGET's replayed-through commit, up to its target,
# that touch a crate already ported to Go, minus the already-applied ones.
#   go/scripts/rust-changes.sh            # one line per commit
#   go/scripts/rust-changes.sh --count    # how many remain
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
file="$root/go/RUST_TARGET"
field() { awk -v k="$1" '$1 == k {for (i = 2; i <= NF; i++) printf "%s%s", $i, (i < NF ? " " : "\n")}' "$file"; }
target="$(field target | awk '{print $1}')"
from="$(field replayed-through)"
applied=" $(field already-applied) "
ported=()
for crate in "$root"/mj-* "$root"/voice-worker; do
	name="$(basename "$crate")"
	package="${name#mj-}"; package="${package//-/}"
	[ "$name" = mj-import ] && package=mjimport
	[ "$name" = mj-worker-bin ] && package=workerbin
	[ "$name" = voice-worker ] && package=voiceworker
	if [ -d "$root/go/internal/$package" ] || [ -d "$root/go/cmd/$name" ]; then
		ported+=("$name")
	fi
done
list=$(git -C "$root" log --reverse --no-merges --format='%h %s' "$from..$target" -- "${ported[@]}" |
	while read -r sha subject; do
		case "$applied" in *" ${sha:0:9} "*) continue ;; esac
		printf '%s %s\n' "$sha" "$subject"
	done)
if [ "${1:-}" = --count ]; then printf '%s\n' "$list" | grep -c . || true; else printf '%s\n' "$list"; fi
