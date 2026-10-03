#!/usr/bin/env python3
"""Measure daemon update work in a named, isolated fake-harness instance."""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import time
import urllib.request
import urllib.error

from reliability_lab import Lab, ScenarioFailure


class UpdateLab(Lab):
    def __init__(self, args):
        super().__init__(args.mj, "daemon-update-cpu", 10202)
        self.instance = args.instance
        self.worker = args.worker.resolve()

    def environment(self):
        env = super().environment()
        env.update(MJ_INSTANCE=self.instance, MJ_WORKER_BINARY=str(self.worker), RUST_LOG="warn")
        return env

    def command(self, *args, **kwargs):
        return super().command("--instance", self.instance, *args, **kwargs)

    def api(self, path, body, timeout=30):
        token = (self.data / "api-token").read_text().strip()
        request = urllib.request.Request(
            self.base_url + "/api/v1" + path, json.dumps(body).encode(),
            headers={"Content-Type": "application/json", "Authorization": "Bearer " + token},
        )
        try:
            with self.http.open(request, timeout=timeout) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise ScenarioFailure(f"{path}: {error.code} {error.read().decode()}") from error


def counters(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rpartition(")")[2].split()
    return {
        "user_ticks": int(fields[11]), "system_ticks": int(fields[12]),
        "minor_faults": int(fields[7]),
        "rss_bytes": int(Path(f"/proc/{pid}/statm").read_text().split()[1]) * os.sysconf("SC_PAGE_SIZE"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--sessions", type=int, default=16)
    parser.add_argument("--seconds", type=int, default=30)
    parser.add_argument("--linger-seconds", type=int, default=0,
                        help="keep the isolated daemon available for profiling after measurement")
    args = parser.parse_args()
    if args.instance == "default" or args.sessions < 2 or args.seconds < 5 or not 0 <= args.linger_seconds <= 60:
        parser.error("use a named instance, at least two sessions and at least five seconds")
    lab = UpdateLab(args)
    waits = []
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=max(32, args.sessions))
    try:
        port = lab.prepare()
        bridge = lab.runtime_root / "fake_acp.py"
        script = bridge.read_text()
        script = script.replace('        report_execution("running")', '''        report_execution("running")
        if text in ("hold", "stream"):
            import time
            for chunk in range(4000):
                if text == "stream":
                    send({"jsonrpc": "2.0", "method": "session/update", "params": {
                        "sessionId": session_id, "update": {"sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "small update "}}}})
                time.sleep(0.05)''')
        script = script.replace('"text": "reliability reply: " + text',
                                '"text": ("retained history " + "x" * 160 + "\\n") * 1200')
        compile(script, str(bridge), "exec")
        bridge.write_text(script)
        # Native login remains the fixture's authentication scheme; the
        # provider file makes repeated TOML interpretation observable.
        (lab.profile / "config.toml").write_text("# unchanged provider fixture\n" * 1000)
        lab.command("sessions", "--json")
        code, pid = lab.wait_daemon_status(port)
        print(f"daemon: {pid}; artifacts: {lab.root}", flush=True)
        assert lab.request("POST", "/auth/session", {"code": code})[0] == 204
        workspace = lab.api("/workspaces", {"name": "cpu-fixture"})["workspace"]["id"]
        def prepare_session(index):
            result = lab.api("/sessions", {
                "workspace_id": workspace, "profile_id": "fake", "target_id": "localhost",
                "bundle_id": "fixture", "title": f"retained-{index}",
                "project_directory": str(lab.project), "prompt": "retain",
            })
            session = result["session_id"]
            answer = lab.api(f"/sessions/{session}/wait", {"timeout_secs": 60}, timeout=65)
            if answer["outcome"] != "finished":
                raise ScenarioFailure(f"fixture did not finish: {answer}")
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                status, conversation = lab.request("GET", f"/api/conversations/{session}")
                if status == 200 and len(json.dumps(conversation)) > 65536:
                    break
                time.sleep(0.1)
            else:
                raise ScenarioFailure("retained conversation did not exceed 64 KB")
            print(f"prepared {index + 1}/{args.sessions}", flush=True)
            return session
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as preparation:
            sessions = list(preparation.map(prepare_session, range(args.sessions)))
        for session in sessions:
            action = "stream" if session == sessions[-1] else "hold"
            accepted = lab.api(f"/sessions/{session}/prompt", {"text": action})
            waits.append(pool.submit(lab.api, f"/sessions/{session}/wait", {
                "turn_id": accepted["turn_id"], "timeout_secs": args.seconds + 120,
            }, args.seconds + 130))
        time.sleep(3)
        if any(wait.done() for wait in waits):
            raise ScenarioFailure("a fixture wait ended before measurement")
        status, streaming_before = lab.request("GET", f"/api/conversations/{sessions[-1]}")
        if status != 200:
            raise ScenarioFailure("streaming conversation unavailable")
        before = counters(pid)
        started = time.monotonic()
        time.sleep(args.seconds)
        elapsed = time.monotonic() - started
        after = counters(pid)
        status, streaming_after = lab.request("GET", f"/api/conversations/{sessions[-1]}")
        if status != 200:
            raise ScenarioFailure("streaming conversation unavailable")
        if any(wait.done() for wait in waits):
            raise ScenarioFailure("a fixture wait ended during measurement")
        hz = os.sysconf("SC_CLK_TCK")
        result = {
            "instance": args.instance, "sessions": args.sessions, "waiters": len(waits),
            "waiters_still_running": sum(not wait.done() for wait in waits),
            "seconds": elapsed, "daemon_pid": pid,
            "user_cpu_percent": (after["user_ticks"] - before["user_ticks"]) / hz / elapsed * 100,
            "system_cpu_percent": (after["system_ticks"] - before["system_ticks"]) / hz / elapsed * 100,
            "minor_faults_per_second": (after["minor_faults"] - before["minor_faults"]) / elapsed,
            "rss_bytes": after["rss_bytes"],
            "stream_ordinals_per_second": (streaming_after["latest_seq"] - streaming_before["latest_seq"]) / elapsed,
        }
        result["cpu_percent"] = result["user_cpu_percent"] + result["system_cpu_percent"]
        (lab.root / "cpu.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result), flush=True)
        print(f"artifacts: {lab.root}", flush=True)
        time.sleep(args.linger_seconds)
    finally:
        lab.cleanup_owned()
        pool.shutdown(wait=True, cancel_futures=True)
        lab.preserve_runtime()
        leak = lab.leak_report()
        if leak:
            raise ScenarioFailure(leak)
        lab.remove_runtime()


if __name__ == "__main__":
    main()
