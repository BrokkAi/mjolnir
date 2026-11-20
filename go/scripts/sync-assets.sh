#!/usr/bin/env bash
# Copy the files the Rust crates embed (include_str!/include_bytes!) into the
# Go packages that embed them with //go:embed, which cannot reach outside a
# package directory. Rerun after changing any source asset; the asset tests
# fail while the copies differ.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
copy() { # copy <source relative to repo root> <destination relative to go/>
	mkdir -p "$(dirname "$root/go/$2")"
	rm -rf "$root/go/$2"
	cp -R "$root/$1" "$root/go/$2"
}
copy mj-core/assets internal/core/assets
