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
# The binaries are built exactly as scripts/run.sh builds them, so the two
# scripts reuse each other's artifacts: `mj` and the dictation helper in the
# default `target/`, the workers in `target/worker`, all under one profile.
# The profile is release unless the arguments select another one. Arguments
# are passed to the `cargo build` for `mj`; only the profile reaches the
# worker builds, as in scripts/run.sh. For example:
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

install_root=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}
bin_dir="$install_root/bin"

# Match the profile across every binary, so the daemon finds workers built
# the same way as the controller. Release is the default for an install.
cargo_args=("$@")
profile_args=(--release)
profile_chosen=0
for ((index=0; index<${#cargo_args[@]}; index++)); do
  case "${cargo_args[index]}" in
    --release|-r) profile_args=(--release); profile_chosen=1 ;;
    --profile)
      if ((index+1 >= ${#cargo_args[@]})); then echo "--profile needs a value" >&2; exit 2; fi
      index=$((index+1))
      profile_args=(--profile "${cargo_args[index]}"); profile_chosen=1 ;;
    --profile=*) profile_args=(--profile "${cargo_args[index]#--profile=}"); profile_chosen=1 ;;
  esac
done
# The caller's own profile flag already sits in cargo_args; Cargo rejects it twice.
if [ "$profile_chosen" = 0 ]; then
  cargo_args+=("${profile_args[@]}")
fi
# Cargo names the dev profile's directory `debug`.
case "${profile_args[*]}" in
  --release) profile_dir=release ;;
  "--profile dev") profile_dir=debug ;;
  *) profile_dir=${profile_args[1]} ;;
esac

# Build one binary and print the path Cargo reports for it, so the caller
# does not have to reconstruct profile and target directory names.
build_executable() {
  local package=$1 binary=$2
  shift 2
  cargo build --locked -p "$package" --bin "$binary" "$@" --message-format=json-render-diagnostics |
  node --input-type=module -e '
    import { readFileSync } from "node:fs";
    const artifacts = readFileSync(0, "utf8").split("\n").filter(Boolean).map(line => JSON.parse(line));
    const paths = new Set(artifacts.filter(item => item.reason === "compiler-artifact" &&
      item.target?.name === process.argv[1] && item.target.kind.includes("bin") && item.executable).map(item => item.executable));
    if (paths.size !== 1) throw new Error(`Cargo did not report exactly one ${process.argv[1]} executable`);
    process.stdout.write([...paths][0]);
  ' "$binary"
}

build_worker() {
  build_executable brokk-mj-worker mj-worker --target-dir target/worker "$@" ${profile_args[@]+"${profile_args[@]}"}
}

# Built binaries and the file names they take in the install directory.
sources=()
names=()

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
    sources+=("$(build_worker)")
    names+=("mj-worker")
    sources+=("$(build_worker --target "$triple")")
    names+=("mj-worker-$triple")
    ;;
  Darwin)
    sources+=("$(build_worker)")
    names+=("mj-worker")
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

# Dictation runs on the host, including when the session worker is remote.
sources+=("$(build_executable brokk-mj-voice-worker mj-voice-worker ${profile_args[@]+"${profile_args[@]}"})")
names+=("mj-voice-worker")

sources+=("$(build_executable brokk-mjolnir mj "${cargo_args[@]}")")
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
