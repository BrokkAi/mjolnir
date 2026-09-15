# Shared Cargo build logic for scripts/run.sh and scripts/install.sh. Source
# this file; do not execute it. Both scripts must build every binary the same
# way, or each script's artifacts invalidate the other's and consecutive runs
# recompile the whole workspace.
#
# Bash 3.2 on macOS cannot return arrays from a function, so these functions
# communicate through globals, and empty-array expansions stay guarded for
# `set -u`. Each function documents the globals it sets.
#
# The caller owns `set -euo pipefail` and the working directory.

# Read the caller's Cargo arguments and settle the profile every binary uses.
# Release is the default: these scripts produce the worker that gets uploaded
# to remote and container targets, where an unoptimized binary costs both
# transfer size and session speed. Pass `--profile dev` for a fast edit loop.
# Sets: cargo_args (the caller's arguments, plus the default profile when they
# chose none, for builds that take every argument), profile_args (the profile
# alone, for builds that take nothing else), profile_dir (the directory Cargo
# writes that profile into).
mj_parse_cargo_args() {
  cargo_args=("$@")
  profile_args=(--release)
  profile_dir=release
  local chosen=0
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --release|-r)
        chosen=1
        ;;
      --profile)
        if [ "$#" -lt 2 ]; then
          echo "--profile needs a value" >&2
          exit 2
        fi
        shift
        mj_select_profile "$1"
        chosen=1
        ;;
      --profile=*)
        mj_select_profile "${1#--profile=}"
        chosen=1
        ;;
    esac
    shift
  done
  # Cargo rejects two profile flags, so only supply the one the caller omitted.
  if [ "$chosen" = 0 ]; then
    cargo_args+=("${profile_args[@]}")
  fi
}

# Record a named profile. Cargo names the dev profile's directory `debug`.
mj_select_profile() {
  profile_args=(--profile "$1")
  if [ "$1" = dev ]; then
    profile_dir=debug
  else
    profile_dir=$1
  fi
}

# Resolve the static Linux worker target for this host.
# Sets: triple. Exits when the architecture or the rustup target is unavailable.
mj_host_musl_triple() {
  local arch
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
}

# Find a container engine that can build a Linux worker on a macOS host.
# Sets: engine, empty when neither Docker nor Podman is running.
mj_container_engine() {
  local candidate
  engine=""
  for candidate in docker podman; do
    if command -v "$candidate" >/dev/null 2>&1 && "$candidate" info >/dev/null 2>&1; then
      engine="$candidate"
      return 0
    fi
  done
}

# Build one or more binaries in a single Cargo invocation and print the path
# Cargo reports for each, one per line, in the order the pairs were given.
# Arguments: package binary [package binary ...] -- [cargo arguments].
# One invocation matters when the binaries share a target directory: Cargo
# locks each output layout for the whole build, so two invocations there run
# one after the other and each compiles the shared crates again.
mj_build_executables() {
  local packages=() binaries=()
  while [ "$#" -gt 0 ] && [ "$1" != -- ]; do
    if [ "$#" -lt 2 ]; then
      echo "mj_build_executables needs package and binary pairs" >&2
      exit 2
    fi
    packages+=("$1")
    binaries+=("$2")
    shift 2
  done
  if [ "$#" -gt 0 ]; then
    shift
  fi
  local selection=() index
  for index in "${!packages[@]}"; do
    selection+=(-p "${packages[$index]}" --bin "${binaries[$index]}")
  done
  cargo build --locked "${selection[@]}" "$@" --message-format=json-render-diagnostics |
  node --input-type=module -e '
    import { readFileSync } from "node:fs";
    const artifacts = readFileSync(0, "utf8").split("\n").filter(Boolean).map(line => JSON.parse(line));
    const lines = process.argv.slice(1).map(binary => {
      const paths = new Set(artifacts.filter(item => item.reason === "compiler-artifact" &&
        item.target?.name === binary && item.target.kind.includes("bin") && item.executable).map(item => item.executable));
      if (paths.size !== 1) throw new Error(`Cargo did not report exactly one ${binary} executable`);
      return [...paths][0];
    });
    process.stdout.write(lines.join("\n") + "\n");
  ' "${binaries[@]}"
}

# Build one binary and print the path Cargo reports for it, so the caller does
# not have to reconstruct profile and target directory names.
mj_build_executable() {
  local package=$1 binary=$2
  shift 2
  mj_build_executables "$package" "$binary" -- "$@"
}

# Build a worker into its own target directory, so controller-only changes do
# not invalidate worker artifacts. Only the profile crosses over from the
# caller's arguments, because the worker has no other build-affecting options.
# The native and cross-compiled workers use different output layouts under
# that directory, so their two invocations can run at the same time.
mj_build_worker() {
  mj_build_executable brokk-mj-worker mj-worker --target-dir target/worker "$@" ${profile_args[@]+"${profile_args[@]}"}
}

# Wait for background builds started with `&`, then fail if any of them did.
# Every build is waited for, so a failure never leaves a Cargo process running
# behind the caller's error.
mj_wait_builds() {
  local failed=0 pid
  for pid in "$@"; do
    wait "$pid" || failed=1
  done
  if [ "$failed" = 1 ]; then
    echo "A build failed; see the Cargo output above." >&2
    exit 1
  fi
}
