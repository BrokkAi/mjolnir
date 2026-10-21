#!/usr/bin/env bash
# Build the agent-dev image from this checkout, including the session worker
# compiled from the same commit. The image's worker stage cannot see Git, so
# this computes the identities mj-core/build.rs would read from it, exactly as
# the publish workflow does, and passes them as build arguments.
#
# usage: build-agent-dev-image.sh docker-or-podman [tag] [extra build arguments...]
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
engine=${1:?usage: build-agent-dev-image.sh docker-or-podman [tag] [build arguments...]}
shift
tag=${1:-localhost/mjolnir/agent-dev:latest}
shift || true

revision=${MJ_BUILD_REVISION:-$(git -C "$repo_root" rev-parse HEAD)}
inputs=$(python3 "$repo_root/mj-core/worker_build_inputs.py" \
  --package-root "$repo_root/mj-core" --fallback-revision "$revision")
version=$(python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["workspace"]["package"]["version"])' \
  "$repo_root/Cargo.toml")
worker_build="$version+$inputs"
# The identity covers committed inputs only, like every other worker build.
if ! git -C "$repo_root" diff --quiet HEAD -- $(grep -v '^#' "$repo_root/mj-core/worker-build-inputs.txt"); then
  echo "warning: uncommitted worker changes are built but not reflected in worker build $worker_build" >&2
fi

"$engine" build \
  --file "$repo_root/containers/Containerfile.agent-dev" \
  --tag "$tag" \
  --build-arg "MJ_BUILD_REVISION=$revision" \
  --build-arg "MJ_WORKER_INPUTS_ID=$inputs" \
  --build-arg "MJ_WORKER_BUILD_ID=$worker_build" \
  "$@" \
  "$repo_root"
printf '%s\n' "$worker_build"
