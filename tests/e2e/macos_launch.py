#!/usr/bin/env python3
"""Opt-in macOS launch probes using a real signed-in Claude or Codex harness.

Uses four paid prompts in a disposable named instance. Requires Darwin, a
previous installed mj, the candidate mj and its matching packaged worker.
Run --help for arguments. GUI and Apple-container missions remain manual.
"""

from __future__ import annotations

import argparse
import http.cookiejar
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import time
import urllib.request

from reliability_lab import daemon_request


class Lab:
    def __init__(self, args: argparse.Namespace):
        self.root = args.artifacts.expanduser().resolve()
        self.root.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.old = args.previous_mj.expanduser().resolve()
        self.new = args.mj.expanduser().resolve()
        self.current = self.old
        self.harness = args.harness
        self.instance = f"mac-launch-{os.getpid()}"
        # Exercise macOS's space-containing native data path shape.
        self.data = self.root / "Library/Application Support/mjolnir"
        self.config = self.root / "config"
        self.profile = self.root / f"{self.harness}-profile"
        self.project = self.root / "project"
        for directory in (self.config, self.profile, self.project):
            directory.mkdir()
        if self.harness == "codex":
            # Copy only login into a disposable profile. Refreshes must never
            # rewrite the real profile's auth.json during an isolated probe.
            source = (args.profile_home or Path.home() / ".codex").expanduser()
            shutil.copyfile(source / "auth.json", self.profile / "auth.json")
            self.profile.joinpath("auth.json").chmod(0o600)
        self.env = dict(os.environ, MJ_INSTANCE=self.instance,
                        MJ_CONFIG_DIR=str(self.config), MJ_DATA_DIR=str(self.data),
                        SESSIONWIKI_DATA=str(self.root / "sessionwiki"),
                        MJOLNIR_NO_UPDATE_CHECK="1")
        # Each installed release must select its own matching sibling worker.
        for name in ("MJ_WORKER_BINARY", "MJ_WORKER_DIR", "MJ_WORKER_URL",
                     "MJ_WORKER_SHA256", "MJ_DEV_RESTART_STALE_DAEMON",
                     "MJ_DAEMON_OWNER_PID", "MJ_DAEMON_EXIT_WHEN_IDLE"):
            self.env.pop(name, None)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        # macOS Claude uses the existing Keychain login regardless of profile
        # home. A fresh home avoids touching the user's settings/session files.
        self.config.joinpath("config.toml").write_text(f'''version = 13
[phone]
enabled = true
bind = "127.0.0.1:{port}"
tailscale_detect = false
[profiles.probe]
kind = {json.dumps(self.harness)}
home = {json.dumps(str(self.profile))}
[targets.localhost]
kind = "bare"
''')
        self.session: str | None = None
        self.results: dict = {"platform": platform.platform(), "instance": self.instance,
                              "commands": [], "checks": {}, "outcome": "running"}
        self.save()

    def save(self):
        self.root.joinpath("results.json").write_text(json.dumps(self.results, indent=2) + "\n")

    def run(self, *args: str, check: bool = True, timeout: int = 180) -> str:
        command = [str(self.current), "--instance", self.instance, *args]
        result = subprocess.run(command, env=self.env, capture_output=True,
                                text=True, timeout=timeout)
        entry = {"command": command, "status": result.returncode,
                 "stdout": result.stdout, "stderr": result.stderr}
        self.results["commands"].append(entry)
        self.save()
        if check and result.returncode != 0:
            raise RuntimeError(f"{args[0]} failed: {result.stderr}")
        return result.stdout

    def check(self, name: str, evidence: object):
        self.results["checks"][name] = evidence
        self.save()
        print(f"passed: {name}", flush=True)

    def until(self, operation, predicate, description: str):
        deadline = time.monotonic() + 180
        while True:
            result = operation()
            if predicate(result):
                return result
            if time.monotonic() >= deadline:
                raise RuntimeError(f"timed out: {description}: {result}")
            time.sleep(1)

    def session_state(self):
        return json.loads(self.run("sessions", "--session", self.session, "--json"))

    def metadata(self):
        return json.loads(self.data.joinpath("daemon.json").read_text())

    def worker(self):
        pid = int(self.data.joinpath("workers", self.session, "worker.pid").read_text())
        command = subprocess.run(["ps", "-p", str(pid), "-o", "command="], check=True,
                                 capture_output=True, text=True).stdout
        assert self.session in command and "worker" in command, "worker PID changed owners"
        return pid

    def wait_reply(self, marker: str, turn: int | None = None):
        turn_args = ["--turn", str(turn)] if turn is not None else []
        result = json.loads(self.run("wait", "--session", self.session,
                                     "--timeout", "180", "--json", *turn_args, timeout=200))
        if result["outcome"] != "finished" or marker not in result.get("final_message", ""):
            raise RuntimeError(f"missing successful reply {marker}: {result}")
        if turn is not None:
            assert result["turn_id"] == turn, "another turn replaced the accepted prompt"
        return result

    def foreground_sleep(self):
        owner = self.worker()
        processes = subprocess.run(["ps", "-axo", "pid=,ppid=,command="], check=True,
                                   capture_output=True, text=True).stdout.splitlines()
        tree = {}
        candidates = []
        for line in processes:
            pid, parent, command = line.strip().split(None, 2)
            tree[int(pid)] = int(parent)
            if '-c import time; time.sleep(45); print("MAC_HANDOFF_SURVIVED")' in command:
                candidates.append(int(pid))
        for candidate in candidates:
            seen = set()
            while candidate in tree and candidate not in seen:
                if candidate == owner:
                    return True
                seen.add(candidate)
                candidate = tree[candidate]
        return False

    def viewer_logout(self):
        access = self.until(
            lambda: daemon_request(self.data, {"action": "web_viewer_access"})["value"],
            lambda value: isinstance(value, dict) and "Ready" in value, "viewer readiness",
        )["Ready"]
        base = access["viewer_url"].rstrip("/")
        jar = http.cookiejar.CookieJar()
        viewer_http = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
        request = urllib.request.Request(base + "/auth/session", method="POST",
            data=json.dumps({"code": access["viewer_code"]}).encode(),
            headers={"Content-Type": "application/json"})
        with viewer_http.open(request, timeout=10) as response:
            assert response.status == 204
        cookie = "; ".join(f"{item.name}={item.value}" for item in jar)

        def status():
            request = urllib.request.Request(base + "/auth/session", headers={"Cookie": cookie})
            with urllib.request.urlopen(request, timeout=10) as response:
                return json.load(response)["signed_in"]

        assert status(), "login cookie was rejected"
        request = urllib.request.Request(base + "/auth/session", method="DELETE",
                                         headers={"Cookie": cookie})
        with urllib.request.urlopen(request, timeout=10) as response:
            assert response.status == 204
        assert not status(), "logged-out cookie was accepted"
        self.run("daemon", "restart")
        assert not status(), "restart forgot cookie revocation"
        self.check("viewer login and durable logout", True)

    def exercise(self, model: str | None):
        for args in (["git", "init", str(self.project)],
                     ["git", "-C", str(self.project), "add", "README.md"],
                     ["git", "-C", str(self.project), "-c", "user.name=Mac Launch Test",
                      "-c", "user.email=mac-launch@example.invalid", "commit", "-m", "fixture"]):
            self.project.joinpath("README.md").write_text("Disposable Mac launch fixture.\n")
            subprocess.run(args, check=True, capture_output=True)
        self.results["previous_version"] = self.run("--version").strip()
        self.run("workspaces", "create", "mac-launch")
        model_args = ["--model", model] if model else []
        created = json.loads(self.run("new", "--workspace", "mac-launch", "--profile", "probe",
            "--target", "localhost", "--project-directory", str(self.project), *model_args,
            "--json", "Reply with exactly MAC_LAUNCH_READY."))
        self.session = created["session_id"]
        self.wait_reply("MAC_LAUNCH_READY")
        accepted = json.loads(self.run("prompt", "--session", self.session, "--json",
            "Run python3 -c 'import time; time.sleep(45); print(\"MAC_HANDOFF_SURVIVED\")' "
            "as a foreground shell command, wait for it to finish, then reply exactly MAC_HANDOFF_SURVIVED."))
        self.until(self.session_state,
                   lambda value: value.get("activity_state", {}).get("state") == "turn",
                   "active turn before upgrade")
        before = self.metadata()
        worker_before = self.worker()
        profile_sessions_before = set(self.profile.glob("sessions/*"))
        # Ensure the requested real shell has started, rather than replacing
        # between prompt admission and the tool invocation.
        self.until(self.foreground_sleep, bool, "foreground sleep before upgrade")
        self.current = self.new
        self.results["candidate_version"] = self.run("--version").strip()
        started = time.monotonic()
        self.run("api-info", "--json")
        after = self.metadata()
        elapsed = time.monotonic() - started
        assert after["pid"] != before["pid"], "obsolete daemon was reused"
        assert self.worker() == worker_before, "busy worker was replaced"
        assert elapsed < 30, f"handoff waited for a worker turn: {elapsed:.1f}s"
        reply = self.wait_reply("MAC_HANDOFF_SURVIVED", accepted["turn_id"])
        self.check("active-turn automatic handoff", {
            "old_daemon": before["pid"], "new_daemon": after["pid"],
            "worker": worker_before, "seconds": elapsed, "turn": reply.get("turn_id")})
        # A second startup must reuse the winning daemon.
        self.run("api-info", "--json")
        assert self.metadata()["pid"] == after["pid"], "repeated startup replaced the daemon"
        self.run("checkpoint", "--session", self.session)
        self.run("suspend", "--session", self.session, "--acknowledge-unpublished-work")
        self.until(self.session_state, lambda value: value["state"] == "suspended",
                   "suspension completion")
        self.run("resume", "--session", self.session)
        self.until(self.session_state, lambda value: value["state"] == "running" and value["is_idle"],
                   "resumed worker readiness")
        self.run("prompt", "--session", self.session, "Reply exactly MAC_RESUME_OK.")
        self.wait_reply("MAC_RESUME_OK")
        self.check("checkpoint, suspend and resume with a space-containing data path", True)
        worker_root = self.data / "workers" / self.session
        launch = json.loads(worker_root.joinpath("launch.json").read_text())
        staged_home = Path(launch["harness_home"])
        assert staged_home.is_dir() and not staged_home.is_symlink()
        assert staged_home.is_relative_to(worker_root), "resumed harness uses the profile home"
        assert set(self.profile.glob("sessions/*")) == profile_sessions_before, \
            "new harness wrote sessions in profile home"
        self.check(f"{self.harness} login from a staged home after upgrade and resume", True)
        self.viewer_logout()
        self.run("prompt", "--session", self.session, "Reply exactly MAC_RESTART_OK.")
        self.wait_reply("MAC_RESTART_OK")
        self.check("session answers after daemon restart", True)
        self.run("setup", "instructions", "--platform", "macos")
        self.run("doctor", "--json", check=False)

    def cleanup(self):
        failures = []
        # Use the candidate's cleanup, including fixes to legacy macOS paths.
        self.current = self.new
        if self.session:
            try:
                self.run("destroy", "--session", self.session)
                self.until(lambda: json.loads(self.run("sessions", "--json"))["sessions"],
                           lambda sessions: all(item["id"] != self.session for item in sessions),
                           "session destruction")
            except Exception as error:
                failures.append(str(error))
        if self.data.joinpath("daemon.json").exists():
            try:
                self.run("daemon", "stop")
            except Exception as error:
                failures.append(str(error))
        self.results["cleanup_failures"] = failures
        self.save()
        if failures:
            self.results["outcome"] = "failed"
            self.save()
            raise RuntimeError("cleanup failed; retained working files: " + "; ".join(failures))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--previous-mj", type=Path, required=True)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--harness", choices=("claude", "codex"), default="claude")
    parser.add_argument("--profile-home", type=Path,
                        help="Codex home whose existing auth.json is copied into the test profile")
    parser.add_argument("--model", help="defaults to haiku for Claude, harness default for Codex")
    args = parser.parse_args()
    if platform.system() != "Darwin":
        parser.error("run this probe on macOS")
    lab = Lab(args)
    try:
        lab.exercise(args.model or ("haiku" if args.harness == "claude" else None))
        lab.results["outcome"] = "passed"
    except BaseException as error:
        lab.results["outcome"] = "failed"
        lab.results["failure"] = str(error)
        raise
    finally:
        lab.cleanup()
    print(f"Evidence: {lab.root}")


if __name__ == "__main__":
    main()
