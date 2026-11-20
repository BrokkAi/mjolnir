#!/bin/bash
# Run only for the pilot. Does not start any Mjolnir session.
set -euo pipefail
config=${1:-${REVIEW_ARMS_ENGINE_ROOT:-/mnt/optane/review-arms-pilot}/engine.json}
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
export PYTHONPATH="$script_dir/../..${PYTHONPATH:+:$PYTHONPATH}"
readarray -t settings < <(python3 -c '
import json, pathlib, sys
with open(sys.argv[1]) as f:
    c = json.load(f)
print(pathlib.Path(c["snapshot_repo"]).resolve())
print(c["git_port"])
' "$config")
repo=${settings[0]}
port=${settings[1]}
[[ $(basename -- "$repo") == engine-trees.git && -f "$repo/git-daemon-export-ok" ]]
[[ $port =~ ^[0-9]{1,5}$ ]] && (( 10#$port >= 1024 && 10#$port <= 65535 ))
root=$(dirname -- "$repo")
pidfile="$root/engine-git-daemon.pid"
log="$root/engine-git-daemon.log"
exec 9>"$root/engine-git-daemon.lock"
flock -x 9
if [[ -e "$pidfile" ]]; then
    printf 'Engine Git daemon pid file already exists at %s\n' "$pidfile" >&2
    exit 1
fi
python3 -B -m rql_mock.engine verify --config "$config"
# Bind loopback only. The daemon may serve only this explicit exported path.
setsid git daemon --listen=127.0.0.1 --port="$port" --base-path="$root" \
    --pid-file="$pidfile" --strict-paths --no-informative-errors --reuseaddr --verbose \
    --enable=upload-pack --forbid-override=upload-pack \
    --disable=receive-pack --forbid-override=receive-pack \
    --disable=upload-archive --forbid-override=upload-archive "$repo" \
    </dev/null >>"$log" 2>&1 9>&- &
daemon=$!
# Confirm the actual daemon bound and can read the audited orphan repository.
until [[ -s "$pidfile" ]]; do
    if ! kill -0 "$daemon" 2>/dev/null; then
        printf 'Engine Git daemon failed to start. See %s\n' "$log" >&2
        exit 1
    fi
    sleep 0.1
done
git ls-remote --exit-code "git://127.0.0.1:$port/engine-trees.git" HEAD >/dev/null
printf 'Engine Git daemon pid %s. Log %s\n' "$(cat -- "$pidfile")" "$log"
