#!/usr/bin/env bash
# Build the current checkout's portable worker using the standard agent image,
# which includes the pinned Rust toolchain, musl target, and native build tools.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
engine=${1:?usage: build-linux-worker.sh docker-or-podman [--release]}
shift
profile=debug
profile_flag=""
if [ "${1:-}" = --release ]; then
  profile=release
  profile_flag=--release
  shift
fi
if [ "$#" -ne 0 ]; then
  echo "usage: build-linux-worker.sh docker-or-podman [--release]" >&2
  exit 2
fi

image=ghcr.io/brokkai/mjolnir/agent-dev:latest
arch=$("$engine" run --rm --entrypoint uname "$image" -m)
case "$arch" in
  aarch64 | arm64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) echo "Unsupported container architecture: $arch" >&2; exit 1 ;;
esac
triple="${arch}-unknown-linux-musl"
output="$repo_root/target/worker/$triple/$profile"
repo_key=$(printf '%s' "$repo_root" | cksum | cut -d ' ' -f 1)
mkdir -p "$output"

# Keep Linux build caches on a container volume, separate from macOS Cargo
# artifacts. Only the completed executable crosses the host filesystem mount.
"$engine" run --rm --init --user 0 --entrypoint sh \
  --mount "type=bind,source=$repo_root,target=/source,readonly" \
  --mount "type=bind,source=$output,target=/output" \
  --mount "type=volume,source=mjolnir-dev-worker-$arch-$repo_key,target=/build" \
  --mount "type=volume,source=mjolnir-dev-cargo-registry,target=/usr/local/cargo/registry" \
  --mount "type=volume,source=mjolnir-dev-cargo-git,target=/usr/local/cargo/git" \
  --workdir /source "$image" -eu -c '
    triple=$1
    profile=$2
    shift 2
    export CC_aarch64_unknown_linux_musl=musl-gcc
    export CC_x86_64_unknown_linux_musl=musl-gcc
    cargo build --locked --target-dir /build --target "$triple" -p brokk-mj-worker --bin mj-worker "$@"
    if cmp -s "/build/$triple/$profile/mj-worker" /output/mj-worker; then
      /output/mj-worker --version >&2
      exit 0
    fi
    cp "/build/$triple/$profile/mj-worker" /output/mj-worker.next
    chmod 755 /output/mj-worker.next
    /output/mj-worker.next --version >&2
    mv -f /output/mj-worker.next /output/mj-worker
  ' sh "$triple" "$profile" ${profile_flag:+"$profile_flag"}
