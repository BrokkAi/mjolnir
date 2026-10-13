#!/usr/bin/env python3
"""Exercise diagnostic ENOSPC in real, isolated worker/daemon processes on Linux.

Run after cargo build: python3 tests/e2e/full_disk_diagnostics.py --worker
 target/debug/mj-worker --mj target/debug/mj --evidence /path/to/evidence.
Only /dev/full receives failed writes. No live instance or real disk is filled.
"""

import argparse
import json
import http.client
import urllib.parse
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import sys
import time

from reliability_lab import daemon_request


def wait_for(predicate, process, label):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if predicate():
            return
        if process.poll() is not None:
            raise AssertionError(f"{label}: process stopped with {process.returncode}")
        time.sleep(0.05)
    raise AssertionError(f"timed out: {label}")


def stop(process):
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    worker_binary, mj_binary = args.worker.resolve(), args.mj.resolve()
    args.evidence.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="mj-full-disk-e2e-") as temporary:
        base = Path(temporary)
        root, profile = base / "worker", base / "profile"
        root.mkdir()
        profile.mkdir()
        (root / "worker.log").symlink_to("/dev/full")
        # Force the startup-error diagnostic that used to call eprintln. The
        # already-resolved-environment flag preserves this test-only directory.
        (root / "worker-startup.json").mkdir()
        bridge = base / "recorded_acp.py"
        # Recorded initialize/new-session replies keep a real supervisor and
        # worker alive without contacting a provider or using host credentials.
        bridge.write_text("""import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request.get('method')
    if method == 'initialize':
        result = {'protocolVersion': 1, 'agentCapabilities': {}, 'authMethods': []}
    elif method == 'session/new':
        result = {'sessionId': 'full-disk-recorded-session', 'modes': {'currentModeId': 'agent-full-access', 'availableModes': [{'id': 'agent-full-access', 'name': 'Full access'}]}}
    else:
        result = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
""")
        config = {
            "session_id": "full-disk-e2e-worker",
            "harness": "codex", "harness_home": str(profile),
            "bridge_command": sys.executable, "bridge_args": [str(bridge)],
            "environment": {"CODEX_HOME": str(profile)},
            "cwd": str(base), "execution_policy": "unconstrained",
            "subagents": {"mode": "none"},
        }
        config_path = base / "launch.json"
        config_path.write_text(json.dumps(config))
        env = dict(os.environ, MJ_INSTANCE="full-disk-e2e", MJ_DATA_DIR=str(base / "data"),
                   MJ_CONFIG_DIR=str(base / "config"), MJOLNIR_NO_UPDATE_CHECK="1",
                   RUST_LOG="warn", XDG_CACHE_HOME=str(base / "cache"))
        with open("/dev/full", "wb", buffering=0) as full:
            worker = subprocess.Popen(
                [str(worker_binary), "--login-environment-ready", "worker", "run",
                 "--root", str(root), "--config", str(config_path)],
                env=env, stdin=subprocess.DEVNULL, stdout=full, stderr=full,
                start_new_session=True,
            )
            try:
                wait_for(lambda: (root / "control.sock").exists(), worker, "worker relay ready")
                assert os.readlink(f"/proc/{worker.pid}/fd/2") == "/dev/full"
                with socket.socket(socket.AF_UNIX) as stream:
                    stream.settimeout(5)
                    stream.connect(str(root / "control.sock"))
                    reader = stream.makefile("rb")
                    try:
                        hello = {"request_id": "hello", "protocol_version": 1,
                                 "request": {"method": "hello", "params": {
                                     "controller_version": "full-disk-e2e",
                                     "supported": {"min": 1, "max": 4294967295}}}}
                        stream.sendall(json.dumps(hello).encode() + b"\n")
                        response = json.loads(reader.readline())
                        assert response["result"] == "ok", response
                        protocol = response["protocol_version"]
                        for index in range(100):
                            request = {"request_id": str(index), "protocol_version": protocol,
                                       "request": {"method": "status"}}
                            stream.sendall(json.dumps(request).encode() + b"\n")
                            response = json.loads(reader.readline())
                            assert response["result"] == "ok", response
                    finally:
                        reader.close()
                assert worker.poll() is None
            finally:
                stop(worker)
                if (root / "worker-exit.json").exists():
                    (args.evidence / "worker-exit.json").write_bytes((root / "worker-exit.json").read_bytes())
            assert worker.returncode == 0, f"worker stopped with {worker.returncode}"
            record_bytes = (root / "worker-exit.json").read_bytes()
            record = json.loads(record_bytes)
            assert len(record_bytes) == 16 * 1024
            assert record["reason"] == "worker stopped"
            assert record["recent_log"], "worker exit record lost its diagnostic tail"
            (args.evidence / "worker-exit.json").write_bytes(record_bytes)
            print("worker: /dev/full stderr, failed startup diagnostics, 100 relay replies, clean exit with reason and recent_log")

            config_dir = base / "config"
            config_dir.mkdir(exist_ok=True)
            (config_dir / "config.toml").write_text(
                'version = 12\n[phone]\nenabled = true\nbind = "127.0.0.1:0"\ntailscale_detect = false\n'
                '[targets.podman]\nkind = "podman"\npull_policy = "never"\n'
                '[targets.docker]\nkind = "docker"\npull_policy = "never"\n' 
            )
            daemon = subprocess.Popen(
                [str(mj_binary), "--instance", "full-disk-e2e", "daemon-run"],
                env=dict(env, RUST_LOG="info"), stdin=subprocess.DEVNULL, stdout=full, stderr=full,
                start_new_session=True,
            )
            try:
                metadata_path = base / "data" / "daemon.json"
                wait_for(metadata_path.exists, daemon, "isolated daemon ready")
                # The enabled viewer prints its code to full stdout at startup.
                def viewer_ready():
                    access = daemon_request(base / "data", {"action": "web_viewer_access"})["value"]
                    return "Ready" in access
                wait_for(viewer_ready, daemon, "viewer ready after full-stdout diagnostic")
                access = daemon_request(base / "data", {"action": "web_viewer_access"})["value"]["Ready"]
                url = urllib.parse.urlsplit(access["viewer_url"])
                connection = http.client.HTTPConnection(url.hostname, url.port, timeout=5)
                try:
                    connection.request("GET", "/")
                    assert connection.getresponse().status == 200, "viewer did not serve HTTP"
                finally:
                    connection.close()
                assert daemon.poll() is None
                assert os.readlink(f"/proc/{daemon.pid}/fd/2") == "/dev/full"
            finally:
                stop(daemon)
            assert daemon.returncode == 0, f"daemon stopped with {daemon.returncode}"
            for path in (base / "data" / "logs").glob("mj-daemon-*.log"):
                (args.evidence / path.name).write_bytes(path.read_bytes())
            print("daemon: named isolated instance, /dev/full stdout/stderr, viewer startup and clean SIGTERM shutdown")


if __name__ == "__main__":
    main()
