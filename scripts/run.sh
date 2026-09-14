#!/usr/bin/env bash
#
# Rebuild native and portable workers and the dictation helper, then run `mj`.
# Local sessions prefer the native worker, so rebuilding only the portable
# worker can leave localhost running old code even with a fresh controller.
# Both workers use the run's profile and isolated target/worker directory.
# macOS uses its local container engine to build the portable Linux worker.
#
# Cargo arguments precede `--`; application arguments follow it, e.g.
#   scripts/run.sh -- login
#   scripts/run.sh --profile dev -- daemon status
# The profile applies to every binary, so the profiles the daemon compares
# still match. Release is the default, because these workers are uploaded to
# remote and container targets; `--profile dev` trades that for link speed.
#
# scripts/install.sh builds the same binaries the same way through
# scripts/lib/build.sh, with the same default, so the two scripts reuse each
# other's Cargo artifacts as long as their profiles match.
#
# On Linux and macOS, a daemon running this host build stays attached.
# If Cargo replaced the executable since the daemon started, the first daemon
# connection gracefully replaces it; detached session workers remain active and
# reconnect.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
. "$repo_root/scripts/lib/build.sh"

build_args=()
app_args=()
while [ "$#" -gt 0 ]; do
  if [ "$1" = -- ]; then
    shift
    app_args=("$@")
    break
  fi
  build_args+=("$1")
  shift
done
# Match the run's profile so the daemon finds a current sibling: it looks for
# the musl worker beside the controller under the same profile directory.
# Empty-array expansions here also support macOS Bash 3.2 with nounset.
mj_parse_cargo_args ${build_args[@]+"${build_args[@]}"}

case "$(uname -s)" in
  Linux)
    mj_host_musl_triple
    mj_build_worker >/dev/null
    mj_build_worker --target "$triple" >/dev/null
    ;;
  Darwin)
    mj_build_worker >/dev/null
    # The native macOS worker cannot run in a Linux container. Build through
    # the available engine before the daemon snapshots its worker sources.
    mj_container_engine
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
MJ_VOICE_WORKER=$(mj_build_executable brokk-mj-voice-worker mj-voice-worker ${cargo_args[@]+"${cargo_args[@]}"})
export MJ_VOICE_WORKER
executable=$(mj_build_executable brokk-mjolnir mj ${cargo_args[@]+"${cargo_args[@]}"})
exec "$executable" ${app_args[@]+"${app_args[@]}"}
