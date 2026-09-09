#!/usr/bin/env python3
"""Exercise workspace/session commands by mouse in an isolated real terminal.

Build mj and mj-worker, then run this script with tmux on PATH. Navigation and
submission primarily use SGR clicks; text fields use ordinary keyboard editing.
Evidence and captures are retained under target/reliability-artifacts.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import shutil
import sys
import time

from reliability_lab import Lab, ScenarioFailure
from tui_components_tmux import Evidence, REPO_ROOT, TmuxController, locate_text


def exercise(lab: Lab, tmux: TmuxController, evidence: Evidence, port: int) -> None:
    def click(label: str, *, last: bool = True) -> None:
        screen = tmux.wait_for(label)
        x, y = locate_text(screen, label, last=last)
        tmux.mouse_click(x + max(1, len(label) // 2), y)

    def absent(label: str) -> None:
        tmux.wait_until(lambda: label not in tmux.capture(), f"{label!r} dismissed")

    def record(label: str, interaction: str, expected: str | None = None) -> None:
        expected = expected or interaction
        screen = tmux.capture()
        evidence.event(label, interaction, expected, expected,
                       evidence.capture(label, screen))

    def name(value: str) -> None:
        tmux.send_key("C-a")
        tmux.send_key("C-k")
        tmux.send_raw("\x1b[200~" + value + "\x1b[201~")

    def workspace_manager() -> None:
        click("☰")
        tmux.wait_for("New workspace")
        tmux.wait_for("Current", "workspace manager finishes loading")

    def workspace_name(value: str) -> None:
        name(value)

    tmux.run("new-session", "-d", "-s", "mouse-commands", "-x", "140", "-y", "40",
             "-c", str(lab.project), "--", str(tmux.binary))
    tmux.session = "mouse-commands"
    screen = tmux.wait_for_any(
        ("Sessions", "No live session"),
        "combined dashboard after direct startup",
    )
    code, _ = lab.wait_daemon_status(port)
    lab.base_url = f"http://127.0.0.1:{port}"
    status, _ = lab.request("POST", "/auth/session", {"code": code})
    if status != 204:
        raise ScenarioFailure(f"fixture viewer login returned {status}")

    # Workspace management opens from the pinned hamburger. Rename the fixture,
    # verify that draft actions stay hidden without drafts, then create a tab.
    workspace_manager()
    click("Rename")
    tmux.wait_for("Workspaces · Rename")
    workspace_name("Mouse renamed")
    click("Save")
    lab.wait_snapshot(
        lambda value: any(row["name"] == "Mouse renamed" for row in value["workspaces"]),
        "workspace rename persisted",
    )
    tmux.wait_until(lambda: "Working:" not in tmux.capture(), "workspace rename completes in manager")
    click("Close")
    absent("New workspace")

    workspace_manager()
    if "Drafts" in tmux.capture():
        raise ScenarioFailure("workspace with no drafts exposed a Drafts action")
    record("workspace-recover-guard", "open workspace menu", "draft actions stay hidden when no detached drafts exist")
    click("Close")
    absent("New workspace")

    workspace_manager()
    click("New workspace")
    tmux.wait_for("Workspaces · New")
    workspace_name("Mouse secondary")
    click("Create")
    created_workspaces = lab.wait_snapshot(
        lambda value: any(row["name"] == "Mouse secondary" for row in value["workspaces"]),
        "workspace create persisted",
    )
    secondary_id = next(
        row["id"] for row in created_workspaces["workspaces"] if row["name"] == "Mouse secondary"
    )
    tmux.wait_for("Mouse secondary")
    absent("New workspace")
    record("workspace-created", "☰; New workspace; name; Create", "Create adds a durable workspace tab and selects it")

    # Confirm the selected tab through the manager's initial selection, rather
    # than waiting for a name that was already visible on an inactive tab.
    for label in ("Mouse renamed", "Mouse secondary"):
        click(label, last=False)
        workspace_manager()
        tmux.wait_for(f"{label}  Current", "workspace manager follows the clicked tab")
        click("Close")
        absent("New workspace")
    record("workspace-tab-selection", "click both workspace tabs", "the workspace manager confirms each selected tab")

    # Resume remains a top sidebar control even when there are no stopped rows.
    click("Resume")
    tmux.wait_for("Resume a session")
    click("Cancel")
    absent("Resume a session")
    record("resume-empty", "click Resume; click Cancel", "the empty resume dialog closes cleanly")

    before = {row["id"] for row in lab.snapshot()["sessions"]}
    click("Create")
    tmux.wait_for("New session · 1/4 profile")
    click("  Next  ")
    tmux.wait_for("New session · 2/4 target")
    click("  Next  ")
    tmux.wait_for("New session · 3/4 local project")
    name(str(lab.project))
    click("  Next  ")
    tmux.wait_for("New session · 4/4 review")
    click("  Create  ")
    absent("New session · 4/4 review")
    snapshot = lab.wait_snapshot(
        lambda value: any(
            row["id"] not in before and row["state"] == "running"
            for row in value["sessions"]
        ),
        "wizard session running",
    )
    added = [row for row in snapshot["sessions"] if row["id"] not in before]
    if len(added) != 1:
        raise ScenarioFailure(f"Create produced {len(added)} sessions")
    if added[0]["workspace_id"] != secondary_id:
        raise ScenarioFailure("tab selection did not scope the new session to Mouse secondary")
    record("wizard-session", "click Create; complete all four wizard steps", "explicit Create opens and completes the full session wizard")

    click(" ⋯ ", last=False)
    tmux.wait_for("Rename session")
    tmux.wait_until(lambda: "a session transition is in progress" not in tmux.capture(), "session commands become available after creation")
    click("Rename session")
    tmux.wait_for("› Rename session", "Rename selected in the palette")
    click("  Run  ")
    tmux.wait_for("╭ Rename session", "rename editor")
    name("Mouse session")
    click("  Save  ")
    lab.wait_snapshot(lambda value: any(row.get("title") == "Mouse session" for row in value["sessions"]), "session renamed")
    record("session-actions", "session row menu renames the clicked session")

    # Collapse support panes using their existing title controls before the
    # width-boundary checks; the combined surface requires 80 columns.
    for title in ("Targets", "Quota"):
        screen = tmux.wait_for(title)
        row, line = next((row, line) for row, line in enumerate(screen.splitlines())
                         if title in line and "▁" in line)
        tmux.mouse_click(line.rindex("▁"), row)
        tmux.wait_until(lambda: "Host / fleet" not in tmux.capture() if title == "Targets"
                        else "Profile  Harness" not in tmux.capture(), title + " minimized")

    for width, height in ((79, 18), (80, 18), (140, 40), (200, 60)):
        tmux.resize(width, height)
        if width < 80:
            tmux.wait_for("Terminal too small", f"dashboard width guard at {width} columns")
            record(f"minimum-{width}x{height}", "resize below 80 columns", "the explicit terminal-width guard is rendered")
            continue
        tmux.wait_for("Sessions", f"dashboard at {width}x{height}")
        tmux.send_key("F2")
        tmux.wait_for("Commands")
        tmux.wait_for("  Close  ")
        click("  Close  ")
        absent("  Close  ")
        record(f"commands-{width}x{height}", "Commands stays clickable and closes at this terminal size")

    tmux.send_key("F2")
    tmux.wait_for("Commands")
    tmux.wait_for("Search commands…")
    name("help")
    tmux.wait_for(" 1 commands ")
    click("  Run  ")
    tmux.wait_for("Keyboard shortcuts")
    x, y = locate_text(tmux.capture(), "Keyboard shortcuts")
    tmux.mouse_event(65, x + 2, y + 2)
    click("  Close  ")
    absent("Keyboard shortcuts")
    record("help-mouse", "Help opens, scrolls, and closes without shortcuts")

    # Notices have a four-second minimum reading period before input dismisses them.
    time.sleep(4.1)
    screen = tmux.wait_for("Prompt")
    x, y = locate_text(screen, "Prompt", last=True)
    tmux.mouse_click(x + 2, y + 1)
    click("F4 web")
    tmux.wait_for("  Close  ")
    click("  Close  ")
    absent("  Close  ")
    record("composer-shortcut", "F4 web is clickable while the composer has focus")

    # The active secondary workspace owns a live session, so its first Delete
    # opens the manager's typed Force delete guard. Complete that guard through
    # the current manager controls and verify the dashboard falls back to the
    # surviving tab.
    workspace_manager()
    click("Delete")
    tmux.wait_for("Type the exact workspace name to confirm:")
    workspace_name("Mouse secondary")
    click("Force delete")
    lab.wait_snapshot(
        lambda value: all(
            row["name"] != "Mouse secondary" for row in value["workspaces"]
        )
        and all(row["workspace_id"] != secondary_id for row in value["sessions"]),
        "force delete workspace",
    )
    tmux.wait_for("Mouse renamed")
    tmux.wait_until(
        lambda: "Working:" not in tmux.capture(),
        "workspace delete completes in manager",
    )
    click("Close")
    absent("New workspace")
    record("workspace-force-delete", "☰; Delete; type exact name; Force delete; Close", "the active workspace and its session are removed and the remaining tab stays open")



def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", type=int, default=908)
    args = parser.parse_args()
    binary = REPO_ROOT / "target/debug/mj"
    terminal_binary = shutil.which("tmux")
    if terminal_binary is None:
        raise SystemExit("tmux must be on PATH")
    lab = Lab(binary, "mouse-commands", args.seed)
    evidence = Evidence(lab.root, binary, args.seed, ((79, 18), (80, 18), (140, 40), (200, 60)))
    tmux = None
    result = 1
    print(f"mouse-commands: artifacts={lab.root}", flush=True)
    try:
        port = lab.prepare(fake_acp_delay_ms=100)
        config = lab.config / "config.toml"
        config.write_text(config.read_text().replace('[startup]\nenabled = false', '[startup]\nenabled = false\nprofile = "fake"\ntarget = "localhost"'))
        home = lab.runtime_root / "home"
        home.mkdir()
        environment = lab.environment()
        for key in ("OPENAI_API_KEY", "CODEX_API_KEY", "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "KIMI_API_KEY"):
            environment.pop(key, None)
        environment.update(HOME=str(home), CODEX_HOME=str(lab.profile),
                           MJ_WORKER_BINARY=str(REPO_ROOT / "target/debug/mj-worker"),
                           TERM="xterm-256color", LC_ALL="C.UTF-8",
                           PATH=f"{lab.runtime_root / 'bin'}:{pathlib.Path(terminal_binary).parent}:/usr/bin:/bin")
        tmux_config = lab.root / "tmux.conf"
        tmux_config.write_text("set -g status off\nset -g mouse off\nset -g default-terminal xterm-256color\n")
        tmux = TmuxController(lab.runtime_root / "tmux.sock", tmux_config, environment, binary)
        exercise(lab, tmux, evidence, port)
        evidence.finish("passed")
        result = 0
        print("mouse-commands: passed", flush=True)
    except Exception as error:
        if tmux and tmux.session:
            evidence.capture("failure", tmux.capture())
        evidence.finish("failed", str(error))
        print(f"mouse-commands: failed: {error}", file=sys.stderr)
    finally:
        errors = []
        operations = [("tmux", lambda: tmux.release() if tmux and tmux.session else None),
                      ("daemon", lab.stop_daemon), ("owned processes", lab.cleanup_owned),
                      ("integrity", lab.integrity), ("preserve runtime", lab.preserve_runtime)]
        for label, operation in operations:
            try:
                operation()
            except Exception as error:
                errors.append(f"{label}: {error}")
        if errors:
            evidence.finish("failed", "; ".join(errors))
            print("; ".join(errors), file=sys.stderr)
            result = 1
        else:
            lab.remove_runtime()
    return result


if __name__ == "__main__":
    raise SystemExit(main())
