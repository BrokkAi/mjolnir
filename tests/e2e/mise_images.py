#!/usr/bin/env python3
"""Real Podman/registry image publication, assignment, cancellation and restart.

Run with matching --mj and portable --worker builds and --instance mise-i.
A deterministic ACP harness is the only provider fixture; no container engine,
registry, daemon, or worker behavior is replaced. All resources are removed.
"""

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import tomllib
import urllib.request

from reliability_lab import Lab, ScenarioFailure


class Images(Lab):
    def __init__(self, args):
        args.worker = args.worker.resolve()
        self.args = args
        self.instance = args.instance
        self.registry = f"mise-img-registry-{os.getpid()}"
        self.images = set()
        self.sessions = []
        self.containers = set()
        self.server = None
        super().__init__(args.mj, "mise-images", 1493)

    def environment(self):
        env = super().environment()
        env.update(MJ_INSTANCE=self.instance,
                   MJ_WORKER_BINARY=str(self.args.worker.resolve()),
                   CONTAINERS_REGISTRIES_CONF=str(self.runtime_root / "registries.conf"),
                   GH_CONFIG_DIR=str(self.runtime_root / "empty-gh"))
        for name in ["GH_TOKEN", "GITHUB_TOKEN", "OPENAI_API_KEY", "ANTHROPIC_API_KEY", *[key for key in env if key.startswith("AWS_")]]:
            env.pop(name, None)
        return env

    def command(self, *args, timeout=240, check=True):
        if args == ("api-token",):
            result = subprocess.run([str(self.hel), "--instance", self.instance, *args],
                                    env=self.environment(), capture_output=True, text=True, timeout=timeout)
        else:
            sequence = len(list(self.root.glob("command-*.stdout")))
            stdout_path = self.root / f"command-{sequence}.stdout"
            stderr_path = self.root / f"command-{sequence}.stderr"
            with stdout_path.open("w") as stdout, stderr_path.open("w") as stderr:
                result = subprocess.run([str(self.hel), "--instance", self.instance, *args],
                                        env=self.environment(), stdout=stdout, stderr=stderr,
                                        text=True, timeout=timeout)
            result.stdout = stdout_path.read_text()
            result.stderr = stderr_path.read_text()
        # api-token is deliberately omitted from command transcripts.
        if args != ("api-token",):
            with (self.root / "cli.jsonl").open("a") as output:
                output.write(json.dumps(dict(args=args, code=result.returncode,
                                             stdout=result.stdout, stderr=result.stderr)) + "\n")
        if check and result.returncode:
            raise ScenarioFailure(f"mj {args}: {result.stderr[-4000:]}")
        return result

    def podman(self, *args, input=None, check=True):
        result = subprocess.run(["podman", *args], env=self.environment(), input=input,
                                capture_output=True, text=True, timeout=240)
        if check and result.returncode:
            raise ScenarioFailure(f"podman {args}: {result.stderr[-4000:]}")
        return result

    def wait(self, predicate, description):
        deadline = time.monotonic() + 240
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.05)
        raise ScenarioFailure(f"timed out waiting for {description}")

    def image_args(self, *args):
        return ["image", "build", "--path", str(self.project), "--bundle", "fixture",
                "--target", "podman", *args]

    def recipe(self, text):
        (self.project / "mise.toml").write_text(text)

    def reference(self, name):
        printed = self.command(*self.image_args("--print")).stdout
        digest = hashlib.sha256(printed.encode()).hexdigest()[:16]
        reference = f"{self.repository}/{name}:{digest}"
        self.images.add(reference)
        return reference

    def launch_build(self, name):
        self.reference(name)
        log_path = self.root / f"{name}.log"
        log = log_path.open("w")
        process = subprocess.Popen([str(self.hel), "--instance", self.instance,
                                    *self.image_args("--name", name)],
                                   env=self.environment(), stdout=log, stderr=log,
                                   start_new_session=True)
        log.close()
        self.record_process("started", name, process.pid)
        self.wait(lambda: "MISE_IMG_PAUSED" in log_path.read_text() or process.poll() is not None,
                  f"{name} build to pause")
        assert process.poll() is None, log_path.read_text()[-4000:]
        return process, log_path

    def record(self, session):
        return self.daemon_request({"action": "session_record",
                                    "arguments": {"session_id": session}})["value"]

    def run_acceptance(self):
        self.prepare()
        port = self.free_port()
        self.repository = f"localhost:{port}/projects"
        (self.runtime_root / "registries.conf").write_text(
            f'[[registry]]\nlocation = "localhost:{port}"\ninsecure = true\n')
        self.podman("run", "-d", "--name", self.registry, "-p",
                    f"127.0.0.1:{port}:5000", "docker.io/library/registry:2")
        def ready():
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/v2/") as response:
                    return response.status == 200
            except OSError:
                return False
        self.wait(ready, "local registry readiness")
        # Milestone 1 owns the HOME assumption on session-base. This
        # independent image-build regression supplies that base requirement.
        self.base_image = f"{self.repository}/base:fixture"
        self.images.add(self.base_image)
        self.podman("build", "-t", self.base_image, "-f", "-",
                    input="FROM ghcr.io/brokkai/mjolnir/session-base:latest\nRUN mkdir -p /home/hel\n")
        self.podman("push", self.base_image)
        fixture_bin = self.runtime_root / "bin"
        # The fixture uses the real Python already present in session-base.
        (fixture_bin / "python3").unlink()
        (fixture_bin / "python3").symlink_to("/usr/bin/python3")
        git_port = self.free_port()
        self.server = subprocess.Popen(["git", "daemon", "--reuseaddr", "--export-all",
                                        "--listen=0.0.0.0", f"--port={git_port}",
                                        f"--base-path={self.runtime_root}", str(self.project)],
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                       start_new_session=True)
        self.git_output(["remote", "add", "origin",
                         f"git://host.containers.internal:{git_port}/project"], self.project)
        body = (self.config / "config.toml").read_text()
        body = body.replace('image = "ghcr.io/brokkai/mjolnir/session-base:latest"',
                            f'image = "{self.base_image}"', 1)
        body = body.replace('[targets.podman]\nkind = "podman"',
                            '[targets.podman]\nkind = "podman"\nplatform = "linux/amd64"')
        body += (f'\n[bundles.other]\nprimary_repo = "fixture"\n'
                 f'[[bundles.other.repositories]]\nid = "fixture"\nlocal = {json.dumps(str(self.project))}\ndestination = "other"\n'
                 f'\n[images]\nrepository = "{self.repository}"\n'
                 '[github.watch]\nenabled = false\n[jev]\nenabled = false\n'
                 '[machines.local]\nkind = "local"\nbuild_cache = { enabled = false }\n')
        (self.config / "config.toml").write_text(body)
        # Both stdin and output exceed a pipe's capacity. The input also
        # crosses a generator chunk, so no shell argv exceeds Linux's limit.
        self.recipe('#' + 'x' * 70_000 + '\n[hooks]\npostinstall = "python3 -c \'print(\\\"x\\\"*131072)\'"\n')
        expected = self.reference("fixture")
        first = self.command(*self.image_args(), timeout=None)
        assert first.stdout.strip() == expected, first.stdout
        config = tomllib.loads((self.config / "config.toml").read_text())
        assert config["images"]["bundles"]["fixture"]["linux/amd64"] == expected
        second = self.command(*self.image_args())
        assert "already in the registry; pulled" in second.stderr
        assert second.stdout.strip() == expected
        status = self.command("image", "status", "--path", str(self.project), "--bundle", "fixture", "--target", "podman")
        assert "current" in status.stdout and expected in status.stdout, status.stdout
        self.record_action("published-assigned-skipped", image=expected)

        self.command("workspaces", "create", "Mise images")
        code, _ = self.wait_daemon_status(0)
        assert self.request("POST", "/auth/session", {"code": code})[0] == 204
        workspace = self.snapshot()["workspaces"][0]["id"]
        response, body = self.request("POST", "/api/v1/actions", {
            "action": "new", "workspace_id": workspace, "profile_id": "fake",
            "bundle_id": "fixture", "target_id": "podman", "title": "mise-image-assignment",
            "resource_allocation": {"kind": "container", "cpus": 2, "memory_bytes": 4 * 1024 ** 3},
            "additional_mounts": [{"source": str(self.runtime_root),
                                   "destination": str(self.runtime_root), "access": "rw"}],
        })
        assert response == 202, body
        snapshot = self.wait(lambda: next((row for row in self.snapshot()["sessions"]
                                           if row.get("title") == "mise-image-assignment"
                                           and row["state"] in ["running", "error"]), None),
                             "session from recorded image")
        self.sessions.append(snapshot["id"])
        assert snapshot["state"] == "running", snapshot
        record = self.record(snapshot["id"])
        assert "images" not in record["project"]["bundle"], "image assignments leaked into a persisted project snapshot"
        container = record["target"]["container_id"]
        self.containers.add(container)
        inspected = json.loads(self.podman("inspect", container).stdout)[0]
        assert inspected["Config"]["Image"] == expected, inspected["Config"]["Image"]
        self.record_action("session-uses-recorded-image", session=snapshot["id"], image=expected)
        response, preview = self.request("GET", f"/api/v1/sessions/{snapshot['id']}/container-upgrade")
        assert response == 200 and preview["target_image"] == expected, preview
        response, body = self.request("POST", "/api/v1/actions", {
            "action": "new", "workspace_id": workspace, "profile_id": "fake",
            "bundle_id": "other", "target_id": "podman", "title": "mise-image-fallback",
            "resource_allocation": {"kind": "container", "cpus": 2, "memory_bytes": 4 * 1024 ** 3},
            "additional_mounts": [{"source": str(self.runtime_root),
                                   "destination": str(self.runtime_root), "access": "rw"}],
        })
        assert response == 202, body
        other = self.wait(lambda: next((row for row in self.snapshot()["sessions"]
                                       if row.get("title") == "mise-image-fallback"
                                       and row["state"] in ["running", "error"]), None),
                          "unassigned bundle on target image")
        self.sessions.append(other["id"])
        assert other["state"] == "running", other
        container = self.record(other["id"])["target"]["container_id"]
        self.containers.add(container)
        inspected = json.loads(self.podman("inspect", container).stdout)[0]
        assert inspected["Config"]["Image"] == self.base_image, inspected["Config"]["Image"]
        self.record_action("unassigned-bundle-uses-target-image", session=other["id"])

        self.recipe('[hooks]\npostinstall = "echo MISE_IMG_PAUSED; sleep 20"\n')
        cancelled_ref = self.reference("cancelled")
        process, log = self.launch_build("cancelled")
        process.send_signal(signal.SIGINT)
        assert process.wait(timeout=60) != 0, log.read_text()[-4000:]
        assert self.podman("image", "exists", cancelled_ref, check=False).returncode != 0
        active = subprocess.check_output(["ps", "-eo", "comm,args"], text=True)
        assert not any(line.split()[0] == "podman" and cancelled_ref in line
                       for line in active.splitlines()), "cancelled engine survived"
        self.record_action("cancelled-no-tag-or-engine", image=cancelled_ref)

        # A distinct recipe avoids reusing the cancelled build's cached RUN.
        self.recipe('[hooks]\npostinstall = "echo MISE_IMG_PAUSED; sleep 21"\n')
        process, log = self.launch_build("restart")
        started = time.monotonic()
        self.command("daemon", "restart")
        assert time.monotonic() - started < 20, "daemon replacement waited for the build"
        assert process.wait(timeout=240) == 0, log.read_text()[-4000:]
        text = log.read_text()
        assert text.count("resubmitting this reproducible image operation once") == 1, text[-4000:]
        self.record_action("restart-one-resubmission")
        self.trace["finished_at"] = self.timestamp()
        self.trace["outcome"] = "passed"
        self.write_trace()
        print(json.dumps({"outcome": "passed", "evidence": str(self.root)}))

    def cleanup(self):
        for session in self.sessions:
            with contextlib.suppress(Exception):
                self.command("destroy", "--session", session, check=False)
        with contextlib.suppress(Exception):
            self.command("daemon", "stop", check=False)
        if self.server:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(self.server.pid, signal.SIGTERM)
            self.server.wait(timeout=10)
        for container in self.containers:
            self.podman("rm", "-f", container, check=False)
        self.podman("rm", "-f", self.registry, check=False)
        for image in self.images:
            self.podman("rmi", "-f", image, check=False)
        self.cleanup_owned()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--instance", default="mise-i")
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use an isolated named instance")
    lab = Images(args)
    try:
        lab.run_acceptance()
    finally:
        lab.cleanup()


if __name__ == "__main__":
    main()
