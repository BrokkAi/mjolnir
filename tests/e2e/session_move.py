#!/usr/bin/env python3
"""Exercise Move with real local workers/Git and an isolated deterministic ACP."""

import argparse
import os
import json
import pathlib
import sqlite3
import sys
import time
import tomllib
import traceback

import reliability_lab
from reliability_lab import Lab, ScenarioFailure


def run(lab):
    port = lab.prepare(fake_acp_prompt_delay_ms=60_000)
    config_path = lab.config / "config.toml"
    config = config_path.read_text()
    second_home = lab.runtime_root / "second-profile"
    second_home.mkdir()
    profile = config.split("[profiles.fake]", 1)[1].split("[bundles.fixture]", 1)[0]
    profile = profile.replace(str(lab.profile), str(second_home)).replace('"60000"', '"0"')
    config_path.write_text(config + "\n[profiles.destination]\n" + profile + '\n[targets.destination]\nkind = "bare"\n' + '\n[subagents.straightforward]\nmodel = "fixture-straightforward"\neffort = "low"\n[subagents.subtle]\nmodel = "fixture-subtle"\neffort = "high"\n')
    bridge = lab.runtime_root / "fake_acp.py"
    options = [
        {"id":"model", "name":"Model", "category":"model", "type":"select", "currentValue":"fixture-straightforward", "options":[{"value":model,"name":model} for model in ["fixture-straightforward","fixture-subtle"]]},
        {"id":"effort", "name":"Effort", "category":"thought_level", "type":"select", "currentValue":"low", "options":[{"value":effort,"name":effort} for effort in ["low","high"]]},
    ]
    script = bridge.read_text().replace('for line in sys.stdin:', 'config_options = ' + repr(options) + '\nfor line in sys.stdin:')
    script = script.replace('            "sessionId": session_id,\n            "modes":', '            "sessionId": session_id,\n            "configOptions": config_options,\n            "modes":')
    script = script.replace('    elif method == "session/set_mode":', '    elif method == "session/set_config_option":\n        params = message["params"]\n        for option in config_options:\n            if option["id"] == params["configId"]:\n                option["currentValue"] = params["value"]\n        result = {"configOptions": config_options}\n    elif method == "session/set_mode":')
    compile(script, str(bridge), "exec")
    bridge.write_text(script)
    client = lab.start_tui("tui-1")
    client.wait_for("Workspaces")
    client.send(b"\r\r")
    client.wait_for("Sessions")
    # Detaching joins the TUI's discovery task before the fixture edits its
    # profile list; later CLI/API actions cannot import host profiles again.
    client.quit()
    # Setup discovers host installations. Delegation probes every enabled
    # profile, so keep this scenario on its two deterministic ACP profiles.
    configured = config_path.read_text()
    for profile_id in tomllib.loads(configured)["profiles"]:
        if profile_id not in {"fake", "destination"}:
            section = f"[profiles.{profile_id}]\n"
            configured = configured.replace(section, section + "enabled = false\n")
    config_path.write_text(configured)
    code, _ = lab.wait_daemon_status(port)
    assert lab.request("POST", "/auth/session", {"code": code})[0] == 204
    workspace = lab.snapshot()["workspaces"][0]["id"]
    created = lab.request("POST", "/api/actions", {
        "action": "new", "workspace_id": workspace, "profile_id": "fake",
        "bundle_id": "fixture", "target_id": "localhost", "title": "move-acceptance",
        "project_directory": str(lab.project),
    })
    assert created[0] == 202, created
    snapshot = lab.wait_snapshot(lambda s: any(row.get("state") == "running" for row in s.get("sessions", [])), "new worker ready")
    session_id = next(row["id"] for row in snapshot["sessions"] if row["state"] == "running")

    def record():
        reply = lab.daemon_request({"action": "session_record", "arguments": {"session_id": session_id}})
        assert reply["value"] is not None, reply
        return reply["value"]

    original = record()
    checkout = pathlib.Path(original["project_directory"])
    content = "recoverable uncommitted content\n" * 4096
    (checkout / "move-untracked.txt").write_text(content)
    (checkout / "move-committed.txt").write_text("committed during the session\n")
    lab.git_output(["add", "move-committed.txt"], cwd=checkout)
    lab.git_output(["commit", "-m", "Move acceptance fixture"], cwd=checkout)
    expected_head = lab.git_output(["rev-parse", "HEAD"], cwd=checkout).strip()
    (checkout / "move-committed.txt").write_text("staged changes\n")
    lab.git_output(["add", "move-committed.txt"], cwd=checkout)
    (checkout / "README.md").write_text("unstaged changes\n")
    expected_status = lab.git_output(["status", "--porcelain"], cwd=checkout)

    def prompt(text):
        lab.wait_snapshot(lambda s: not (lab.session(s, session_id) or {}).get("operation"), "previous web action settled")
        response = lab.request("POST", "/api/actions", {"action": "prompt", "session_id": session_id, "text": text})
        assert response[0] == 202, (text, response)

    def wait_queued(count):
        lab.wait_snapshot(lambda s: len((lab.session(s, session_id) or {}).get("queued_prompts", [])) == count
                          and not (lab.session(s, session_id) or {}).get("operation"), "queued command admission")

    def move(profile_id, target, queue, selectors="both", subagents=None):
        started = time.monotonic()
        expected_policy = ({"mode": subagents} if subagents is not None
                           else record().get("subagents") or {"mode": "native"})
        flags = []
        if subagents is not None:
            flags += ["--subagents", subagents]
        if selectors != "target":
            flags += ["--profile", profile_id]
        if selectors != "profile":
            flags += ["--target", target]
        if selectors == "profile":
            flags += ["--clear-resources"]  # The TUI's historical bare-host request.
        # A Move that follows another operation closely can meet the automatic
        # checkpoint of the turn that just finished, which holds the session's
        # lifecycle lease for a moment. The refusal names it; ask again.
        deadline = time.monotonic() + 30
        while True:
            result = lab.command("move", "--session", session_id, *flags,
                                 "--queue", queue, "--yes", "--json", timeout=90, check=False)
            if result.returncode == 0:
                break
            if "already has a lifecycle operation" not in result.stderr or time.monotonic() > deadline:
                raise ScenarioFailure(f"Hel move failed ({result.returncode}): {result.stderr.strip()}")
            time.sleep(0.5)
        outcome = json.loads(result.stdout)
        assert outcome["outcome"] == "completed", outcome
        assert outcome["session_id"] == session_id
        current = record()
        assert current["last_profile"] == profile_id
        assert current["target_template_id"] == target
        assert current["workspace_id"] == original["workspace_id"]
        assert current["subagents"] == expected_policy, current
        launch_path = pathlib.Path(current["target"]["worker_root"]) / "launch.json"
        assert json.loads(launch_path.read_text())["subagents"] == expected_policy
        assert current["native_session_id"] == original["native_session_id"]
        assert pathlib.Path(current["project_directory"], "move-untracked.txt").read_text() == content
        restored = pathlib.Path(current["project_directory"])
        assert lab.git_output(["rev-parse", "HEAD"], cwd=restored).strip() == expected_head
        assert lab.git_output(["status", "--porcelain"], cwd=restored) == expected_status
        lab.record_action("move", operation_id=outcome["operation_id"], queue=queue,
                          elapsed_seconds=round(time.monotonic() - started, 3))

    prompt("interrupted-discard")
    lab.wait_snapshot(lambda s: (lab.session(s, session_id) or {}).get("chat_phase") == "running", "active source turn")
    prompt("discard-this-pending-prompt")
    wait_queued(1)
    move("destination", "destination", "discard", subagents="mjolnir")
    lab.wait_snapshot(lambda s: (lab.session(s, session_id) or {}).get("chat_phase") == "idle", "idle destination")
    for task_type, model, effort in [("straightforward", "fixture-straightforward", "low"), ("subtle", "fixture-subtle", "high")]:
        status, response = lab.request("POST", f"/api/v1/sessions/{session_id}/subagents", {
            "task_name": task_type, "instructions": "Verify configured delegation.", "task_type": task_type,
        })
        assert status == 201, (status, response)
        child = response["session"]["id"]
        with sqlite3.connect(lab.data / "mj.sqlite3") as database:
            selected = database.execute("SELECT json_extract(record_json, '$.model'), json_extract(record_json, '$.effort') FROM subagent_sessions WHERE child_session_id=?", (child,)).fetchone()
        assert selected == (model, effort), (task_type, selected)
        lab.command("destroy", "--session", child)
        lab.wait_snapshot(lambda _: lab.daemon_request({
            "action": "session_record", "arguments": {"session_id": child},
        })["value"] is None, "sub-agent destruction completes")
    logs = (lab.runtime_root / "fake-acp.log").read_text()
    assert "discard-this-pending-prompt" not in logs, "discarded prompt ran on a harness"
    # A profile-only move goes back to the deliberately slow source profile.
    # Target, attached directories, and resource sizing are unchanged, so the
    # move replaces only the harness: the same worker root is reused, nothing
    # under it is torn down, and untracked files stay where they are.
    before_swap = record()
    swap_locator = before_swap["target"]
    assert swap_locator["kind"] == "local-bare", swap_locator
    worker_root = pathlib.Path(swap_locator["worker_root"])
    survivor = pathlib.Path(before_swap["project_directory"]) / "in-place-survivor.txt"
    survivor_content = "survives an in-place harness swap\n" * 4096
    survivor.write_text(survivor_content)
    ignored = checkout / "ignored-build-output"
    excludes = pathlib.Path(lab.git_output(["rev-parse", "--path-format=absolute", "--git-path", "info/exclude"], cwd=checkout).strip())
    with excludes.open("a") as handle:
        handle.write("\nignored-build-output\n")
    ignored.write_bytes(b"warm build cache")
    checkout_identity = checkout.stat().st_ino

    # The survivor is untracked, so the status the move must preserve now
    # includes it.
    expected_status = lab.git_output(["status", "--porcelain"], cwd=checkout)
    daemon_log = lab.data / "daemon.log"
    log_offset = daemon_log.stat().st_size if daemon_log.exists() else 0
    move("fake", "destination", "discard", "profile", subagents="native")
    after_swap = record()
    assert after_swap["target"] == swap_locator, (swap_locator, after_swap["target"])
    assert survivor.read_text() == survivor_content, "in-place move lost an untracked file"
    assert checkout.stat().st_ino == checkout_identity, "profile switch recreated its checkout"
    assert ignored.read_bytes() == b"warm build cache", "profile switch lost ignored build output"

    assert after_swap["native_session_id"] == before_swap["native_session_id"]
    ownership = json.loads((worker_root / "ownership.json").read_text())
    assert ownership.get("profile_id") == "fake", ownership
    assert ownership.get("session_id") == session_id, ownership
    with daemon_log.open() as handle:
        handle.seek(log_offset)
        swap_log = handle.read()
    # The log window covers only this move, and the lab runs one session, so any
    # such line would be this session's worker state being removed.
    torn_down = [line for line in swap_log.splitlines()
                 if "remove exact local Mjolnir worker state" in line]
    assert not torn_down, torn_down

    # Restart must take the same bounded handoff as the compatible Move above.
    # A full checkpoint would reject this file; no workspace bytes need moving.
    oversized = checkout / "restart-large-build-output"
    with oversized.open("wb") as handle:
        handle.truncate(9 * 1024 * 1024 * 1024)
    staged_home = worker_root / "profile"
    staged_identity = staged_home.stat().st_ino
    native_marker = staged_home / "restart-retained-native-state"
    native_marker.write_text("retained native state\n")
    before_restart = record()
    restart_started = time.monotonic()
    lab.daemon_request({"action": "restart_session", "arguments": {
        "session_id": session_id,
    }}, timeout=90)
    lab.record_action("restart", elapsed_seconds=round(time.monotonic() - restart_started, 3))
    after_restart = record()
    assert after_restart["state"] == "running", after_restart
    assert after_restart["target"] == before_restart["target"]
    assert after_restart["native_session_id"] == before_restart["native_session_id"]
    assert after_restart.get("checkpoint") == before_restart.get("checkpoint")
    assert checkout.stat().st_ino == checkout_identity, "Restart recreated its checkout"
    assert staged_home.stat().st_ino == staged_identity, "Restart restaged its native home"
    assert native_marker.read_text() == "retained native state\n"
    assert oversized.stat().st_size == 9 * 1024 * 1024 * 1024
    assert ignored.read_bytes() == b"warm build cache", "Restart lost ignored build output"
    oversized.unlink()
    prompt("interrupted-start")
    lab.wait_snapshot(lambda s: (lab.session(s, session_id) or {}).get("chat_phase") == "running", "second active source turn")
    prompt("start-pending-one")
    wait_queued(1)
    prompt("start-pending-two")
    wait_queued(2)
    move("destination", "destination", "start")
    lab.wait_snapshot(lambda s: (lab.session(s, session_id) or {}).get("chat_phase") == "idle", "queued destination turns finish")
    logs = (lab.runtime_root / "fake-acp.log").read_text()
    assert logs.count('"text": "start-pending-one"') == 1, logs[-4000:]
    assert logs.count('"text": "start-pending-two"') == 1, logs[-4000:]
    assert logs.count("interrupted-discard") == 1, "interrupted prompt replayed"
    assert logs.count("interrupted-start") == 1, "interrupted prompt replayed"
    move("destination", "localhost", "discard", "target")
    # Same selections are an idempotent no-op, without checkpointing again.
    unchanged = json.loads(lab.command("move", "--session", session_id, "--profile", "destination", "--yes", "--json").stdout)
    assert unchanged["outcome"] == "unchanged", unchanged
    assert lab.request("POST", "/api/actions", {
        "action": "suspend", "session_id": session_id,
        "acknowledge_unpublished_work": True,
    })[0] == 202
    lab.wait_snapshot(lambda s: (lab.session(s, session_id) or {}).get("state") == "suspended", "final cleanup")
    lab.stop_daemon()
    lab.integrity()
    assert not lab.owned_pids(), "owned worker processes remained"


def main():
    # Cold installation copies a large debug worker before Move can be tested.
    reliability_lab.TIMEOUT = 90.0
    parser = argparse.ArgumentParser()
    parser.add_argument("--hel", required=True, type=pathlib.Path)
    parser.add_argument("--instance", default="subagent-move-redesign-test")
    parser.add_argument("--worker", required=True, type=pathlib.Path)
    args = parser.parse_args()
    if args.instance == "default":
        parser.error("use a separate named test instance")
    os.environ["MJ_INSTANCE"] = args.instance
    os.environ["MJ_WORKER_BINARY"] = str(args.worker.resolve())
    lab = Lab(args.hel, "session-move", 1)
    print(f"move acceptance artifacts: {lab.root}", flush=True)
    try:
        run(lab)
    except BaseException as error:
        traceback.print_exc()
        lab.trace["outcome"] = "failed"
        lab.trace["failure"] = str(error)
        lab.cleanup_owned()
        lab.preserve_runtime()
        print(f"move acceptance failed: {error}; runtime retained at {lab.runtime_root}", file=sys.stderr)
        return 1
    else:
        lab.trace["outcome"] = "passed"
        lab.preserve_runtime()
        lab.remove_runtime()
        print("move acceptance passed: real local-bare workers, deterministic ACP", flush=True)
        return 0
    finally:
        lab.trace["finished_at"] = lab.timestamp()
        lab.write_trace()


if __name__ == "__main__":
    raise SystemExit(main())
