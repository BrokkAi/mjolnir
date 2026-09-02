#!/usr/bin/env python3
"""Kill Hel at compiled-in durability boundaries and validate restart safety."""

from __future__ import annotations

import argparse
import contextlib
import os
import pathlib
import signal
import sys
import threading
import time

from reliability_lab import Lab, ScenarioFailure, TIMEOUT


HOOKS = [
    "daemon_metadata_before_listening",
    "journal_append_before_snapshot_publication",
    "config_replacement_before_reference_migration",
    "lifecycle_reservation_before_result_publication",
    "relay_projection_before_revision_publication",
    "checkpoint_archive_before_database_publication",
]


def wait_hook(lab: Lab, hook: str) -> int:
    marker = lab.hooks / f"{hook}.reached"
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        with contextlib.suppress(OSError, ValueError):
            line = marker.read_text().strip()
            pid = int(line.removeprefix("pid="))
            if pathlib.Path(f"/proc/{pid}").exists():
                lab.record_action("hook-reached", hook=hook, pid=pid)
                return pid
        time.sleep(0.02)
    raise ScenarioFailure(f"timed out waiting for test hook {hook}")


def kill_hook_owner(lab: Lab, hook: str, pid: int) -> None:
    if pid not in lab.owned_pids():
        raise ScenarioFailure(f"hook {hook} named unowned process {pid}")
    os.kill(pid, signal.SIGKILL)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        status = pathlib.Path(f"/proc/{pid}/stat")
        try:
            fields = status.read_text().split()
        except FileNotFoundError:
            break
        if len(fields) > 2 and fields[2] == "Z":
            break
        time.sleep(0.02)
    else:
        raise ScenarioFailure(f"hook owner {pid} survived SIGKILL")
    lab.record_process("killed", hook, pid)
    (lab.hooks / f"{hook}.continue").write_text("continue after first crash\n")


def start_dashboard(lab: Lab, name: str, create_workspace: bool) -> object:
    client = lab.start_tui(name)
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        screen = client.text()
        if "Sessions" in screen:
            return client
        if "Workspaces" in screen:
            client.send(b"\r\r" if create_workspace else b"\r")
            client.wait_for("Sessions")
            return client
        if client.process.poll() is not None:
            raise ScenarioFailure(f"{name} exited before reaching the dashboard")
        time.sleep(0.05)
    raise ScenarioFailure(f"{name} did not reach a workspace screen: {client.text()[-4000:]}")


def login_web(lab: Lab, port: int) -> None:
    code, pid = lab.wait_daemon_status(port)
    lab.daemon_pid = pid
    lab.base_url = f"http://127.0.0.1:{port}"
    status, _ = lab.request("POST", "/auth/session", {"code": code})
    if status != 204:
        raise ScenarioFailure(f"web login returned {status}")


def restart_after_daemon_crash(lab: Lab, port: int, ordinal: int) -> object:
    client = start_dashboard(lab, f"restart-{ordinal}", create_workspace=False)
    login_web(lab, port)
    lab.record_action("daemon-restarted", pid=lab.daemon_pid)
    return client


def one_workspace(lab: Lab) -> dict[str, object]:
    snapshot = lab.snapshot()
    workspaces = snapshot.get("workspaces", [])
    if not isinstance(workspaces, list) or len(workspaces) != 1:
        raise ScenarioFailure(f"expected one workspace: {workspaces!r}")
    return workspaces[0]


def start_session(lab: Lab, title: str) -> str:
    workspace = one_workspace(lab)
    status, _ = lab.request(
        "POST",
        "/api/actions",
        {
            "action": "new",
            "workspace_id": workspace["id"],
            "profile_id": "fake",
            "bundle_id": "fixture",
            "target_id": "localhost",
            "title": title,
            "project_directory": str(lab.project),
        },
    )
    if status != 202:
        raise ScenarioFailure(f"new action returned {status}")
    lab.record_action("new-session", title=title)
    return title


def start_daemon_lifecycle_session(lab: Lab, title: str) -> str:
    workspace = one_workspace(lab)
    reply = lab.daemon_request(
        {
            "action": "start_create_session",
            "arguments": {
                "workspace_id": workspace["id"],
                "profile_id": "fake",
                "bundle_id": "fixture",
                "project_directory": str(lab.project),
                "target_template_id": "localhost",
                "additional_mounts": [],
                "allow_dirty_local": False,
                "resource_allocation": None,
                "title": title,
                "session_title_override": title,
            },
        },
    )
    if not isinstance(reply, dict) or reply.get("reply") != "registered_session":
        raise ScenarioFailure(f"unexpected start-create reply: {reply!r}")
    session = reply.get("value", {}).get("session", {})
    session_id = session.get("id")
    if not isinstance(session_id, str):
        raise ScenarioFailure(f"start-create reply omitted its session id: {reply!r}")
    lab.record_action("new-daemon-session", title=title, session_id=session_id)
    return session_id


def wait_session(lab: Lab, title: str, states: set[str]) -> dict[str, object]:
    snapshot = lab.wait_snapshot(
        lambda value: any(
            item.get("title") == title and item.get("state") in states
            for item in value.get("sessions", [])
        ),
        f"session {title} in {sorted(states)}",
    )
    return next(item for item in snapshot["sessions"] if item["title"] == title)


def crash_and_restart_daemon(lab: Lab, hook: str, port: int, ordinal: int) -> object:
    pid = wait_hook(lab, hook)
    kill_hook_owner(lab, hook, pid)
    if lab.daemon_pid == pid:
        lab.daemon_pid = None
    return restart_after_daemon_crash(lab, port, ordinal)


def run_hook(lab: Lab, hook: str) -> None:
    port = lab.prepare()
    first = lab.start_tui("initial")

    if hook == "daemon_metadata_before_listening":
        pid = wait_hook(lab, hook)
        kill_hook_owner(lab, hook, pid)
        first.terminate()
        start_dashboard(lab, "restart-1", create_workspace=True)
        login_web(lab, port)
        one_workspace(lab)
        lab.record_action("restart-validated", hook=hook)
        return

    first.wait_for("Workspaces")
    first.send(b"\r\r")
    first.wait_for("Sessions")
    login_web(lab, port)

    if hook == "config_replacement_before_reference_migration":
        outcome: list[object] = []

        def rename() -> None:
            try:
                outcome.append(
                    lab.daemon_request(
                        {
                            "action": "rename_target",
                            "arguments": {"old_id": "localhost", "new_id": "local-renamed"},
                        },
                    )
                )
            except BaseException as error:
                outcome.append(error)

        thread = threading.Thread(target=rename, name="rename-target")
        thread.start()
        marker = lab.hooks / f"{hook}.reached"
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline and not marker.exists():
            if not thread.is_alive():
                thread.join()
                raise ScenarioFailure(
                    f"rename request completed before reaching its durability hook: {outcome!r}"
                )
            time.sleep(0.02)
        crash_and_restart_daemon(lab, hook, port, 1)
        thread.join(timeout=5)
        if thread.is_alive():
            raise ScenarioFailure("rename request did not unblock after daemon crash")
        snapshot = lab.snapshot()
        target_ids = {item["id"] for item in snapshot.get("targets", [])}
        if "local-renamed" not in target_ids or "localhost" in target_ids:
            raise ScenarioFailure(f"config rename did not recover: {sorted(target_ids)}")
        lab.record_action("restart-validated", hook=hook)
        return

    title = f"hook-{hook[:18]}"

    if hook == "lifecycle_reservation_before_result_publication":
        start_daemon_lifecycle_session(lab, title)
        crash_and_restart_daemon(lab, hook, port, 1)
        wait_session(lab, title, {"running"})
        lab.record_action("restart-validated", hook=hook)
        return

    start_session(lab, title)

    if hook == "journal_append_before_snapshot_publication":
        pid = wait_hook(lab, hook)
        kill_hook_owner(lab, hook, pid)
        # The create may recover the target or fail safely. It must settle,
        # retain a responsive daemon, and leave SQLite internally consistent.
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline:
            snapshot = lab.snapshot()
            session = next(
                (item for item in snapshot.get("sessions", []) if item.get("title") == title),
                None,
            )
            if session is None or session.get("state") in {"running", "error", "stopped"}:
                break
            time.sleep(0.1)
        else:
            raise ScenarioFailure("worker journal crash left provisioning permanently active")
        lab.record_action("restart-validated", hook=hook)
        return

    session = wait_session(lab, title, {"running"})
    session_id = str(session["id"])

    if hook == "relay_projection_before_revision_publication":
        crash_and_restart_daemon(lab, hook, port, 1)
        wait_session(lab, title, {"running"})
        lab.record_action("restart-validated", hook=hook)
        return

    if hook == "checkpoint_archive_before_database_publication":
        prompt = f"checkpoint hook {lab.seed}"
        status, _ = lab.request(
            "POST",
            "/api/actions",
            {"action": "prompt", "session_id": session_id, "text": prompt},
        )
        if status != 202:
            raise ScenarioFailure(f"prompt action returned {status}")
        crash_and_restart_daemon(lab, hook, port, 1)
        wait_session(lab, title, {"running", "stopped"})
        deadline = time.monotonic() + TIMEOUT
        while time.monotonic() < deadline:
            status, transcript = lab.request("GET", f"/api/conversations/{session_id}")
            if status == 200 and isinstance(transcript, dict):
                lines = [
                    line
                    for entry in transcript.get("entries", [])
                    for line in entry.get("lines", [])
                ]
                if lines.count(prompt) == 1:
                    break
            time.sleep(0.1)
        else:
            raise ScenarioFailure("acknowledged prompt did not survive checkpoint-boundary crash")
        lab.record_action("restart-validated", hook=hook)
        return

    raise ScenarioFailure(f"unimplemented hook scenario {hook}")


def finish(lab: Lab) -> None:
    snapshot = lab.snapshot()
    for session in snapshot.get("sessions", []):
        if session.get("state") == "stopped":
            continue
        session_id = str(session["id"])
        status, _ = lab.request(
            "POST", "/api/actions", {"action": "close", "session_id": session_id}
        )
        if status != 202:
            raise ScenarioFailure(f"close action for {session_id} returned {status}")
        lab.wait_snapshot(
            lambda value, selected=session_id: any(
                item.get("id") == selected and item.get("state") == "stopped"
                for item in value.get("sessions", [])
            ),
            f"stopped session {session_id}",
        )
    for client in lab.clients:
        client.terminate()
    if lab.daemon_pid is not None:
        lab.stop_daemon()
    lab.integrity()
    leaks = lab.owned_pids()
    if leaks:
        raise ScenarioFailure(f"owned processes remained after cleanup: {leaks}")
    lab.capture_process_tree()
    lab.trace["finished_at"] = lab.timestamp()
    lab.trace["outcome"] = "passed"
    lab.write_trace()


def run_one(binary: pathlib.Path, hook: str, seed: int) -> None:
    lab = Lab(binary, f"test-hook-{hook}", seed)
    lab.hook_name = hook
    print(f"hook-chaos: hook={hook} artifacts={lab.root}", flush=True)
    try:
        run_hook(lab, hook)
        finish(lab)
    except BaseException as error:
        lab.trace["finished_at"] = lab.timestamp()
        lab.trace["outcome"] = "failed"
        lab.trace["failure"] = str(error)
        lab.write_trace()
        lab.cleanup_owned()
        with contextlib.suppress(Exception):
            lab.integrity()
        lab.capture_process_tree()
        lab.preserve_runtime()
        lab.remove_runtime()
        print(f"hook-chaos: failed hook={hook}: {error}", file=sys.stderr)
        raise
    lab.preserve_runtime()
    lab.remove_runtime()
    print(f"hook-chaos: passed hook={hook} leaks=0", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--hook", choices=["all", *HOOKS], default="all")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("hel", type=pathlib.Path)
    args = parser.parse_args()
    if os.environ.get("MJ_CHAOS_ISOLATED") != "1":
        parser.error("set MJ_CHAOS_ISOLATED=1 only for a disposable test root")
    selected = HOOKS if args.hook == "all" else [args.hook]
    for index, hook in enumerate(selected):
        run_one(args.hel.resolve(), hook, args.seed + index)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
