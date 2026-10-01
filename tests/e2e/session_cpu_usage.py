#!/usr/bin/env python3
"""Exercise worker CPU sampling, the daemon feed and the live TUI in cpu-test."""
import argparse
import json
import math
import os
import pathlib
import re
import time

from reliability_lab import Lab, PtyClient, ScenarioFailure


class CpuLab(Lab):
    def environment(self):
        env = super().environment()
        env["MJ_INSTANCE"] = "cpu-test"
        return env

    def command(self, *args, **kwargs):
        return super().command("--instance", "cpu-test", *args, **kwargs)

    def start_tui(self, name):
        client = PtyClient(name, [str(self.hel), "--instance", "cpu-test"], self.environment(), self.root / f"{name}.capture")
        self.clients.append(client)
        return client

    def wait_snapshot(self, predicate, description, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            snapshot = self.snapshot()
            if predicate(snapshot):
                return snapshot
            time.sleep(0.1)
        raise ScenarioFailure(f"timed out waiting for {description}: {json.dumps(snapshot)[:2000]}")

    def cpu(self, session_id):
        reply = self.daemon_request({"action": "runtime_changes", "arguments": {"cursor": None, "wait": False}})
        projection = reply["value"]["Snapshot"]["projection"]
        return projection["session_cpu"].get(session_id)

    def wait_cpu(self, session_id, predicate, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = self.cpu(session_id)
            if value and value["state"] == "measured" and predicate(value["usage"]):
                return value["usage"]
            time.sleep(0.2)
        raise ScenarioFailure(f"CPU sample did not settle: {value}")

    def run_cpu(self):
        print("Preparing isolated cpu-test instance", flush=True)
        port = self.prepare()
        # Launch a CPU burner as a real descendant of the fake ACP harness,
        # through its normal prompt handling. Reap it before replying so both
        # live-child and waited-child accounting are exercised.
        bridge = self.runtime_root / "fake_acp.py"
        script = bridge.read_text()
        script = script.replace('        report_execution("running")', '''        report_execution("running")
        import subprocess
        import math
        count = max(1, math.ceil(os.cpu_count() / 100))
        code = "import time; end=time.monotonic()+45; x=1\\nwhile time.monotonic()<end: x=(x*3+1)%1000003"
        children = [subprocess.Popen([sys.executable, "-c", code]) for _ in range(count)]
        for child in children:
            child.wait()''')
        bridge.write_text(script)
        tui = self.start_tui("tui-1")
        tui.wait_for("Sessions")
        code, _ = self.wait_daemon_status(port)
        status, _ = self.request("POST", "/auth/session", {"code": code})
        assert status == 204
        workspace = self.snapshot()["workspaces"][0]["id"]
        status, _ = self.request("POST", "/api/actions", {"action": "new", "workspace_id": workspace, "profile_id": "fake", "bundle_id": "fixture", "target_id": "localhost", "title": "cpu-burner", "project_directory": str(self.project)})
        assert status == 202
        snapshot = self.wait_snapshot(lambda snapshot: any(session.get("title") == "cpu-burner" and session.get("state") == "running" for session in snapshot.get("sessions", [])), "running CPU session")
        session_id = next(session["id"] for session in snapshot["sessions"] if session["title"] == "cpu-burner")
        # A completed baseline establishes the sampler cadence before the turn.
        self.wait_cpu(session_id, lambda usage: True, timeout=30)
        started = time.monotonic()
        print("Starting CPU-bound worker child", flush=True)
        self.submit_prompt(session_id, "measure CPU")
        count = max(1, math.ceil(os.cpu_count() / 100))
        expected = 1000 * count / os.cpu_count()
        usage = self.wait_cpu(session_id, lambda usage: expected / 1.5 <= usage["recent_permille"] <= expected * 1.5)
        load_delay = time.monotonic() - started
        # Read the session row, then open the actual Targets dropdown.
        deadline = started + 20
        while True:
            rows = tui.text().splitlines()
            title_row = next((index for index, row in enumerate(rows) if "cpu-burner" in row), None)
            if title_row is not None and title_row + 1 < len(rows) and re.search(r"\d+(?:\.\d+)?%", rows[title_row + 1]):
                break
            if time.monotonic() >= deadline:
                raise ScenarioFailure("CPU did not reach the session row within 20s: " + tui.text())
            time.sleep(0.1)
        screen = tui.text().splitlines()
        row = next(index for index, line in enumerate(screen) if "Targets ▾" in line)
        column = screen[row].index("Targets ▾") + 1
        tui.send(f"\x1b[<0;{column};{row+1}M\x1b[<0;{column};{row+1}m".encode())
        tui.wait_for("CPU by session…")
        tui.send(b"4")
        tui.wait_for("hourly /", timeout=10)
        assert "cpu-burner" in tui.text()
        covered = usage["hourly_covered_secs"]
        self.command("daemon", "restart")
        # The lab uses an ephemeral viewer port; discover the replacement's
        # address and authenticate its new viewer before reading snapshots.
        code, _ = self.wait_daemon_status(port)
        status, _ = self.request("POST", "/auth/session", {"code": code})
        assert status == 204
        after = self.wait_cpu(session_id, lambda usage: usage["hourly_covered_secs"] > covered, timeout=30)
        tui.wait_for("CPU by session", timeout=10)
        deadline = time.monotonic() + 60
        while (self.session(self.snapshot(), session_id) or {}).get("chat_phase") != "idle":
            if time.monotonic() >= deadline:
                raise ScenarioFailure("CPU command did not finish")
            time.sleep(0.2)
        idle_started = time.monotonic()
        idle = self.wait_cpu(session_id, lambda usage: usage["recent_permille"] < 10)
        evidence = {"busy": usage, "after_daemon_restart": after, "idle": idle, "load_delay_secs": round(load_delay, 2)}
        (self.root / "cpu-evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(json.dumps(evidence, indent=2), flush=True)
        tui.wait_for("0% recent", timeout=max(0.1, idle_started + 20 - time.monotonic()))
        tui.send(b"\x1b")
        deadline = idle_started + 20
        while True:
            rows = tui.text().splitlines()
            title_row = next((index for index, row in enumerate(rows) if "cpu-burner" in row), None)
            if title_row is not None and title_row + 1 < len(rows) and not re.search(r"\d+(?:\.\d+)?%", rows[title_row + 1]):
                break
            if time.monotonic() >= deadline:
                raise ScenarioFailure("CPU did not clear from the session row within 20s: " + tui.text())
            time.sleep(0.1)
        evidence = {"busy": usage, "after_daemon_restart": after, "idle": idle, "load_delay_secs": round(load_delay, 2), "idle_delay_secs": round(time.monotonic() - idle_started, 2)}
        assert evidence["idle_delay_secs"] <= 20, evidence
        (self.root / "cpu-evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(json.dumps(evidence, indent=2), flush=True)
        self.stop_daemon()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=pathlib.Path, default=pathlib.Path("target/debug/mj"))
    args = parser.parse_args()
    lab = CpuLab(args.mj, "session-cpu", 1, watchdog=False)
    try:
        lab.run_cpu()
    finally:
        lab.cleanup_owned()
        lab.capture_process_tree()
        lab.integrity()
    print(f"CPU acceptance evidence: {lab.root}")


if __name__ == "__main__":
    main()
