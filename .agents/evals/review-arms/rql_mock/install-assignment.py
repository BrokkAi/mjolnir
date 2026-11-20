#!/usr/bin/env python3
"""Publish or revoke a pilot session's RQL assignment."""
import argparse
import fcntl
import ipaddress
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib
from urllib.parse import urlsplit


HERE = Path(__file__).resolve().parent
DEFAULT_CONFIG = Path.home() / ".config/mjolnir/instances/rql-census-pilot/config.toml"


def atomic_write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            output.write(text)
            output.flush()
            os.fsync(output.fileno())
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def key_pair(directory, sid):
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    private = directory / sid / "key"
    if not private.exists():
        with tempfile.TemporaryDirectory(dir=directory) as temp:
            stage = Path(temp) / sid
            stage.mkdir(mode=0o700)
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "",
                            "-C", f"rql-pilot-{sid}", "-f", str(stage / "key")],
                           check=True, stdout=sys.stderr)
            stage.rename(private.parent)
    public = private.with_suffix(".pub").read_text().split()
    kind, token = public[:2]
    if kind != "ssh-ed25519":
        raise ValueError("expected an Ed25519 writer key")
    return private.read_text(), f"{kind} {token}"


def authorized_entry(transport, sid, public):
    command = shlex.join([str(HERE / "serve-rql.sh"), str(transport), sid])
    if "\n" in command or "\r" in command:
        raise ValueError("forced-command paths must fit on one line")
    command = command.replace('"', '\\"')
    return f'restrict,command="{command}" {public} rql-pilot-{sid}'


def host_assignment(assignment, *, revoke):
    sid = assignment["session_id"]
    transport = Path(os.environ["RQL_TRANSPORT_CONFIG"]).resolve()
    auth = Path(os.environ["RQL_AUTHORIZED_KEYS"]).resolve()
    if auth.is_relative_to((Path.home() / ".ssh").resolve()):
        raise ValueError("RQL_AUTHORIZED_KEYS must be a dedicated pilot file outside ~/.ssh")
    key_dir = transport.parent / ".rql-writer-keys"
    transport.parent.mkdir(parents=True, exist_ok=True)
    with transport.with_name(f"{transport.name}.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        config = json.loads(transport.read_text()) if transport.exists() else {}
        sessions = config.setdefault("sessions", {})
        lines = auth.read_text().splitlines() if auth.exists() else []
        tag = f"rql-pilot-{sid}"
        kept = [line for line in lines if not line.rstrip().endswith(f" {tag}")]
        if revoke:
            if kept != lines:
                atomic_write(auth, "".join(f"{line}\n" for line in kept))
            if sid in sessions:
                del sessions[sid]
                atomic_write(transport, json.dumps(config, indent=2) + "\n")
            if (key_dir / sid).exists():
                shutil.rmtree(key_dir / sid)
            return

        required = ("arm", "diff", "intro", "log", "manifest_sha256", "engine_inputs_sha256")
        entry = {name: assignment[name] for name in required}
        if (not Path(entry["log"]).is_absolute()
                or not all(re.fullmatch(r"[0-9a-f]{64}", entry[name]) for name in required[-2:])):
            raise ValueError("the RQL assignment requires an absolute log and both frozen input digests")
        Path(entry["log"]).parent.mkdir(parents=True, exist_ok=True)
        private, public = key_pair(key_dir, sid)
        config["manifest"] = str(Path(assignment["manifest"]).resolve())
        config["engine"] = "rql_mock.engine:answer"
        sessions[sid] = entry
        atomic_write(transport, json.dumps(config, indent=2) + "\n")
        atomic_write(auth, "".join(f"{line}\n" for line in [*kept, authorized_entry(transport, sid, public)]))
        return private


def client_environment():
    config_path = Path(os.environ.get("RQL_PILOT_CONFIG", DEFAULT_CONFIG))
    config = tomllib.loads(config_path.read_text())
    profiles = [profile for profile in config["profiles"].values()
                if profile.get("kind") == "codex" and profile.get("codex_profile") == "review_arms_rql"
                and profile.get("enabled", True)]
    if len(profiles) != 1:
        raise ValueError("the pilot configuration must have exactly one enabled RQL Codex profile")
    return {name: value for name, value in profiles[0]["environment"].items() if name.startswith("RQL_")}


def known_host(target):
    parsed = urlsplit(target if target.startswith("ssh://") else f"ssh://{target}")
    host = parsed.hostname
    if not parsed.username or not host or parsed.password or parsed.path or parsed.query or parsed.fragment:
        raise ValueError("RQL_SSH_TARGET must name a user and a Podman-reachable host")
    try:
        address = ipaddress.ip_address(host)
    except ValueError:
        address = None
    if host == "localhost" or (address is not None and address.is_loopback):
        raise ValueError("RQL_SSH_TARGET uses the container's loopback instead of a host address")
    port = parsed.port or 22
    label = f"[{host}]:{port}" if port != 22 else host
    public = Path(os.environ["RQL_SSH_HOST_PUBLIC_KEY"]).read_text().split()
    kind, token = public[:2]
    return f"{label} {kind} {token}\n"


PUBLISH = """
import json, os, sys, tempfile
from pathlib import Path

environment, local, credentials = json.load(sys.stdin)
assignment = Path(environment["RQL_SESSION_ASSIGNMENT_DIR"]) / (local["session_id"] + ".json")
files = [(Path(path), content) for path, content in credentials]
files.append((assignment, json.dumps(local) + "\\n"))
for path, text in files:
    if not path.is_absolute():
        raise ValueError("RQL client paths must be absolute")
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, name = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            output.write(text)
            output.flush()
            os.fsync(output.fileno())
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)
client = Path(environment["RQL_SSH_CONFIG"]).with_name("client.py")
os.execve(sys.executable, [sys.executable, "-B", str(client), "--preflight"], {**os.environ, **environment})
"""


def publish(assignment):
    sys.path.insert(0, str(HERE.parent))
    from driver import container

    local = {key: assignment[key] for key in ("session_id", "arm")}
    sid, arm = local["session_id"], local["arm"]
    environment = {**client_environment(), "MJ_SESSION_ID": sid}
    credentials = []
    if arm != "none":
        pin = known_host(environment["RQL_SSH_TARGET"])
        key = host_assignment(assignment, revoke=False)
        credentials.extend(((environment["RQL_SSH_KEY"], key), (environment["RQL_SSH_KNOWN_HOSTS"], pin)))
    output = subprocess.run(
        ["podman", "exec", "--interactive", container(sid)["container"], "python3", "-B", "-c", PUBLISH],
        check=True, input=json.dumps([environment, local, credentials]), text=True, stdout=subprocess.PIPE,
    )
    preflight = json.loads(output.stdout)
    if preflight != {"session_id": sid, "tools": [] if arm == "none" else ["rql_query"]}:
        raise RuntimeError("the session's RQL client did not confirm its expected tools")
    print(output.stdout, end="")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revoke", action="store_true")
    parser.add_argument("assignment", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    try:
        assignment = json.loads(args.assignment.read_text())
        if (not re.fullmatch(r"[0-9a-f]{32}", assignment["session_id"])
                or assignment["arm"] not in {"R", "R-loc", "none"}):
            raise ValueError("invalid pilot session assignment")
        if args.revoke:
            if assignment["arm"] != "none":
                host_assignment(assignment, revoke=True)
        else:
            publish(assignment)
    except (OSError, ValueError, KeyError, TypeError, AssertionError, subprocess.SubprocessError) as error:
        sys.exit(f"install RQL assignment: {error}")


if __name__ == "__main__":
    main()
