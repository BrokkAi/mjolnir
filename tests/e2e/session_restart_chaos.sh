#!/usr/bin/env bash
set -euo pipefail

if [[ ${MJ_CHAOS_ISOLATED:-} != 1 ]]; then
    echo "refusing to signal processes outside an explicitly isolated chaos container" >&2
    echo "set MJ_CHAOS_ISOLATED=1 only inside a disposable Podman container" >&2
    exit 2
fi
if [[ $# -ne 1 ]]; then
    echo "usage: $0 /path/to/hel" >&2
    exit 2
fi

hel_input=$1
[[ -x $hel_input ]] || { echo "hel binary is not executable: $hel_input" >&2; exit 2; }
hel_binary=$(cd -- "$(dirname -- "$hel_input")" && pwd)/$(basename -- "$hel_input")
command -v python3 >/dev/null
command -v timeout >/dev/null

chaos_root=$(mktemp -d)
artifact_dir=${MJ_CHAOS_ARTIFACT_DIR:-}
worker_root=$chaos_root/worker
workspace=$chaos_root/workspace
profile=$chaos_root/profile
memory=$chaos_root/memory
baseline=$chaos_root/baseline
bridge_script=$chaos_root/bridge.py
launch_config=$chaos_root/worker.json
worker_log=$chaos_root/worker.log
proxy_log=$chaos_root/proxy.log
worker_pid=
supervisor=

cleanup() {
    if [[ -n ${worker_pid:-} ]] && kill -0 "$worker_pid" 2>/dev/null; then
        kill -TERM "$worker_pid" 2>/dev/null || true
        wait "$worker_pid" 2>/dev/null || true
    fi
    if [[ -n $artifact_dir ]]; then
        mkdir -p "$artifact_dir"
        while IFS= read -r -d '' evidence; do
            relative=${evidence#"$chaos_root/"}
            mkdir -p "$artifact_dir/$(dirname -- "$relative")"
            cp -a "$evidence" "$artifact_dir/$relative"
        done < <(find "$chaos_root" -type f -print0)
        ps -eo pid=,ppid=,pgid=,sid=,stat=,etimes=,args= >"$artifact_dir/process-tree.txt"
    fi
}
trap cleanup EXIT

mkdir -p "$worker_root" "$workspace" "$profile" "$memory" "$baseline"

cat >"$bridge_script" <<'PY'
import json
import os
import subprocess
import sys
import traceback

state = os.environ["MJ_CHAOS_STATE"]
hel = os.environ["MJ_CHAOS_BINARY"]
memory_root = os.environ["MJ_CHAOS_MEMORY"]
bridge_log = os.path.join(state, "bridge.log")

def log(message):
    with open(bridge_log, "a", encoding="utf-8") as output:
        output.write(message + "\n")

def report_exception(kind, value, trace):
    with open(bridge_log, "a", encoding="utf-8") as output:
        traceback.print_exception(kind, value, trace, file=output)

sys.excepthook = report_exception

def record(name, pid):
    with open(os.path.join(state, name), "w", encoding="utf-8") as output:
        output.write(str(pid))

record("bridge.pid", os.getpid())
log("bridge started pid=" + str(os.getpid()))
provider = subprocess.Popen(["sleep", "300"])
record("provider.pid", provider.pid)
memory = subprocess.Popen(
    [hel, "worker", "memory-mcp", "--root", memory_root],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL,
)
record("memory.pid", memory.pid)

def reply(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

try:
    for line in sys.stdin:
        log("request " + line.rstrip())
        request = json.loads(line)
        method = request.get("method")
        ident = request.get("id")
        if method == "initialize":
            reply({"jsonrpc": "2.0", "id": ident, "result": {"protocolVersion": 1}})
        elif method in ("session/new", "session/load"):
            reply({"jsonrpc": "2.0", "id": ident, "result": {"sessionId": "chaos-native"}})
        elif ident is not None:
            reply({"jsonrpc": "2.0", "id": ident, "result": {}})
finally:
    for child in (memory, provider):
        if child.poll() is None:
            child.terminate()
    for child in (memory, provider):
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
    log("bridge stopped")
PY

python3 - "$launch_config" "$bridge_script" "$workspace" "$profile" "$memory" "$baseline" "$chaos_root" "$hel_binary" <<'PY'
import json
import sys

config, bridge, workspace, profile, memory, baseline, state, hel = sys.argv[1:]
with open(config, "w", encoding="utf-8") as output:
    json.dump({
        "session_id": "018f9dd2-a3b4-7c8d-9000-chaos000001",
        "harness": "kimi",
        "bridge_command": "python3",
        "bridge_args": [bridge],
        "environment": {
            "KIMI_HOME": profile,
            "MJ_CHAOS_STATE": state,
            "MJ_CHAOS_BINARY": hel,
            "MJ_CHAOS_MEMORY": memory,
        },
        "cwd": workspace,
        "additional_directories": [],
        "native_session_id": None,
        "project_memory": {
            "project_key": "chaos",
            "root": memory,
            "baseline_root": baseline,
            "repository_roots": {},
            "mcp_delivery": "acp",
        },
        "execution_policy": "configured_approvals",
    }, output)
PY

require_pid() {
    local pid=$1 expected=$2
    [[ $pid =~ ^[0-9]+$ ]] || { echo "invalid $expected pid: $pid" >&2; exit 1; }
    kill -0 "$pid" 2>/dev/null || { echo "$expected pid is not alive: $pid" >&2; exit 1; }
}

wait_for_file_pid() {
    local path=$1 expected=$2 previous=${3:-}
    local pid=
    for _ in $(seq 1 250); do
        pid=$(cat "$path" 2>/dev/null || true)
        if [[ $pid =~ ^[0-9]+$ ]] && [[ $pid != "$previous" ]] && kill -0 "$pid" 2>/dev/null; then
            printf '%s\n' "$pid"
            return 0
        fi
        sleep 0.02
    done
    echo "timed out waiting for $expected pid in $path" >&2
    ps -eo pid=,ppid=,pgid=,sid=,stat=,wchan=,args= >&2
    echo "pid files: bridge=$(cat "$chaos_root/bridge.pid" 2>/dev/null || true) supervisor=$supervisor worker=$worker_pid" >&2
    tail -n 80 "$worker_log" >&2
    tail -n 80 "$chaos_root/bridge.log" >&2 || true
    return 1
}

supervisor_pid() {
    local pid parent command
    for _ in $(seq 1 250); do
        while read -r pid parent command; do
            if [[ $parent == "$worker_pid" && $command == *"worker acp-supervisor"* ]]; then
                printf '%s\n' "$pid"
                return 0
            fi
        done < <(ps -eo pid=,ppid=,args=)
        sleep 0.02
    done
    echo "timed out waiting for ACP supervisor child of $worker_pid" >&2
    ps -eo pid=,ppid=,pgid=,sid=,args= >&2
    tail -n 80 "$worker_log" >&2
    tail -n 80 "$chaos_root/bridge.log" >&2 || true
    return 1
}

start_worker() {
    "$hel_binary" worker run --root "$worker_root" --config "$launch_config" >>"$worker_log" 2>&1 &
    worker_pid=$!
    require_pid "$worker_pid" worker
    for _ in $(seq 1 250); do
        [[ -S $worker_root/control.sock ]] && return 0
        kill -0 "$worker_pid" 2>/dev/null || {
            echo "worker stopped during startup" >&2
            tail -n 80 "$worker_log" >&2
            return 1
        }
        sleep 0.02
    done
    echo "timed out waiting for worker socket" >&2
    return 1
}

attach_response() {
    printf '%s\n' '{"request_id":"chaos-attach","protocol_version":5,"request":{"method":"attach","params":{"after_ordinal":0,"after_digest":"0000000000000000000000000000000000000000000000000000000000000000"}}}' |
        timeout 5 "$hel_binary" worker proxy --root "$worker_root" 2>>"$proxy_log"
}

marker_count() {
    attach_response | python3 -c '
import json, sys
response = json.loads(sys.stdin.readline())
events = response["payload"]["data"]["events"]
print(sum(event["observation"].get("type") == "session_restarted" for event in events))
'
}

wait_for_markers() {
    local expected=$1 actual=
    for _ in $(seq 1 25); do
        actual=$(marker_count 2>/dev/null || true)
        [[ $actual == "$expected" ]] && return 0
        sleep 0.2
    done
    echo "expected $expected restart markers, found ${actual:-unreadable}" >&2
    ps -eo pid=,ppid=,pgid=,sid=,stat=,wchan=,args= >&2
    echo "pid files: bridge=$(cat "$chaos_root/bridge.pid" 2>/dev/null || true) supervisor=$supervisor worker=$worker_pid" >&2
    attach_response >&2 || true
    tail -n 80 "$worker_log" >&2
    tail -n 80 "$chaos_root/bridge.log" >&2 || true
    return 1
}

assert_markers() {
    local expected=$1 actual
    actual=$(marker_count)
    [[ $actual == "$expected" ]] || {
        echo "expected $expected restart markers, found $actual" >&2
        tail -n 80 "$worker_log" >&2
        tail -n 80 "$chaos_root/bridge.log" >&2 || true
        exit 1
    }
}

kill_exact() {
    local label=$1 pid=$2 signal=$3 expected=$4
    require_pid "$pid" "$label"
    echo "chaos: $signal $label pid=$pid; expected markers=$expected"
    kill -"$signal" "$pid"
    for _ in $(seq 1 100); do
        state=$(awk '{ print $3 }' "/proc/$pid/stat" 2>/dev/null || true)
        if [[ -z $state || $state == Z ]]; then
            echo "chaos: $label pid=$pid exited"
            return 0
        fi
        sleep 0.01
    done
    echo "$label pid=$pid still exists after $signal" >&2
    return 1
}

start_worker
bridge_pid=$(wait_for_file_pid "$chaos_root/bridge.pid" bridge)
memory_pid=$(wait_for_file_pid "$chaos_root/memory.pid" memory)
provider_pid=$(wait_for_file_pid "$chaos_root/provider.pid" provider)
supervisor=$(supervisor_pid)
echo "chaos: topology worker=$worker_pid supervisor=$supervisor bridge=$bridge_pid memory=$memory_pid provider=$provider_pid"
assert_markers 0

# A proxy belongs only to one controller transport. Destroying it must not
# claim that the target-side session restarted.
proxy_input=$chaos_root/proxy-input
mkfifo "$proxy_input"
exec {proxy_input_fd}<>"$proxy_input"
"$hel_binary" worker proxy --root "$worker_root" <&$proxy_input_fd >"$chaos_root/held-proxy.out" 2>>"$proxy_log" &
proxy_pid=$!
require_pid "$proxy_pid" proxy
printf '%s\n' '{"request_id":"held-proxy","protocol_version":5,"request":{"method":"status"}}' >&$proxy_input_fd
kill_exact proxy "$proxy_pid" KILL 0
wait "$proxy_pid" 2>/dev/null || true
exec {proxy_input_fd}>&-
assert_markers 0

kill_exact memory-mcp "$memory_pid" KILL 0
assert_markers 0
kill_exact provider-child "$provider_pid" TERM 0
assert_markers 0

kill_exact acp-bridge "$bridge_pid" TERM 1
wait_for_markers 1
bridge_pid=$(wait_for_file_pid "$chaos_root/bridge.pid" bridge "$bridge_pid")
kill_exact acp-bridge "$bridge_pid" KILL 2
wait_for_markers 2
bridge_pid=$(wait_for_file_pid "$chaos_root/bridge.pid" bridge "$bridge_pid")

# Let the resumed session clear the intentional rapid-death fuse before
# exercising the independent supervisor boundary.
sleep 5.1
supervisor=$(supervisor_pid)
kill_exact acp-supervisor "$supervisor" KILL 3
wait_for_markers 3
wait_for_file_pid "$chaos_root/bridge.pid" bridge "$bridge_pid" >/dev/null

kill_exact worker "$worker_pid" TERM 4
wait "$worker_pid" 2>/dev/null || true
old_worker=$worker_pid
start_worker
[[ $worker_pid != "$old_worker" ]]
wait_for_markers 4

kill_exact worker "$worker_pid" KILL 5
wait "$worker_pid" 2>/dev/null || true
old_worker=$worker_pid
start_worker
[[ $worker_pid != "$old_worker" ]]
wait_for_markers 5

# A managed harness version must stay leased by the ACP supervisor after its
# worker-side lease disappears. Otherwise a quiet upgrade can garbage-collect
# the executable while an hours-long busy turn is still running.
lease_root=$chaos_root/managed-harness
lease_path=$lease_root/.lease
lease_spec=$lease_root/supervisor.json
lease_fifo=$lease_root/parent-input
lease_output_fifo=$lease_root/parent-output
mkdir -p "$lease_root"
: >"$lease_path"
mkfifo "$lease_fifo"
mkfifo "$lease_output_fifo"
python3 - "$lease_spec" "$lease_path" "$workspace" <<'PY'
import json
import sys

spec, lease, workspace = sys.argv[1:]
with open(spec, "w", encoding="utf-8") as output:
    json.dump({
        "command": "/bin/sh",
        "args": ["-c", "sleep 300"],
        "environment": {},
        "cwd": workspace,
        "harness_lease": lease,
    }, output)
PY

lease_state() {
    python3 - "$lease_path" <<'PY'
import fcntl
import sys

with open(sys.argv[1], "r+", encoding="utf-8") as lease:
    try:
        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        print("locked")
    else:
        print("unlocked")
PY
}

cat "$lease_output_fifo" >"$lease_root/supervisor.out" &
lease_output_pid=$!
"$hel_binary" worker acp-supervisor --spec "$lease_spec" <"$lease_fifo" \
    >"$lease_output_fifo" 2>"$lease_root/supervisor.log" &
lease_supervisor=$!
exec {lease_input_fd}>"$lease_fifo"
for _ in $(seq 1 250); do
    [[ $(lease_state) == locked ]] && break
    sleep 0.02
done
[[ $(lease_state) == locked ]] || {
    echo "ACP supervisor did not acquire the managed harness lease" >&2
    exit 1
}
echo "chaos: ACP supervisor holds managed harness lease independently"
exec {lease_input_fd}>&-
for _ in $(seq 1 250); do
    kill -0 "$lease_supervisor" 2>/dev/null || break
    sleep 0.02
done
kill -0 "$lease_supervisor" 2>/dev/null && {
    echo "ACP supervisor did not stop after parent EOF" >&2
    exit 1
}
wait "$lease_supervisor"
wait "$lease_output_pid"
[[ $(lease_state) == unlocked ]] || {
    echo "ACP supervisor did not release the managed harness lease" >&2
    exit 1
}

echo "chaos: passed; five real worker/bridge generations produced five durable markers and supervisor lease handoff survived"
