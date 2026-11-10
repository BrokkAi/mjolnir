#!/usr/bin/env python3
"""Live acceptance for terminal draft durability, recovery, and quit ordering."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import sqlite3
import sys
import time
from typing import Any

from reliability_lab import TIMEOUT_SCALE, Lab, ScenarioFailure
from tui_components_chat import COMPOSER_BORDER
from tui_components_tmux import Evidence, TmuxController, locate_text


REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
QUIT_FIELDS = {
    "dashboard_drop_ms",
    "attachment_shutdown_ms",
    "detach_connect_ms",
    "detach_request_ms",
}
QUIT_MEASUREMENT_FIELDS = {"total_ms", "cleanup_ms"}


class InstanceLab(Lab):
    """Keep even one-shot CLI helpers on this scenario's named instance."""

    def __init__(self, hel: pathlib.Path, scenario: str, seed: int, instance: str):
        self.instance = instance
        super().__init__(hel, scenario, seed)

    def environment(self) -> dict[str, str]:
        environment = super().environment()
        environment["MJ_INSTANCE"] = self.instance
        return environment


def read_draft_rows(lab: Lab) -> list[dict[str, Any]]:
    database = lab.data / "mj.sqlite3"
    connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
    try:
        rows = connection.execute(
            "SELECT draft_id, session_id, source, owner_pid, text, recovered_at "
            "FROM detached_drafts WHERE workspace_id = ? ORDER BY draft_id",
            (lab.workspace_id,),
        ).fetchall()
    finally:
        connection.close()
    return [
        {
            "id": row[0],
            "session_id": row[1],
            "source": row[2],
            "owner_pid": row[3],
            "text": row[4],
            "recovered_at": row[5],
        }
        for row in rows
    ]


def visible_drafts(lab: Lab) -> list[dict[str, Any]]:
    response = lab.daemon_request(
        {"action": "snapshot", "arguments": {"workspace_id": lab.workspace_id}}
    )
    value = response.get("value")
    if response.get("reply") != "snapshot" or not isinstance(value, dict):
        raise ScenarioFailure(f"workspace snapshot returned an unexpected reply: {response!r}")
    drafts = value.get("drafts")
    if not isinstance(drafts, list):
        raise ScenarioFailure(f"workspace snapshot omitted drafts: {value!r}")
    return drafts


def active_rows(lab: Lab, session_id: str, owner_pid: int) -> list[dict[str, Any]]:
    return [
        row
        for row in read_draft_rows(lab)
        if row["session_id"] == session_id
        and row["owner_pid"] == owner_pid
        and row["text"] != ""
        and row["recovered_at"] is None
    ]


def wait_for_draft(
    lab: Lab,
    tmux: TmuxController,
    session_id: str,
    owner_pid: int,
    expected_text: str,
    *,
    expected_id: str | None = None,
) -> dict[str, Any]:
    deadline = time.monotonic() + 20.0 * TIMEOUT_SCALE
    last = []
    while time.monotonic() < deadline:
        last = active_rows(lab, session_id, owner_pid)
        snapshots = visible_drafts(lab)
        visible_ids = {
            row.get("id")
            for row in snapshots
            if row.get("session_id") == session_id and row.get("owner_pid") == owner_pid
        }
        if (
            len(last) == 1
            and last[0]["text"] == expected_text
            and last[0]["id"] in visible_ids
            and (expected_id is None or last[0]["id"] == expected_id)
        ):
            return last[0]
        time.sleep(0.05)
    raise ScenarioFailure(
        f"draft did not become durable for pid={owner_pid}, session={session_id}: "
        f"expected {expected_text!r}, rows={last!r}, pane={tmux.capture()[-3000:]!r}"
    )


def assert_retired(lab: Lab, session_id: str, owner_pid: int, draft_id: str) -> None:
    deadline = time.monotonic() + 20.0 * TIMEOUT_SCALE
    while time.monotonic() < deadline:
        active = active_rows(lab, session_id, owner_pid)
        snapshots = visible_drafts(lab)
        visible_ids = {
            row.get("id")
            for row in snapshots
            if row.get("session_id") == session_id and row.get("owner_pid") == owner_pid
        }
        tombstoned = any(
            row["id"] == draft_id
            and (row["recovered_at"] is not None or row["text"] == "")
            for row in read_draft_rows(lab)
        )
        if not active and draft_id not in visible_ids and tombstoned:
            return
        time.sleep(0.05)
    raise ScenarioFailure(
        f"draft {draft_id} was not retired and hidden; active={active!r}, "
        f"visible_ids={visible_ids!r}"
    )


def click_text(tmux: TmuxController, label: str, *, last: bool = False) -> None:
    screen = tmux.wait_for(label)
    time.sleep(0.3 * TIMEOUT_SCALE)
    x, y = locate_text(screen, label, last=last)
    tmux.mouse_click(x + 2, y)


def open_session(tmux: TmuxController, session_id: str, title: str) -> None:
    click_text(tmux, title)
    tmux.send_key("Enter")
    tmux.wait_until(
        lambda: tmux.capture().count(title) >= 2 and COMPOSER_BORDER in tmux.capture(),
        f"{title!r} selected in the conversation pane",
    )
    screen = tmux.capture()
    x, y = locate_text(screen, COMPOSER_BORDER, last=True)
    tmux.mouse_click(x + 2, y + 1)


def session_title(lab: Lab, session_id: str) -> str:
    row = next(
        (item for item in lab.snapshot().get("sessions", []) if item.get("id") == session_id),
        None,
    )
    if row is None:
        raise ScenarioFailure(f"session {session_id} disappeared from the viewer snapshot")
    return str(row.get("title") or row.get("name") or session_id)


def session_workspace_id(lab: Lab, session_id: str) -> str:
    connection = sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True)
    try:
        row = connection.execute(
            "SELECT workspace_id FROM session_contexts WHERE session_id = ?",
            (session_id,),
        ).fetchone()
    finally:
        connection.close()
    if row is None:
        raise ScenarioFailure(f"session {session_id} has no workspace context")
    return str(row[0])


def rename_session(lab: Lab, session_id: str, title: str) -> None:
    lab.wait_snapshot(
        lambda value: any(
            item.get("id") == session_id
            and item.get("state") == "running"
            and item.get("capabilities", {}).get("rename")
            for item in value.get("sessions", [])
        ),
        f"session {session_id} running and rename-capable",
    )
    status, response = lab.request(
        "POST",
        "/api/actions",
        {"action": "rename", "session_id": session_id, "title": title},
    )
    if status != 202:
        raise ScenarioFailure(f"fixture rename returned {status}: {response!r}")
    lab.wait_snapshot(
        lambda value: any(
            item.get("id") == session_id and item.get("title") == title
            for item in value.get("sessions", [])
        ),
        f"session {session_id} renamed to {title}",
    )


def create_session(lab: Lab, tmux: TmuxController, evidence: Evidence) -> str:
    before = {row["id"] for row in lab.snapshot().get("sessions", [])}
    # The empty workspace initially focuses the conversation pane. Creation
    # is routed from Sessions, so focus that pane before using its shortcut.
    click_text(tmux, "Sessions")
    tmux.send_prefix("c")
    tmux.wait_for("New session · 1/4 profile")
    tmux.send_key("Home")
    tmux.send_key("Enter")
    tmux.wait_for("New session · 2/4 target")
    click_text(tmux, "  Next  ")
    tmux.wait_for("New session · 3/4 local project")
    tmux.send_key("C-u")
    tmux.send_key("Enter")
    tmux.wait_for("Project directory cannot be empty")
    tmux.send_text("relative")
    tmux.send_key("Enter")
    tmux.wait_for("Enter an absolute path or a path starting with ~/")
    tmux.send_key("C-u")
    tmux.send_raw("\x1b[200~" + str(lab.project) + "\x1b[201~")
    tmux.wait_for(str(lab.project))
    click_text(tmux, "  Next  ")
    tmux.wait_for("New session · 4/4 review")
    click_text(tmux, "  Back  ")
    tmux.wait_for("New session · 3/4 local project")
    click_text(tmux, "  Next  ")
    tmux.wait_for("New session · 4/4 review")
    tmux.wait_for("Delegation")
    tmux.wait_for("Set up Mjolnir sub-agents")
    click_text(tmux, "Native")
    tmux.wait_for(" Values ")
    tmux.send_key("Escape")
    click_text(tmux, "  Create  ")
    created = lab.wait_snapshot(
        lambda value: any(row["id"] not in before for row in value.get("sessions", [])),
        "Create creates a durable session",
    )
    added = [row for row in created.get("sessions", []) if row["id"] not in before]
    if len(added) != 1:
        raise ScenarioFailure(f"Create produced {len(added)} sessions")
    screen = tmux.capture()
    evidence.event(
        "draft-session-created",
        "open the create wizard with Ctrl+B, c and create a fixture session",
        "one session is visible in the daemon snapshot",
        added[0]["id"],
        evidence.capture("draft-session-created", screen),
    )
    return added[0]["id"]


def start_client(
    lab: Lab,
    binary: pathlib.Path,
    instance: str,
    name: str,
) -> TmuxController:
    home = lab.runtime_root / f"home-{name}"
    tmp = lab.runtime_root / f"tmp-{name}"
    home.mkdir()
    tmp.mkdir()
    environment = lab.environment()
    for key in (
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "KIMI_API_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ):
        environment.pop(key, None)
    environment.update(
        {
            "HOME": str(home),
            "CODEX_HOME": str(lab.profile),
            "MJ_INSTANCE": instance,
            "MJ_WORKER_BINARY": os.environ["MJ_WORKER_BINARY"],
            "TMPDIR": str(tmp),
            "TERM": "xterm-256color",
            "LC_ALL": "C.UTF-8",
            "PATH": f"{lab.runtime_root / 'bin'}:/usr/bin:/bin",
        }
    )
    config = lab.root / f"tmux-{name}.conf"
    config.write_text(
        "set-option -g status off\n"
        "set-option -g mouse off\n"
        "set-option -g history-limit 2000\n"
        "set-option -g default-terminal xterm-256color\n"
    )
    tmux = TmuxController(lab.runtime_root / f"tmux-{name}.sock", config, environment, binary)
    tmux.start(f"draft-{name}", 140, 40)
    tmux.wait_for("Sessions")
    return tmux


def wait_for_quit_log(lab: Lab, pid: int) -> dict[str, int]:
    deadline = time.monotonic() + 15.0 * TIMEOUT_SCALE
    pattern = re.compile(r"\b([a-z_]+_ms)=(\d+)\b")
    while time.monotonic() < deadline:
        lines = []
        for path in (lab.data / "logs").glob(f"mj-tui-*-{pid}.log"):
            try:
                lines.extend(path.read_text(errors="replace").splitlines())
            except OSError:
                continue
        matches = [line for line in lines if "the dashboard exited" in line]
        for line in reversed(matches):
            fields = {name: int(value) for name, value in pattern.findall(line)}
            if (
                QUIT_FIELDS | QUIT_MEASUREMENT_FIELDS <= fields.keys()
                and "quit_requested=true" in line
            ):
                return fields
        time.sleep(0.05)
    raise ScenarioFailure(
        f"TUI pid {pid} exited without the requested-quit timing fields "
        f"{sorted(QUIT_FIELDS | QUIT_MEASUREMENT_FIELDS)}"
    )


def setup_fixture(lab: Lab) -> int:
    port = lab.prepare(fake_acp_delay_ms=300)
    fixture_bin = lab.runtime_root / "bin"
    for name, script in {
        "node": "#!/bin/sh\n[ \"$1\" = -e ] && exit 0\nexit 1\n",
        "npm": "#!/bin/sh\n[ \"$1\" = --version ] && { echo 10.0.0; exit 0; }\nexit 1\n",
    }.items():
        executable = fixture_bin / name
        executable.write_text(script)
        executable.chmod(0o700)
    config_path = lab.config / "config.toml"
    config_text = config_path.read_text()
    profile = config_text.split("[profiles.fake]", 1)[1].split("[bundles.fixture]", 1)[0]
    reviewer_home = lab.runtime_root / "reviewer"
    reviewer_home.mkdir()
    profile = profile.replace(json.dumps(str(lab.profile)), json.dumps(str(reviewer_home)))
    config_path.write_text(
        config_text
        + "\n[profiles.reviewer]\n"
        + profile
        + '\n[review]\nenabled = false\nprofile = "reviewer"\n\n[quota]\nruntime = "host"\n'
    )
    (lab.runtime_root / "fake_acp.py").write_text(
        (REPO_ROOT / "tests/e2e/tui_components_acp.py").read_text()
    )
    return port


def run(args: argparse.Namespace) -> int:
    artifact_parent = pathlib.Path(os.environ["MJ_RELIABILITY_ARTIFACTS"]).expanduser().resolve()
    target = (REPO_ROOT / "target").resolve()
    if artifact_parent == target or artifact_parent.is_relative_to(target):
        raise ScenarioFailure("MJ_RELIABILITY_ARTIFACTS must be outside the repository target directory")
    binary = args.hel.resolve()
    worker = args.worker.resolve()
    for label, path in (("mj", binary), ("mj-worker", worker)):
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ScenarioFailure(f"{label} executable is missing or not executable: {path}")
    os.environ["MJ_WORKER_BINARY"] = str(worker)
    instance = f"tui-quit-persistence-{args.seed}-{os.getpid()}"
    lab = InstanceLab(binary, "tui-quit-persistence", args.seed, instance)
    evidence = Evidence(lab.root, binary, args.seed, ((140, 40),))
    lab.record_action("instance", name=f"tui-quit-persistence-{args.seed}-{os.getpid()}")
    tmux_clients: dict[str, TmuxController] = {}
    result = 1
    try:
        port = setup_fixture(lab)
        first = start_client(lab, binary, instance, "one")
        tmux_clients["one"] = first
        initial_screen = first.capture()
        evidence.event(
            "dashboard-ready",
            "start real mj in a private tmux server",
            "dashboard and Sessions pane rendered",
            "dashboard ready",
            evidence.capture("dashboard-ready", initial_screen),
        )
        code, _ = lab.wait_daemon_status(port)
        status, _ = lab.request("POST", "/auth/session", {"code": code})
        if status != 204:
            raise ScenarioFailure(f"fixture viewer login returned {status}")
        first_id = create_session(lab, first, evidence)
        lab.workspace_id = session_workspace_id(lab, first_id)
        rename_session(lab, first_id, f"draft-session-{args.seed}-one")
        first_title = session_title(lab, first_id)
        open_session(first, first_id, first_title)

        # Create a second navigation target before entering any test drafts.
        second_id = create_session(lab, first, evidence)
        if session_workspace_id(lab, second_id) != lab.workspace_id:
            raise ScenarioFailure("fixture sessions were created in different workspaces")
        rename_session(lab, second_id, f"draft-session-{args.seed}-two")
        second_title = session_title(lab, second_id)
        open_session(first, first_id, first_title)

        second = start_client(lab, binary, instance, "two")
        tmux_clients["two"] = second
        second_pid = second.pane_pid()
        open_session(second, first_id, first_title)
        first_pid = first.pane_pid()
        if first_pid == second_pid:
            raise ScenarioFailure("two TUI clients unexpectedly share one pane process")

        marker_one = f"draft-one-{args.seed}"
        first.send_text(marker_one)
        first.wait_for(marker_one)
        row_one = wait_for_draft(lab, first, first_id, first_pid, marker_one)
        first.send_text("-updated")
        first.wait_for(marker_one + "-updated")
        row_one = wait_for_draft(
            lab, first, first_id, first_pid, marker_one + "-updated", expected_id=row_one["id"]
        )

        marker_two = f"draft-two-{args.seed}"
        second.send_text(marker_two)
        second.wait_for(marker_two)
        row_two = wait_for_draft(lab, second, first_id, second_pid, marker_two)
        if row_one["id"] == row_two["id"]:
            raise ScenarioFailure("independent clients shared one recovery entry")

        # Switch conversations without typing; each client must retain its own
        # existing row and update it in place rather than create a duplicate.
        open_session(first, second_id, second_title)
        open_session(first, first_id, first_title)
        row_one = wait_for_draft(
            lab, first, first_id, first_pid, marker_one + "-updated", expected_id=row_one["id"]
        )
        if len(active_rows(lab, first_id, first_pid)) != 1:
            raise ScenarioFailure("unchanged navigation duplicated client one's recovery entry")
        open_session(second, second_id, second_title)
        open_session(second, first_id, first_title)
        row_two = wait_for_draft(
            lab, second, first_id, second_pid, marker_two, expected_id=row_two["id"]
        )

        # Clearing and then submitting retire only the current client's row.
        first.send_key("C-u")
        assert_retired(lab, first_id, first_pid, row_one["id"])
        sent = f"submitted-draft-{args.seed}"
        first.send_text(sent)
        first.wait_for(sent)
        pending = wait_for_draft(
            lab, first, first_id, first_pid, sent, expected_id=row_one["id"]
        )
        first.send_key("Enter")
        first.wait_for(f"reliability reply: {sent}", "fake ACP completion")
        assert_retired(lab, first_id, first_pid, pending["id"])
        wait_for_draft(lab, second, first_id, second_pid, marker_two, expected_id=row_two["id"])

        # Start from the acknowledged row, then quit immediately after a new
        # edit. The screen assertion is not a persistence poll; a fast daemon
        # may still ACK before the quit chord reaches it.
        quitting = f"{sent}-quit-suffix"
        first.send_text(quitting)
        first.wait_for(quitting)
        first.send_key("C-b")
        first.send_key("q")
        first.wait_until(lambda: not first.has_session(), "TUI quit")
        timings = wait_for_quit_log(lab, first_pid)
        after_quit = active_rows(lab, first_id, first_pid)
        if len(after_quit) != 1 or after_quit[0]["id"] != pending["id"]:
            raise ScenarioFailure(f"quit created or lost recovery rows: {after_quit!r}")
        if after_quit[0]["text"] != quitting:
            raise ScenarioFailure(f"quit did not persist the latest edit: {after_quit!r}")
        wait_for_draft(lab, second, first_id, second_pid, marker_two, expected_id=row_two["id"])
        evidence.event(
            "quit-draft-durable",
            "type a new suffix and immediately press Ctrl+B, q without polling the store",
            "the final text is durable in the acknowledged row and quit timing fields are logged",
            json.dumps({"draft_id": after_quit[0]["id"], "quit_timings": timings}, sort_keys=True),
        )

        recovery = lab.daemon_request(
            {"action": "recover_draft", "arguments": {"draft_id": pending["id"]}}
        )
        if recovery.get("reply") != "done":
            raise ScenarioFailure(f"daemon draft recovery returned {recovery!r}")
        assert_retired(lab, first_id, first_pid, pending["id"])

        reopened = start_client(lab, binary, instance, "reopened")
        tmux_clients["reopened"] = reopened
        open_session(reopened, first_id, first_title)
        reopened.wait_for(quitting, "recovered prompt in reopened conversation")
        remaining = wait_for_draft(lab, second, first_id, second_pid, marker_two, expected_id=row_two["id"])
        if remaining["text"] != marker_two:
            raise ScenarioFailure("recovering client one's draft changed client two's draft")

        evidence.finish("passed")
        lab.trace["outcome"] = "passed"
        lab.trace["finished_at"] = lab.timestamp()
        lab.write_trace()
        result = 0
    except BaseException as error:
        evidence.finish("failed", str(error))
        lab.trace["outcome"] = "failed"
        lab.trace["failure"] = str(error)
        lab.trace["finished_at"] = lab.timestamp()
        lab.write_trace()
        print(f"tui quit persistence: failed: {error}", file=sys.stderr)
    finally:
        cleanup_errors = []
        for name, tmux in tmux_clients.items():
            try:
                if tmux.session is not None:
                    tmux.release()
            except Exception as error:
                cleanup_errors.append(f"tmux {name}: {error}")
        for label, operation in (
            ("daemon", lab.stop_daemon),
            ("owned processes", lab.cleanup_owned),
            ("process capture", lab.capture_process_tree),
            ("integrity", lab.integrity),
            ("preserve runtime", lab.preserve_runtime),
        ):
            try:
                operation()
            except Exception as error:
                cleanup_errors.append(f"{label}: {error}")
        if not cleanup_errors:
            try:
                lab.remove_runtime()
            except Exception as error:
                cleanup_errors.append(f"remove runtime: {error}")
        if cleanup_errors:
            evidence.finish("failed", "; ".join(cleanup_errors))
            print(f"tui quit persistence cleanup failed: {'; '.join(cleanup_errors)}", file=sys.stderr)
            result = 1
    return result


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", required=True, type=int)
    parser.add_argument("--hel", required=True, type=pathlib.Path, help="copied real mj executable")
    parser.add_argument("--worker", required=True, type=pathlib.Path, help="copied real mj-worker executable")
    return parser.parse_args()


if __name__ == "__main__":
    try:
        raise SystemExit(run(parse_args()))
    except KeyError as error:
        print(f"tui quit persistence setup gap: missing environment variable {error}", file=sys.stderr)
        raise SystemExit(2)
