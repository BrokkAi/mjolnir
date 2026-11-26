#!/usr/bin/env python3
"""Start real authenticated harnesses on the implicit session image, without prompts.

Pass matching --mj and portable --worker binaries, --source-config pointing at
an existing mj config, and --artifacts for text evidence. Only credential files
are copied into private profiles; source profiles are never modified. All CLI
calls use --instance mise-d and private configuration/data directories.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import tomllib

from mise_project_tools import WORKER_QUERY


CREDENTIALS = {
    "codex": ["auth.json"],
    "claude": [".credentials.json"],
    "kimi": ["credentials/kimi-code.json", "config.toml", "oauth/kimi-code"],
    "grok": ["auth.json"],
    "muse": ["auth.json"],
    "antigravity": ["antigravity-cli/antigravity-oauth-token"],
    "dsh": [".credentials.yaml", ".env"],
    "open-code": ["opencode.json"],
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--source-config", type=Path, required=True)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--instance", default="mise-d")
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use an isolated named instance")
    args.artifacts.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="mise-d-"))
    config, data, repo = root / "config", root / "data", root / "fixture"
    for directory in [config, data, repo]:
        directory.mkdir(mode=0o700)
    env = {name: os.environ[name] for name in ["HOME", "PATH", "USER", "LOGNAME", "LANG", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] if name in os.environ}
    env.update(MJ_CONFIG_DIR=str(config), MJ_DATA_DIR=str(data),
               MJ_INSTANCE=args.instance, MJ_WORKER_BINARY=str(args.worker.resolve()))
    source = tomllib.loads(args.source_config.read_text())
    secrets_path = args.source_config.parent / "secrets.toml"
    secrets = tomllib.loads(secrets_path.read_text()) if secrets_path.exists() else {}
    profiles, results, sessions, containers = {}, {}, [], set()
    server = None

    def run(argv, *, check=True, input=None):
        result = subprocess.run(list(map(str, argv)), env=env, capture_output=True, text=True, input=input)
        if check and result.returncode:
            raise RuntimeError(f"{argv}: {result.stderr}")
        return result.stdout

    def cli(*argv, check=True):
        return run([args.mj.resolve(), "--instance", args.instance, *argv], check=check)

    def wait(predicate, description):
        # Test execution deadline, not a project-tools runtime limit.
        deadline = time.monotonic() + 1200
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(0.2)
        raise RuntimeError("timed out: " + description)

    def container(session):
        with sqlite3.connect(f"file:{data / 'mj.sqlite3'}?mode=ro", uri=True) as database:
            row = database.execute("SELECT resource_id FROM session_targets WHERE session_id=?", (session,)).fetchone()
        return row[0] if row else None

    try:
        profile_text = ""
        for entry in source["profiles"].values():
            kind = entry["kind"]
            if kind in profiles or kind not in CREDENTIALS:
                continue
            home = Path(entry["home"]).expanduser()
            if not (home / CREDENTIALS[kind][0]).exists():
                continue
            profile = root / ("profile-" + kind) / {"muse": "muse", "open-code": "opencode"}.get(kind, "home")
            profile.mkdir(mode=0o700, parents=True)
            for relative in CREDENTIALS[kind]:
                src, dst = home / relative, profile / relative
                if src.is_file():
                    dst.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(src, dst)
                    dst.chmod(0o600)
            if kind == "codex":
                (profile / "config.toml").write_text("")
            profiles[kind] = profile
            profile_text += f"[profiles.{kind}]\nkind = {json.dumps(kind)}\nhome = {json.dumps(str(profile))}\n"
            for key, value in entry.get("environment", {}).items():
                if isinstance(value, dict):
                    value = (secrets.get(value["from_secret"]) if "from_secret" in value
                             else os.environ.get(value.get("from_env", "")))
                if isinstance(value, str):
                    env[key] = value
                    profile_text += f"environment.{json.dumps(key)} = {{ from_env = {json.dumps(key)} }}\n"
        print("Configured harness kinds: " + ", ".join(profiles), flush=True)
        (repo / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.99.1"\nprofile = "minimal"\n')
        (repo / ".nvmrc").write_text("24\n")
        (repo / "README.md").write_text("mise default-image fixture\n")
        run(["git", "init", "--initial-branch=main", repo])
        run(["git", "-C", repo, "add", "."])
        run(["git", "-C", repo, "-c", "user.name=Mise Test", "-c", "user.email=mise@example.invalid", "commit", "-m", "recipe"])
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        with (args.artifacts / "git-daemon.log").open("w") as log:
            server = subprocess.Popen(["git", "daemon", "--reuseaddr", "--export-all", "--listen=0.0.0.0", f"--port={port}", f"--base-path={root}", str(root)], env=env, start_new_session=True, stdout=log, stderr=subprocess.STDOUT)
        run(["git", "-C", repo, "remote", "add", "origin", f"git://host.containers.internal:{port}/fixture"])
        # No image key: exercise the real default rather than an explicit base.
        (config / "config.toml").write_text(f'''version = 23
{profile_text}
[targets.podman]
kind = "podman"
pull_policy = "never"
workspace_storage = {{ kind = "podman-volume" }}
[targets.docker]
kind = "docker"
pull_policy = "never"
[machines.local]
kind = "local"
build_cache = {{ enabled = false }}
[github.watch]
enabled = false
[jev]
enabled = false
[bundles.fixture]
primary_repo = "fixture"
[[bundles.fixture.repositories]]
id = "fixture"
local = {json.dumps(str(repo))}
destination = "fixture"
''')
        cli("workspaces", "create", "Mise default")
        for kind in profiles:
            session, cid = None, None
            try:
                session = json.loads(cli("--workspace", "Mise default", "new", "--profile", kind, "--target", "podman", "--bundle", "fixture", "--subagents", "none", "--review", "off", "--no-project-memory", "--no-github-events", "--no-github-credentials", "--json"))["session_id"]
                sessions.append(session)

                def ready():
                    record = json.loads(cli("sessions", "--session", session, "--json"))
                    if record.get("has_error"):
                        raise RuntimeError(record.get("last_error", record))
                    return record["state"] == "running"

                wait(ready, f"{kind} ACP session ready")
                cid = container(session)
                image = run(["podman", "inspect", "--format", "{{.ImageName}}", cid]).strip()
                assert image == "ghcr.io/brokkai/mjolnir/session-base:latest", image
                worker_root = f"/var/lib/hel/workers/{session}"
                startup = run(["podman", "exec", cid, "cat", worker_root + "/worker-startup.json"])
                assert "Install project tools" in startup, startup
                assert "acp-session-open" in startup, startup
                (args.artifacts / f"{kind}-startup.json").write_text(startup)
                run(["podman", "exec", "-i", cid, "python3", "-", session, "cargo --version; node --version"], input=WORKER_QUERY)
                transcript = wait(lambda: (value if "cargo 1.99.1" in (value := cli("transcript", "--session", session, "--json")) and "v24." in value else None), "tool versions")
                (args.artifacts / f"{kind}-transcript.json").write_text(transcript)
                results[kind] = {"result": "passed", "session": session, "image": image}
                print(f"{kind}: ACP session running on {image}; project tools installed; cargo 1.99.1 and Node24", flush=True)
            except Exception as error:
                results[kind] = {"result": "failed", "session": session, "error": str(error)}
                print(f"{kind}: FAILED {error}", flush=True)
            if session:
                cid = cid or container(session)
                if cid:
                    containers.add(cid)
                    for filename in ["project-tools.log", "worker-startup.json", "acp-supervisor.log", "worker.log"]:
                        value = run(["podman", "exec", cid, "cat", f"/var/lib/hel/workers/{session}/{filename}"], check=False)
                        (args.artifacts / f"{kind}-{filename}").write_text(value)
                cli("destroy", "--session", session, check=False)
            (args.artifacts / "results.json").write_text(json.dumps(results, indent=2))
    finally:
        for session in sessions:
            cli("destroy", "--session", session, check=False)
            cid = container(session)
            if cid:
                containers.add(cid)
        cli("daemon", "stop")
        for cid in containers:
            run(["podman", "rm", "-f", cid], check=False)
        for log in (data / "logs").glob("*.log"):
            shutil.copyfile(log, args.artifacts / log.name)
        if server:
            os.killpg(server.pid, signal.SIGTERM)
            server.wait()
        shutil.rmtree(root)
    assert profiles, "no native logins found"
    assert all(result["result"] == "passed" for result in results.values()), results


if __name__ == "__main__":
    main()
