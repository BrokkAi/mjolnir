#!/bin/bash
set -euo pipefail
config=${1:-${REVIEW_ARMS_ENGINE_ROOT:-/mnt/optane/review-arms-pilot}/engine.json}
repo=$(python3 -c '
import json, pathlib, sys
with open(sys.argv[1]) as f:
    print(pathlib.Path(json.load(f)["snapshot_repo"]).resolve())
' "$config")
[[ $(basename -- "$repo") == engine-trees.git ]]
root=$(dirname -- "$repo")
pidfile="$root/engine-git-daemon.pid"
exec 9>"$root/engine-git-daemon.lock"
flock -x 9
[[ -f "$pidfile" ]] || exit 0
pid=$(cat -- "$pidfile")
[[ $pid =~ ^[1-9][0-9]*$ ]]
if [[ -r /proc/$pid/cmdline ]]; then
    mapfile -d '' -t argv <"/proc/$pid/cmdline"
    if [[ "${argv[0]##*/}" != git-daemon && "${argv[1]-}" != daemon ]] ||
       ! printf '%s\n' "${argv[@]}" | rg -Fx -- "--pid-file=$pidfile" >/dev/null ||
       ! printf '%s\n' "${argv[@]}" | rg -Fx -- "--listen=127.0.0.1" >/dev/null; then
        printf 'PID %s no longer belongs to the engine Git daemon\n' "$pid" >&2
        exit 1
    fi
    # Git may fork helpers beneath the group leader created by setsid.
    read -r pgid sid < <(ps -o pgid=,sid= -p "$pid")
    if [[ ! $pgid =~ ^[1-9][0-9]*$ || $pgid != "$sid" ]]; then
        printf 'PID %s is outside the engine Git daemon session\n' "$pid" >&2
        exit 1
    fi
    kill -TERM -- "-$pgid"
fi
rm -- "$pidfile"
