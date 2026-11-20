#!/usr/bin/env bash
#
# Build the Go implementation's programs into target/go/<profile>/.
#
#   scripts/go/build.sh                 # release: target/go/release/mj, mj-worker
#   scripts/go/build.sh --profile dev   # with debug assertions (the mj_debug tag)
#
# The Go programs are built without C and are static, so the same command
# cross-compiles: GOOS=linux GOARCH=arm64 scripts/go/build.sh.
#
# Like a Rust build run from Cargo's target directory, a program under
# target/ is a development build: it refuses to start, replace, stop or
# migrate the default instance. Run it with --instance <name>.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"

profile=release
while [ "$#" -gt 0 ]; do
  case "$1" in
    --profile) profile=$2; shift 2 ;;
    --profile=*) profile=${1#--profile=}; shift ;;
    *) echo "scripts/go/build.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

tags=()
case "$profile" in
  release) ;;
  dev) tags=(-tags mj_debug) ;;
  *) echo "scripts/go/build.sh: profile must be release or dev" >&2; exit 2 ;;
esac

out="target/go/$profile"
mkdir -p "$out"

if [ ! -d go/cmd ]; then
  echo "scripts/go/build.sh: no program is ported yet (go/cmd does not exist); see python3 go/port/status.py" >&2
  exit 1
fi

export CGO_ENABLED=0
for program in go/cmd/*/; do
  name=$(basename "$program")
  go build "${tags[@]}" -trimpath -o "$out/$name$(go env GOEXE)" "./$program"
  echo "$out/$name$(go env GOEXE)"
done
