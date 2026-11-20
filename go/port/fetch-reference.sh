#!/usr/bin/env bash
# Fetches the Rust sources the Go port is checked against: the standard
# library of the pinned toolchain, and every crate in Cargo.lock. Nothing here
# is committed; rerun it on a new machine.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cache="${MJ_GO_PORT_CACHE:-$HOME/.cache/mjolnir/build/go-port}"
toolchain="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$root/rust-toolchain.toml")"

mkdir -p "$cache"
if [ ! -d "$cache/rust-src/src/rust/library/std" ]; then
    archive="$cache/rust-src-$toolchain.tar.xz"
    curl --fail --silent --show-error --location --output "$archive" \
        "https://static.rust-lang.org/dist/rust-src-$toolchain.tar.xz"
    rm -rf "$cache/rust-src"
    mkdir -p "$cache/rust-src"
    tar -xJf "$archive" -C "$cache/rust-src" --strip-components=4 \
        --wildcards '*/lib/rustlib/src/rust/library'
    rm -f "$archive"
fi

(cd "$root" && cargo fetch --locked >/dev/null)

echo "standard library: $cache/rust-src/src/rust/library"
echo "crates:           $(ls -d "$HOME"/.cargo/registry/src/*/ | head -1)"
