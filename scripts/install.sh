#!/usr/bin/env bash
#
# Install `mj` from this checkout together with the portable worker that
# container and remote (SSH) sessions need.
#
# `cargo install --path mj-cli` installs only the controller. Managed targets
# then fail with "no Linux worker", because the controller looks for a static
# musl worker named `mj-worker-<target-triple>` beside its own binary, the
# layout the release archives use. This wrapper builds that worker first, so a
# failed worker build leaves the existing installation alone, then installs
# `mj` and places the worker beside it. macOS also gets the native worker for
# local sessions and, when Docker or Podman is running, the Linux worker built
# through that engine.
#
# Cargo's install root receives the binaries: CARGO_INSTALL_ROOT, else
# CARGO_HOME, else ~/.cargo, with executables in its bin directory. Choose a
# destination with CARGO_INSTALL_ROOT rather than --root. Any arguments are
# passed through to `cargo install`, e.g.
#   scripts/install.sh
#   CARGO_INSTALL_ROOT="$HOME/.local" scripts/install.sh
#   scripts/install.sh --force
#
# Only the Linux worker for the host (or the container engine's) architecture
# is built. Targets on another architecture need the release installer, which
# bundles both.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

install_root=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}
bin_dir="$install_root/bin"

# Built workers and the file names they take beside `mj`.
worker_sources=()
worker_names=()

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
    cargo build --release --locked --target-dir target/worker --target "$triple" -p brokk-mj-worker --bin mj-worker
    worker_sources+=("target/worker/$triple/release/mj-worker")
    worker_names+=("mj-worker-$triple")
    ;;
  Darwin)
    cargo build --release --locked --target-dir target/worker -p brokk-mj-worker --bin mj-worker
    worker_sources+=("target/worker/release/mj-worker")
    worker_names+=("mj-worker")
    # The native macOS worker cannot run in a Linux container, so container
    # targets need a worker built through the available engine.
    engine=""
    for candidate in docker podman; do
      if command -v "$candidate" >/dev/null 2>&1 && "$candidate" info >/dev/null 2>&1; then
        engine="$candidate"
        break
      fi
    done
    if [ -n "$engine" ]; then
      triple=$("$repo_root/scripts/build-linux-worker.sh" "$engine" --release)
      worker_sources+=("target/worker/$triple/release/mj-worker")
      worker_names+=("mj-worker-$triple")
    else
      echo "No running Docker or Podman engine; installing the native worker for local sessions only." >&2
    fi
    ;;
  *)
    echo "scripts/install.sh supports Linux and macOS hosts" >&2
    exit 1
    ;;
esac

cargo install --locked --path mj-cli --root "$install_root" "$@"

# Replace each worker by rename, so the copy never writes into an executable
# that a running session still uses.
for index in "${!worker_sources[@]}"; do
  destination="$bin_dir/${worker_names[$index]}"
  cp "${worker_sources[$index]}" "$destination.next"
  chmod 755 "$destination.next"
  mv -f "$destination.next" "$destination"
  echo "Installed $destination" >&2
done

"$bin_dir/mj" --version
