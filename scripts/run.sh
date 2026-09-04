#!/usr/bin/env bash
#
# Rebuild the static musl worker, then build and run the host `mj` with the
# daemon from that same host build.
#
# Plain `cargo build` targets the host (glibc) and never rebuilds the musl
# worker under target/<triple>/. A long-lived daemon then hands container and
# remote (SSH) sessions whatever musl worker was last built with an explicit
# `--target`, with no warning that it is stale. This wrapper rebuilds the musl
# worker at the same profile the run will use, so it is the "just works"
# replacement for `cargo build && cargo run` when you exercise container or
# remote sessions. Local-bare sessions run a glibc worker and do not need it,
# but building both here is cheap once warm.
#
# Any arguments are passed through to `cargo run`, e.g.
#   scripts/run.sh -- login
#   scripts/run.sh --release -- daemon status
# A `--release` anywhere in the arguments builds the worker in release too, so
# the profiles the daemon compares still match.
#
# On Linux, a daemon already running this exact host executable stays attached.
# If Cargo replaced the executable since the daemon started, the first daemon
# connection gracefully replaces it; detached session workers remain active and
# reconnect. Other hosts retain the existing protocol-version replacement.
set -euo pipefail

arch="$(uname -m)"
triple="${arch}-unknown-linux-musl"

# Match the run's profile so the daemon finds a current sibling: it looks for
# the musl worker beside the controller under the same profile directory.
profile_args=()
for arg in "$@"; do
  if [ "$arg" = "--release" ]; then
    profile_args=(--release)
  fi
done

if ! rustup target list --installed 2>/dev/null | grep -qx "$triple"; then
  echo "The $triple target is not installed. Run: rustup target add $triple" >&2
  exit 1
fi

cargo build --target "$triple" -p brokk-mjolnir --bin mj "${profile_args[@]}"
if [ -e /proc/self/exe ]; then
  export MJ_DEV_RESTART_STALE_DAEMON=1
fi
exec cargo run -p brokk-mjolnir --bin mj "$@"
