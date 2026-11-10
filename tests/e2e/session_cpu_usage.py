#!/usr/bin/env python3
"""Exercise worker CPU sampling, the daemon feed and the live TUI in cpu-test."""
import argparse
import contextlib
import json
import math
import os
import pathlib
import re
import subprocess
import time

from reliability_lab import Lab, PtyClient, ScenarioFailure


class CpuLab(Lab):
    @property
    def instance(self):
        return f"cpu-test-{self.root.name}"

    def environment(self):
        env = super().environment()
        env["MJ_INSTANCE"] = self.instance
        return env

    def command(self, *args, **kwargs):
        return super().command("--instance", self.instance, *args, **kwargs)

    def start_tui(self, name):
        client = PtyClient(name, [str(self.hel), "--instance", self.instance], self.environment(), self.root / f"{name}.capture")
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
        reply = self.daemon_request({"action": "runtime_changes", "arguments": {
            "cursor": None, "wait": False, "subagent_parents": [],
        }})
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

    def container_id(self, session_id):
        reply = self.daemon_request({"action": "session_record", "arguments": {"session_id": session_id}})
        return reply["value"]["target"]["container_id"]

    @staticmethod
    def container_cpu_usec(container_id):
        result = subprocess.run(
            ["podman", "exec", container_id, "cat", "/sys/fs/cgroup/cpu.stat"],
            capture_output=True, text=True, timeout=10, check=True,
        )
        for line in result.stdout.splitlines():
            key, value = line.split()
            if key == "usage_usec":
                return int(value)
        raise ScenarioFailure(f"container cpu.stat has no usage_usec: {result.stdout!r}")

    def start_container_git_remote(self):
        (self.runtime_root / "releases").mkdir()
        python = self.runtime_root / "bin/python3"
        python.unlink()
        python.write_text('#!/bin/sh\nexec /usr/bin/python3 "$@"\n')
        python.chmod(0o700)
        port = self.free_port()
        output = (self.root / "git-daemon.stdout").open("w")
        errors = (self.root / "git-daemon.stderr").open("w")
        server = subprocess.Popen(
            ["git", "daemon", "--reuseaddr", "--export-all", "--listen=0.0.0.0",
             f"--port={port}", f"--base-path={self.runtime_root}", str(self.project)],
            stdout=output, stderr=errors, start_new_session=True,
        )
        self.record_process("started", "git-daemon", server.pid)
        origin = f"git://host.containers.internal:{port}/project"
        self.git_output(["remote", "add", "origin", origin], self.project)

    def run_container_sibling(self, workspace, tui):
        status, response = self.request("POST", "/api/actions", {
            "action": "new", "workspace_id": workspace, "profile_id": "fake",
            "bundle_id": "fixture", "target_id": "podman", "title": "container-cpu-sibling",
            "resource_allocation": {"kind": "container", "cpus": 2, "memory_bytes": 4 * 1024 ** 3},
            "additional_mounts": [{
                "source": str(self.runtime_root), "destination": str(self.runtime_root), "access": "rw",
            }],
        })
        assert status == 202, response
        snapshot = self.wait_snapshot(
            lambda value: any(row.get("title") == "container-cpu-sibling" and row.get("state") == "running"
                              for row in value.get("sessions", [])),
            "running container CPU session",
        )
        session_id = next(row["id"] for row in snapshot["sessions"]
                          if row.get("title") == "container-cpu-sibling" and row["state"] == "running")
        container_id = self.container_id(session_id)
        baseline = self.wait_cpu(session_id, lambda _: True, timeout=60)
        assert baseline["scope"] == "container", baseline
        before_usec = self.container_cpu_usec(container_id)
        started = time.monotonic()
        # This command is launched by the test driver through Podman exec, so
        # its CPU is in the session container but outside the worker's process tree.
        burner = r'''import multiprocessing, os, sys
stop = multiprocessing.Event()
def burn():
    value = 1
    while not stop.is_set():
        value = (value * 3 + 1) % 1000003
count = max(1, (os.cpu_count() + 49) // 50)
children = [multiprocessing.Process(target=burn) for _ in range(count)]
for child in children: child.start()
sys.stdin.buffer.read(1)
stop.set()
for child in children: child.join()
assert all(child.exitcode == 0 for child in children)
'''
        command = subprocess.Popen(
            ["podman", "exec", "-i", container_id, "python3", "-c", burner],
            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=False,
        )
        try:
            during = None
            during_usec = before_usec
            covered = baseline["hourly_covered_secs"]
            coverage_advances = 0
            deadline = time.monotonic() + 25
            while time.monotonic() < deadline:
                sample = self.cpu(session_id)
                if (sample and sample["state"] == "measured"
                        and sample["usage"]["scope"] == "container"
                        and sample["usage"]["hourly_covered_secs"] > covered
                        and sample["usage"]["recent_permille"] > 0):
                    # Two distinct coverage advances establish that the final
                    # selected sample covers an interval after the burner began.
                    covered = sample["usage"]["hourly_covered_secs"]
                    coverage_advances += 1
                    if coverage_advances == 2:
                        during = sample["usage"]
                        during_usec = self.container_cpu_usec(container_id)
                        assert during_usec - before_usec >= 500_000, (before_usec, during_usec)
                        break
                if command.poll() is not None:
                    raise ScenarioFailure("sibling burner exited before a container CPU sample appeared")
                time.sleep(0.2)
            if during is None:
                raise ScenarioFailure(f"container sibling CPU was not sampled: {self.cpu(session_id)}")
            command.stdin.write(b"x")
            command.stdin.flush()
            command.stdin.close()
            returncode = command.wait(timeout=20)
            stderr = command.stderr.read().decode("utf-8", "replace")
            if returncode:
                raise ScenarioFailure(f"Podman sibling burner failed: {stderr}")

            # The final direct cgroup read is after the burner has exited. The
            # feed exposes rates rather than a cumulative total, so compare its
            # sampled rate with the independently measured monotonic total delta.
            after_usec = self.container_cpu_usec(container_id)
            elapsed = time.monotonic() - started
            delta_usec = after_usec - before_usec
            assert delta_usec > 1_000_000, (before_usec, after_usec)
            online_cpus = during["online_cpus"]
            direct_permille = 1000 * delta_usec / (elapsed * 1_000_000 * online_cpus)
            assert direct_permille > 0
            assert direct_permille / 4 <= during["recent_permille"] <= direct_permille * 4, {
                "feed_during_sibling": during,
                "direct_cgroup_delta_usec": delta_usec,
                "elapsed_secs": elapsed,
                "direct_permille": direct_permille,
            }
            after = self.wait_cpu(
                session_id,
                lambda usage: usage["scope"] == "container" and usage["hourly_covered_secs"] > covered,
                timeout=20,
            )
            direct_hourly_permille = (
                1000 * delta_usec
                / (1_000_000 * after["hourly_covered_secs"] * after["online_cpus"])
            )
            assert direct_hourly_permille > 0
            assert direct_hourly_permille / 4 <= after["hourly_permille"] <= direct_hourly_permille * 4, {
                "feed_after_exit": after,
                "direct_cgroup_delta_usec": delta_usec,
                "direct_hourly_permille": direct_hourly_permille,
            }
            evidence = {
                "scope": after["scope"],
                "feed_during_sibling": during,
                "feed_after_exit": after,
                "cgroup_before_usec": before_usec,
                "cgroup_at_sample_usec": during_usec,
                "cgroup_after_exit_usec": after_usec,
                "cgroup_delta_usec": delta_usec,
                "direct_permille": round(direct_permille, 2),
                "elapsed_secs": round(elapsed, 2),
            }
            (self.root / "container-cpu-evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
            print(json.dumps(evidence, indent=2), flush=True)
            assert "container-cpu-sibling" in tui.text()
        finally:
            if command.poll() is None:
                if command.stdin and not command.stdin.closed:
                    command.stdin.close()
                try:
                    command.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    command.terminate()
                    command.kill()
                    command.wait()

    def run_cpu(self, *, container_only=False):
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
        if container_only:
            self.start_container_git_remote()
            # Hard-won: b7d5c8399: process-tree sampling missed CPU burned by an independent container exec.
            self.run_container_sibling(workspace, tui)
            self.stop_daemon()
            return
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
            if title_row is not None and re.search(r"\d+(?:\.\d+)?%", rows[title_row]):
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
        tui.wait_for("hourly", timeout=10)
        screen = tui.text().splitlines()
        recent_row = next(index for index, line in enumerate(screen) if "Recent" in line)
        recent_column = screen[recent_row].index("Recent") + 1
        tui.send(f"\x1b[<0;{recent_column};{recent_row+1}M\x1b[<0;{recent_column};{recent_row+1}m".encode())
        tui.wait_for("recent", timeout=10)
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
        deadline = idle_started + 20
        while True:
            rows = tui.text().splitlines()
            if any("cpu-burner" in row and re.search(r"\d+(?:\.\d+)?% recent", row) for row in rows):
                break
            if time.monotonic() >= deadline:
                raise ScenarioFailure("CPU did not reach the idle recent metric: " + tui.text())
            time.sleep(0.1)
        tui.send(b"\x1b")
        deadline = idle_started + 20
        while True:
            rows = tui.text().splitlines()
            title_row = next((index for index, row in enumerate(rows) if "cpu-burner" in row), None)
            if title_row is not None and not re.search(r"\d+(?:\.\d+)?%", rows[title_row]):
                break
            if time.monotonic() >= deadline:
                raise ScenarioFailure("CPU did not clear from the session row within 20s: " + tui.text())
            time.sleep(0.1)
        evidence = {"busy": usage, "after_daemon_restart": after, "idle": idle, "load_delay_secs": round(load_delay, 2), "idle_delay_secs": round(time.monotonic() - idle_started, 2)}
        assert evidence["idle_delay_secs"] <= 20, evidence
        (self.root / "cpu-evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(json.dumps(evidence, indent=2), flush=True)
        self.start_container_git_remote()
        # Hard-won: b7d5c8399: process-tree sampling missed CPU burned by an independent container exec.
        self.run_container_sibling(workspace, tui)
        self.stop_daemon()

    def cleanup_cpu_containers(self):
        # The engine owns resource existence, including failed provisioning.
        # Each run has its own instance label; no session must reach `running`.
        owned = subprocess.run(
            ["podman", "container", "ls", "--all", "--quiet",
             "--filter", "label=dev.mj.managed=true",
             "--filter", f"label=dev.mj.instance={self.instance}"],
            capture_output=True, text=True, timeout=30, check=True,
        )
        for container_id in owned.stdout.split():
            subprocess.run(
                ["podman", "container", "rm", "--force", "--", container_id],
                capture_output=True, text=True, timeout=30, check=True,
            )
            exists = subprocess.run(
                ["podman", "container", "exists", container_id],
                capture_output=True, text=True, timeout=10,
            )
            if exists.returncode == 0:
                raise ScenarioFailure(f"owned Podman container still exists after cleanup: {container_id}")
            if exists.returncode != 1:
                raise ScenarioFailure(f"could not verify removal of {container_id}: {exists.stderr}")

    def cleanup_cpu_run(self):
        # ExitStack runs every step after a failure, in reverse registration order.
        with contextlib.ExitStack() as cleanup:
            cleanup.callback(self.integrity)
            cleanup.callback(self.capture_process_tree)
            cleanup.callback(self.cleanup_cpu_containers)
            cleanup.callback(self.cleanup_owned)
            cleanup.callback(self.stop_daemon)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=pathlib.Path, default=pathlib.Path("target/debug/mj"))
    parser.add_argument("--container-only", action="store_true", help="run only the isolated container sibling-work case")
    args = parser.parse_args()
    lab = CpuLab(args.mj, "session-cpu", 1, watchdog=False)
    try:
        lab.run_cpu(container_only=args.container_only)
    finally:
        lab.cleanup_cpu_run()
    print(f"CPU acceptance evidence: {lab.root}")


if __name__ == "__main__":
    main()
