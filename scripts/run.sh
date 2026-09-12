#!/usr/bin/env bash
#
# Rebuild native and portable workers, then build and run the host `mj`.
# Local sessions prefer the native worker, so rebuilding only the portable
# worker can leave localhost running old code even with a fresh controller.
# Both workers use the run's profile and isolated target/worker directory.
# macOS uses its local container engine to build the portable Linux worker.
#
# Any arguments are passed through to `cargo run`, e.g.
#   scripts/run.sh -- login
#   scripts/run.sh --release -- daemon status
# A `--release` anywhere in the arguments builds the worker in release too, so
# the profiles the daemon compares still match.
#
# On Linux and macOS, a daemon running this host build stays attached.
# If Cargo replaced the executable since the daemon started, the first daemon
# connection gracefully replaces it; detached session workers remain active and
# reconnect.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

# Match the run's profile so the daemon finds a current sibling: it looks for
# the musl worker beside the controller under the same profile directory.
profile_flag=""
for arg in "$@"; do
  if [ "$arg" = "--release" ]; then
    profile_flag=--release
  fi
done

case "$(uname -s)" in
  Linux)
    case "$(uname -m)" in
      x86_64 | amd64) arch=x86_64 ;;
      aarch64 | arm64) arch=aarch64 ;;
      *)
        echo "Unsupported Linux architecture: $(uname -m)" >&2
        exit 1
        ;;
    esac
    triple="${arch}-unknown-linux-musl"
    if ! rustup target list --installed 2>/dev/null | grep -qx "$triple"; then
      echo "The $triple target is not installed. Run: rustup target add $triple" >&2
      exit 1
    fi
    cargo build --target-dir target/worker -p brokk-mj-worker --bin mj-worker ${profile_flag:+"$profile_flag"}
    cargo build --target-dir target/worker --target "$triple" -p brokk-mj-worker --bin mj-worker ${profile_flag:+"$profile_flag"}
    ;;
  Darwin)
    cargo build --target-dir target/worker -p brokk-mj-worker --bin mj-worker ${profile_flag:+"$profile_flag"}
    # The native macOS worker cannot run in a Linux container. Build through
    # the available engine before the daemon snapshots its worker sources.
    engine=""
    for candidate in docker podman; do
      if command -v "$candidate" >/dev/null 2>&1 && "$candidate" info >/dev/null 2>&1; then
        engine="$candidate"
        break
      fi
    done
    if [ -n "$engine" ]; then
      "$repo_root/scripts/build-linux-worker.sh" "$engine" ${profile_flag:+"$profile_flag"} >/dev/null
    else
      echo "No running Docker or Podman engine; built the native worker for local sessions only." >&2
    fi
    ;;
  *)
    echo "scripts/run.sh supports Linux and macOS hosts" >&2
    exit 1
    ;;
esac

export MJ_DEV_RESTART_STALE_DAEMON=1
exec cargo run -p brokk-mjolnir --bin mj "$@" 2>/dev/null
