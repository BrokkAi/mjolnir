#!/usr/bin/env python3
"""Create a real Podman session and optionally Move it to another target.

Provide the built mj and its matching portable worker. The Podman engine
must have the agent-dev:latest image cached. The fixture runs a private Git
origin and a deterministic ACP peer. Use a named instance for each case.
"""

import argparse
import hashlib
import json
from pathlib import Path
import random

from container_upgrades import Regression


IMAGE = "ghcr.io/brokkai/mjolnir/agent-dev:latest"


# Hard-won: 6edc4602e: fresh container Create read a nonexistent checkpoint.
def create_source(lab):
    lab.cli("workspaces", "create", "Podman workspace regression")
    lab.authenticate()
    workspace = lab.snapshot()["workspaces"][0]["id"]
    fixture = {
        "source": str(lab.runtime_root),
        "destination": str(lab.runtime_root),
        "access": "rw",
    }
    allocation = {"kind": "container", "cpus": 2, "memory_bytes": 4 << 30}
    session = lab.create_session(workspace, [fixture], "Podman source", allocation)
    source = lab.container(session)
    checkout = f'{lab.record(session)["container_workspace"]}/fixture'
    assert lab.podman("exec", source, "cat", f"{checkout}/README.md") == "isolated Hel reliability repository\n"
    return session, source, checkout


def destroy_session(lab, session):
    lab.cli("destroy", "--session", session)
    lab.wait(
        lambda: lab.daemon_request({
            "action": "session_record", "arguments": {"session_id": session},
        })["value"] is None,
        "isolated session destruction",
    )


def add_keep_payload(lab, session, source, checkout):
    # Incompressible fixture data drives the stream across pipe boundaries.
    payload = random.Random(1662).randbytes(128 * 1024)
    source_file = lab.runtime_root / "evidence.txt"
    source_file.write_bytes(payload)
    lab.podman(
        "exec", source, "sh", "-ec",
        'mkdir -p "$1"; cp "$2" "$1/evidence.txt"; printf "changed in Podman\\n" >> "$3/README.md"',
        "fixture", f"{checkout}/.mj/keep/{session}", str(source_file), checkout,
    )
    return hashlib.sha256(payload).hexdigest()


# Hard-won: #1662: destination provisioning collided with the retained source name.
def move_to_podman(lab):
    with (lab.config / "config.toml").open("a") as config:
        config.write(
            '\n[targets.destination]\nkind = "podman"\n'
            f'image = "{IMAGE}"\npull_policy = "never"\n'
            'workspace_storage = { kind = "podman-volume" }\n'
        )
    session, source, checkout = create_source(lab)
    digest = add_keep_payload(lab, session, source, checkout)
    result = json.loads(
        lab.cli("move", "--session", session, "--target", "destination", "--yes", "--json").stdout
    )
    assert result["outcome"] == "completed", result
    after = lab.record(session)
    destination = lab.container(session)
    assert after["target_template_id"] == "destination", after
    assert destination != source, (source, destination)
    checkout = f'{after["container_workspace"]}/fixture'
    restored = f"{checkout}/.mj/keep/{session}/evidence.txt"
    assert lab.podman("exec", destination, "sha256sum", restored).split()[0] == digest
    assert lab.podman("exec", destination, "cat", f"{checkout}/README.md").endswith("changed in Podman\n")
    return session, source


# Hard-won: #1663: LocalBare Move staged a worker before its placement was committed.
def move_to_bare(lab):
    origin = lab.git_output(["remote", "get-url", "origin"], lab.project).strip()
    local_origin = origin.replace("host.containers.internal", "127.0.0.1")
    lab.git_output(
        ["config", "--file", str(lab.runtime_root / "gitconfig"), f"url.{local_origin}.insteadOf", origin],
        lab.project,
    )
    session, source, checkout = create_source(lab)
    digest = add_keep_payload(lab, session, source, checkout)
    # The fixture bind is only needed by the Podman worker. LocalBare can
    # already see these paths, so remove the bind through Move preparation.
    preparation = lab.daemon_request({
        "action": "prepare_move_session",
        "arguments": {
            "session_id": session,
            "target_template_id": "localhost",
            "clear_resource_allocation": True,
            "additional_mounts": [],
        },
    }, timeout=120)
    assert preparation["reply"] == "move_preparation", preparation
    result = lab.daemon_request({
        "action": "move_session",
        "arguments": {
            "preparation": preparation["value"],
            "acknowledge_interruption": True,
        },
    }, timeout=180)
    assert result["value"]["outcome"] == "completed", result
    after = lab.record(session)
    assert after["target_template_id"] == "localhost", after
    checkout = Path(after["managed_worktree"]["worktree_root"])
    restored = checkout / ".mj" / "keep" / session / "evidence.txt"
    assert hashlib.sha256(restored.read_bytes()).hexdigest() == digest
    assert (checkout / "README.md").read_text().endswith("changed in Podman\n")
    assert not checkout.with_suffix(".move-keep").exists()
    return session, source


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", required=True, type=Path)
    parser.add_argument("--worker", required=True, type=Path)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--destination", choices=["podman", "bare"])
    args = parser.parse_args()
    if args.instance.strip() in {"", "default"}:
        parser.error("use a separate named test instance")

    lab = Regression(args)
    print(f"Isolated fixture evidence {lab.root}", flush=True)
    try:
        lab.prepare_container_fixture(IMAGE)
        if args.destination is None:
            session, _, _ = create_source(lab)
            result = "fresh Podman Create"
        else:
            move = move_to_podman if args.destination == "podman" else move_to_bare
            session, source = move(lab)
            containers = lab.podman(
                "container", "ls", "--all", "--filter", f"label=dev.mj.session={session}",
                "--format", "{{.Names}}",
            ).splitlines()
            assert source not in containers, containers
            result = f"Podman Move to {args.destination}"
        destroy_session(lab, session)
    finally:
        lab.cleanup()
    print(f"Passed {result}", flush=True)


if __name__ == "__main__":
    main()
