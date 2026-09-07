#!/usr/bin/env python3
"""Exercise Move with real local workers/Git and an isolated deterministic ACP."""

import argparse
import json
import pathlib
import sys
import time
import traceback

from reliability_lab import Lab


def run(lab):
    port = lab.prepare(fake_acp_prompt_delay_ms=60_000)
    config_path = lab.config / "config.toml"
    config = config_path.read_text()
    second_home = lab.runtime_root / "second-profile"
    second_home.mkdir()
    profile = config.split("[profiles.fake]", 1)[1].split("[bundles.fixture]", 1)[0]
    profile = profile.replace(str(lab.profile), str(second_home)).replace('"60000"', '"0"')
    config_path.write_text(config + "\n[profiles.destination]\n" + profile + '\n[targets.destination]\nkind = "local-bare"\n')
    client = lab.start_tui("tui-1")
    client.wait_for("Workspaces")
    client.send(b"\r\r")
    client.wait_for("Sessions")
    code, _ = lab.wait_daemon_status(port)
    lab.base_url = f"http://127.0.0.1:{port}"
    assert lab.request("POST", "/auth/session", {"code": code})[0] == 204
    workspace = lab.snapshot()["workspaces"][0]["id"]
    assert lab.request("POST", "/api/actions", {
        "action": "new", "workspace_id": workspace, "profile_id": "fake",
        "bundle_id": "fixture", "target_id": "localhost", "title": "move-acceptance",
        "project_directory": str(lab.project),
    })[0] == 202
    snapshot = lab.wait_snapshot(lambda s: any(row.get("state") == "running" for row in s.get("sessions", [])), "new worker ready")
    session_id = next(row["id"] for row in snapshot["sessions"] if row["state"] == "running")

    def record():
        reply = lab.daemon_request({"action": "runtime_snapshot", "arguments": {"workspace_id": workspace, "after_revision": 0}})
        return next(row for row in reply["value"]["records"] if row["id"] == session_id)

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

    def move(profile_id, target, queue, selectors="both"):
        started = time.monotonic()
        flags = []
        if selectors != "target":
            flags += ["--profile", profile_id]
        if selectors != "profile":
            flags += ["--target", target]
        result = lab.command("move", "--session", session_id, *flags,
                             "--queue", queue, "--yes", "--json", timeout=90)
        outcome = json.loads(result.stdout)
        assert outcome["outcome"] == "completed", outcome
        assert outcome["session_id"] == session_id
        current = record()
        assert current["last_profile"] == profile_id
        assert current["target_template_id"] == target
        assert current["workspace_id"] == original["workspace_id"]
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
    move("destination", "destination", "discard")
    lab.wait_snapshot(lambda s: (lab.session(s, session_id) or {}).get("chat_phase") == "idle", "idle destination")
    logs = (lab.runtime_root / "fake-acp.log").read_text()
    assert "discard-this-pending-prompt" not in logs, "discarded prompt ran on a harness"
    # A profile-only move goes back to the deliberately slow source profile.
    move("fake", "destination", "discard", "profile")
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
    assert lab.request("POST", "/api/actions", {"action": "close", "session_id": session_id})[0] == 202
    lab.wait_snapshot(lambda s: (lab.session(s, session_id) or {}).get("state") == "stopped", "final cleanup")
    client.quit()
    lab.stop_daemon()
    lab.integrity()
    assert not lab.owned_pids(), "owned worker processes remained"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--hel", required=True, type=pathlib.Path)
    args = parser.parse_args()
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
