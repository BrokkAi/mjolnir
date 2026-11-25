#!/usr/bin/env bash
# Usage: build-session-base-image.sh docker-or-podman [tag] [platform] [base]
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
engine=${1:?usage: build-session-base-image.sh docker-or-podman [tag] [platform] [base]}
tag=${2:-localhost/mjolnir/session-base:dev}
platform=${3:-linux/amd64}
base=${4:-docker.io/library/buildpack-deps:noble}

"$engine" pull --platform "$platform" "$base"
metadata=$("$engine" image inspect "$base")
field_lines=$(python3 -c '
import json,sys
image=json.load(sys.stdin)[0]
digest=image.get("Digest") or image["RepoDigests"][0].split("@",1)[1]
config=image.get("Config") or {}
print(sys.argv[1].split("@",1)[0]+"@"+digest)
print(config.get("User") or "")
print(config.get("WorkingDir") or "/")
' "$base" <<< "$metadata")
# read preserves the empty User line without splitting or dropping fields.
{
  IFS= read -r reference
  IFS= read -r user
  IFS= read -r workdir
} <<< "$field_lines"

if [[ -n "${MJ_BIN:-}" ]]; then
  mj_command=("$MJ_BIN")
elif command -v mbx >/dev/null 2>&1; then
  mj_command=(mbx run --manifest-path "$repo_root/Cargo.toml" -p brokk-mjolnir --bin mj --)
else
  mj_command=(cargo run --quiet --manifest-path "$repo_root/Cargo.toml" -p brokk-mjolnir --bin mj --)
fi
if [[ "${engine##*/}" == docker ]]; then
  build_input=(-)
else
  build_input=(--file -)
fi
"${mj_command[@]}" image base --base "$reference" --platform "$platform" --user "$user" --workdir "$workdir" |
  "$engine" build --platform "$platform" --tag "$tag" "${build_input[@]}"
