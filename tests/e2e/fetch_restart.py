#!/usr/bin/env python3
"""Stream large CLI and MCP fetches across restart in a private named instance.

Build mj and mj-worker with --features test-hooks. No host instance is touched.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time

import reliability_lab

WAIT = 120
SIZE = 64 * 1024 * 1024


def wait_for(predicate, description):
    deadline = time.monotonic() + WAIT
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.02)
    raise RuntimeError(f"timed out waiting for {description}")


def digest(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def peak_at_hook(lab, name):
    marker = (lab.hooks / f"{name}.reached").read_text()
    pid = marker.strip().removeprefix("pid=")
    status = Path(f"/proc/{pid}/status").read_text()
    peak = int(next(line.split()[1] for line in status.splitlines()
                    if line.startswith("VmHWM:"))) * 1024
    assert peak < SIZE, f"{name} buffered the whole {SIZE}-byte payload: peak={peak}"
    return peak


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--instance", required=True)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named instance")
    lab = reliability_lab.Lab(args.mj, "fetch-restart", 1544)
    children = []
    try:
        lab.prepare()
        with (lab.config / "config.toml").open("a") as config:
            config.write("\n[jev]\nenabled = false\n[github.watch]\nenabled = false\n")
        scratch = lab.runtime_root / "scratch"
        scratch.mkdir()
        lab.hook_name = "fetch_source_ready"

        def environment():
            return {**lab.environment(), "MJ_INSTANCE": args.instance,
                    "MJ_WORKER_BINARY": str(args.worker.resolve()), "TMPDIR": str(scratch)}

        def command(*arguments):
            return [str(args.mj.resolve()), "--instance", args.instance, *arguments]

        def cli(*arguments):
            result = subprocess.run(command(*arguments), env=environment(), capture_output=True,
                                    text=True, timeout=WAIT)
            assert result.returncode == 0, (arguments, result.stdout, result.stderr)
            return result.stdout

        cli("workspaces", "create", "Fetch restart")

        def new_session():
            session = json.loads(cli("new", "--workspace", "Fetch restart", "--profile", "fake",
                                  "--target", "localhost", "--project-directory", str(lab.project),
                                  "--subagents", "none", "--json"))["session_id"]
            def ready():
                record = json.loads(cli("sessions", "--session", session, "--json"))
                assert not record.get("has_error"), record
                return record["state"] == "running"
            wait_for(ready, "session's live target")
            return session

        def wait_hook(name, child):
            def reached():
                marker = lab.hooks / f"{name}.reached"
                if marker.exists() and marker.read_text().strip().removeprefix("pid=").isdigit():
                    return True
                if child.poll() is not None:
                    stdout, stderr = child.communicate()
                    raise RuntimeError(f"transfer exited before {name}: {stdout} {stderr}")
                return False
            wait_for(reached, name)

        source_session = new_session()
        # A scratch file is explicitly part of the fetch surface. Sparse zeros
        # make fixture creation cheap; the wire still carries all 64 MiB.
        source = scratch / "large.bin"
        with source.open("wb") as file:
            file.truncate(SIZE)
        expected = digest(source)
        download = subprocess.Popen(command("fetch", "--session", source_session,
                                             "--path", str(source), "--json"),
                                    env=environment(),
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        children.append(download)
        wait_hook("fetch_source_ready", download)
        source_peak = peak_at_hook(lab, "fetch_source_ready")
        started = time.monotonic()
        cli("daemon", "restart")
        restart_seconds = time.monotonic() - started
        assert download.poll() is None, "download ended instead of following the replacement"
        (lab.hooks / "fetch_source_ready.continue").touch()
        stdout, stderr = download.communicate(timeout=WAIT)
        assert download.returncode == 0, stderr
        saved = json.loads(stdout)
        assert saved["bytes"] == SIZE and digest(saved["path"]) == expected
        assert "retrying the download" in stderr, stderr
        Path(saved["path"]).unlink()

        lab.hook_name = "fetch_destination_ready"
        cli("daemon", "restart")
        receiver = new_session()
        endpoint = wait_for(lambda: next((p for p in lab.data.rglob("subagents.sock")
                                         if receiver in p.parts), None), "receiver tool endpoint")
        request = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
            "name": "fetch", "arguments": {"session_id": source_session, "path": str(source)}}}
        copy = subprocess.Popen([str(args.worker.resolve()), "worker", "subagent-mcp",
                                 "--socket", str(endpoint), "--role", "message_only"],
                                env=environment(), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True)
        children.append(copy)
        copy.stdin.write(json.dumps(request) + "\n")
        copy.stdin.flush()
        wait_hook("fetch_destination_ready", copy)
        receiver_peak = peak_at_hook(lab, "fetch_destination_ready")
        destination = scratch / "mjolnir" / source_session / source.relative_to("/")
        assert not destination.exists(), "partial destination was published"
        cli("daemon", "restart")
        assert not destination.exists(), "restart published an incomplete transfer"
        (lab.hooks / "fetch_destination_ready.continue").touch()
        # Keep the same MCP request pending while durable delegation replays it.
        stdout, stderr = copy.communicate(timeout=WAIT)
        assert copy.returncode == 0, stderr
        reply = json.loads(stdout.splitlines()[0])
        assert "error" not in reply, reply
        result = reply["result"]
        assert not result.get("isError"), result
        saved = result["structuredContent"]
        assert saved["bytes"] == SIZE and digest(saved["path"]) == expected
        destination = Path(saved["path"])
        leftovers = [p for p in destination.parent.iterdir() if p != destination and p.stat().st_size]
        assert not leftovers, f"cancelled receiver left payload files behind: {leftovers}"
        destination.unlink()
        print(json.dumps({"cli_bytes": SIZE, "mcp_bytes": SIZE,
                          "restart_seconds": restart_seconds, "source_peak_bytes": source_peak,
                          "receiver_peak_bytes": receiver_peak, "artifacts": str(lab.root)}))
    finally:
        for child in children:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
        lab.cleanup_owned()


if __name__ == "__main__":
    main()
