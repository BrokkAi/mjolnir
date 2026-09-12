#!/usr/bin/env bash
#
# Rebuild native and portable workers, then build and run the host `mj`.
# Local sessions prefer the native worker, so rebuilding only the portable
# worker can leave localhost running old code even with a fresh controller.
# Both workers use the run's profile and isolated target/worker directory.
# macOS uses its local container engine to build the portable Linux worker.
#
# Cargo arguments precede `--`; application arguments follow it, e.g.
#   scripts/run.sh -- login
#   scripts/run.sh --release -- daemon status
# A `--release` before `--` builds the worker in release too, so
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
cargo_args=()
cargo_arg_count=0
app_args=()
while [ "$#" -gt 0 ]; do
  if [ "$1" = -- ]; then
    shift
    app_args=("$@")
    break
  fi
  cargo_args+=("$1")
  cargo_arg_count=$((cargo_arg_count+1))
  shift
done
# Empty-array expansions below also support macOS Bash 3.2 with nounset.
profile_args=()
for ((index=0; index<cargo_arg_count; index++)); do
  case "${cargo_args[index]}" in
    --release|-r) profile_args=(--release) ;;
    --profile)
      if ((index+1 >= cargo_arg_count)); then echo "--profile needs a value" >&2; exit 2; fi
      index=$((index+1))
      profile_args=(--profile "${cargo_args[index]}") ;;
    --profile=*) profile_args=(--profile "${cargo_args[index]#--profile=}") ;;
  esac
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
    cargo build --target-dir target/worker -p brokk-mj-worker --bin mj-worker ${profile_args[@]+"${profile_args[@]}"}
    cargo build --target-dir target/worker --target "$triple" -p brokk-mj-worker --bin mj-worker ${profile_args[@]+"${profile_args[@]}"}
    ;;
  Darwin)
    cargo build --target-dir target/worker -p brokk-mj-worker --bin mj-worker ${profile_args[@]+"${profile_args[@]}"}
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
      "$repo_root/scripts/build-linux-worker.sh" "$engine" ${profile_args[@]+"${profile_args[@]}"} >/dev/null
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
executable=$(cargo build -p brokk-mjolnir --bin mj ${cargo_args[@]+"${cargo_args[@]}"} --message-format=json-render-diagnostics |
  node --input-type=module -e '
    import { readFileSync } from "node:fs";
    const artifacts = readFileSync(0, "utf8").split("\n").filter(Boolean).map(line => JSON.parse(line));
    const paths = new Set(artifacts.filter(item => item.reason === "compiler-artifact" &&
      item.target?.name === "mj" && item.target.kind.includes("bin") && item.executable).map(item => item.executable));
    if (paths.size !== 1) throw new Error("Cargo did not report exactly one mj executable");
    process.stdout.write([...paths][0]);
  ')
exec "$executable" ${app_args[@]+"${app_args[@]}"}
