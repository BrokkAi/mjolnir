#!/usr/bin/env python3
"""Create a real Podman session with a private Git origin and deterministic ACP.

Provide the built mj and its matching portable worker. The Podman engine
must have the agent-dev:latest image cached. Run in a named test instance.
"""

import argparse
from pathlib import Path

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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", required=True, type=Path)
    parser.add_argument("--worker", required=True, type=Path)
    parser.add_argument("--instance", required=True)
    args = parser.parse_args()
    if not args.instance.strip() or args.instance == "default":
        parser.error("use a separate named test instance")

    lab = Regression(args)
    print(f"Isolated fixture evidence {lab.root}", flush=True)
    try:
        lab.prepare_container_fixture(IMAGE)
        session, _, _ = create_source(lab)
        destroy_session(lab, session)
    finally:
        lab.cleanup()
    print("Passed fresh Podman Create", flush=True)


if __name__ == "__main__":
    main()
