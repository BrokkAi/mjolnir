#!/usr/bin/env bash
#
# Install `mj` from this checkout together with its dictation helper and the
# portable worker that container and remote (SSH) sessions need.
#
# `cargo install --path mj-cli` installs only the controller. Managed targets
# then fail with "no Linux worker", because the controller looks for a static
# musl worker named `mj-worker-<target-triple>` beside its own binary, the
# layout the release archives use. This script builds the workers first, so a
# failed worker build leaves the existing installation alone, then installs
# `mj` and places the helpers beside it. Both hosts get a native worker for
# local sessions. On macOS, Docker or Podman builds the portable Linux worker.
#
# The binaries are built exactly as scripts/run.sh builds them, through the
# shared scripts/lib/build.sh, so the two scripts reuse each other's artifacts:
# `mj` and the dictation helper in the default `target/`, the workers in
# `target/worker`, all under one profile. Both default to release, because the
# installed worker is uploaded to remote and container targets. A profile flag
# applies to every binary; other arguments reach the `cargo build` for `mj` and
# the dictation helper, which share a target directory. For example:
#   scripts/install.sh
#   scripts/install.sh --profile dev
#   CARGO_INSTALL_ROOT="$HOME/.local" scripts/install.sh
#
# Cargo's install root receives the binaries: CARGO_INSTALL_ROOT, else
# CARGO_HOME, else ~/.cargo, with executables in its bin directory.
#
# Only the Linux worker for the host (or the container engine's) architecture
# is built. Targets on another architecture need the release installer, which
# bundles both.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
. "$repo_root/scripts/lib/build.sh"

install_root=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}
bin_dir="$install_root/bin"

# Match the profile across every binary, so the daemon finds workers built the
# same way as the controller.
mj_parse_cargo_args "$@"

# Built binaries and the file names they take in the install directory.
sources=()
names=()

case "$(uname -s)" in
  Linux)
    mj_host_musl_triple
    sources+=("$(mj_build_worker)")
    names+=("mj-worker")
    sources+=("$(mj_build_worker --target "$triple")")
    names+=("mj-worker-$triple")
    ;;
  Darwin)
    sources+=("$(mj_build_worker)")
    names+=("mj-worker")
    # The native macOS worker cannot run in a Linux container, so container
    # targets need a worker built through the available engine.
    mj_container_engine
    if [ -n "$engine" ]; then
      triple=$("$repo_root/scripts/build-linux-worker.sh" "$engine" ${profile_args[@]+"${profile_args[@]}"})
      sources+=("target/worker/$triple/$profile_dir/mj-worker")
      names+=("mj-worker-$triple")
    else
      echo "No running Docker or Podman engine; installing the native worker for local sessions only." >&2
    fi
    ;;
  *)
    echo "scripts/install.sh supports Linux and macOS hosts" >&2
    exit 1
    ;;
esac

# Dictation runs on the host, including when the session worker is remote. It
# shares the default target directory with `mj`, so it takes the same arguments.
sources+=("$(mj_build_executable brokk-mj-voice-worker mj-voice-worker ${cargo_args[@]+"${cargo_args[@]}"})")
names+=("mj-voice-worker")

sources+=("$(mj_build_executable brokk-mjolnir mj ${cargo_args[@]+"${cargo_args[@]}"})")
names+=("mj")

# Replace each binary by rename, so the copy never writes into an executable
# that a running session still uses.
mkdir -p "$bin_dir"
for index in "${!sources[@]}"; do
  destination="$bin_dir/${names[$index]}"
  cp "${sources[$index]}" "$destination.next"
  chmod 755 "$destination.next"
  mv -f "$destination.next" "$destination"
  echo "Installed $destination" >&2
done

"$bin_dir/mj" --version
