#!/usr/bin/env python3
"""Exercise workspace/session commands by mouse in an isolated real terminal.

Build mj and mj-worker, then run this script with tmux on PATH. Keyboard input
is limited to editing text fields; navigation and submission use SGR clicks.
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
        tmux.mouse_click(x + (2 if label.startswith("  ") else 0), y)

    def absent(label: str) -> None:
        tmux.wait_until(lambda: label not in tmux.capture(), f"{label!r} dismissed")

    def record(label: str, expected: str) -> None:
        screen = tmux.capture()
        evidence.event(label, "mouse clicks and text entry", expected, expected,
                       evidence.capture(label, screen))

    def name(value: str) -> None:
        tmux.send_key("C-a")
        tmux.send_key("C-k")
        tmux.send_raw("\x1b[200~" + value + "\x1b[201~")

    def workspace_picker() -> None:
        # The workspace name occupies the sidebar's first content row.
        tmux.mouse_click(5, 1)
        tmux.wait_for("  Open  ")
        tmux.wait_until(lambda: "Loading workspace details" not in tmux.capture(), "workspace management metadata ready")

    tmux.run("new-session", "-d", "-s", "mouse-commands", "-x", "140", "-y", "40",
             "-c", str(lab.project), "--", str(tmux.binary))
    tmux.session = "mouse-commands"
    screen = tmux.wait_for_any(("  Open  ", "Quick new"), "workspace picker or dashboard")
    if "  Open  " in screen:
        click("  Open  ")
    tmux.wait_for("Quick new")
    code, _ = lab.wait_daemon_status(port)
    lab.base_url = f"http://127.0.0.1:{port}"
    status, _ = lab.request("POST", "/auth/session", {"code": code})
    if status != 204:
        raise ScenarioFailure(f"fixture viewer login returned {status}")

    workspace_picker()
    click("  New  ")
    tmux.wait_for("New workspace")
    name("Mouse workspace")
    click("  Create  ")
    tmux.wait_for("Quick new")
    lab.wait_snapshot(lambda value: any(row["name"] == "Mouse workspace" for row in value["workspaces"]), "workspace persisted")
    record("workspace-created", "mouse-created workspace is durable and open")

    workspace_picker()
    click("  Rename  ")
    tmux.wait_for("Rename workspace")
    name("Mouse renamed")
    click("  Save  ")
    lab.wait_snapshot(lambda value: any(row["name"] == "Mouse renamed" for row in value["workspaces"]), "workspace rename persisted")
    click("  Open  ")
    tmux.wait_for("Quick new")
    record("workspace-renamed", "renamed workspace can be opened by mouse")

    workspace_picker()
    click("  New  ")
    tmux.wait_for("New workspace")
    name("Mouse disposable")
    click("  Create  ")
    tmux.wait_for("Quick new")
    workspace_picker()
    click("  Delete  ")
    tmux.wait_for("Delete workspace Mouse disposable?")
    click("  Cancel  ")
    absent("Delete workspace Mouse disposable?")
    click("  Delete  ")
    tmux.wait_for("Delete workspace Mouse disposable?")
    click("  Delete  ")
    lab.wait_snapshot(lambda value: all(row["name"] != "Mouse disposable" for row in value["workspaces"]), "workspace deleted")
    click("Mouse renamed")
    click("  Open  ")
    tmux.wait_for("Quick new")
    record("workspace-deleted", "Cancel preserves workspace; confirmed Delete removes it")

    before = {row["id"] for row in lab.snapshot()["sessions"]}
    click("Quick new")
    snapshot = lab.wait_snapshot(lambda value: any(row["id"] not in before and row["state"] == "running" for row in value["sessions"]), "quick session running")
    added = [row for row in snapshot["sessions"] if row["id"] not in before]
    if len(added) != 1:
        raise ScenarioFailure("Quick new did not create exactly one session")
    record("quick-session", "Quick new starts exactly one session with saved defaults")

    click("New…")
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
    lab.wait_snapshot(lambda value: sum(row["state"] == "running" for row in value["sessions"]) == 2, "wizard session running")
    record("wizard-session", "New… supports creation through every wizard step by mouse")

    click(" ⋯ ", last=False)
    tmux.wait_for("Rename session")
    click("Rename session")
    click("  Run  ")
    tmux.wait_for("Rename session", "rename editor")
    name("Mouse session")
    click("  Save  ")
    lab.wait_snapshot(lambda value: any(row.get("title") == "Mouse session" for row in value["sessions"]), "session renamed")
    record("session-actions", "session row menu renames the clicked session")

    # Collapse support panes using their existing title controls before the
    # 40x10 check; standard panes require at least thirteen terminal rows.
    for title in ("Targets", "Quota"):
        screen = tmux.wait_for(title)
        row, line = next((row, line) for row, line in enumerate(screen.splitlines())
                         if title in line and "▁" in line)
        tmux.mouse_click(line.rindex("▁"), row)
        tmux.wait_until(lambda: "Host / fleet" not in tmux.capture() if title == "Targets"
                        else "Profile  Harness" not in tmux.capture(), title + " minimized")

    for width, height in ((40, 10), (72, 18), (140, 40), (200, 60)):
        tmux.resize(width, height)
        click("Commands", last=False)
        tmux.wait_for("  Close  ")
        click("  Close  ")
        absent("  Close  ")
        record(f"commands-{width}x{height}", "Commands stays clickable and closes at this terminal size")

    click("Commands", last=False)
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
    screen = tmux.wait_for("╭ Prompt")
    x, y = locate_text(screen, "╭ Prompt")
    tmux.mouse_click(x + 2, y + 1)
    click("F4 web")
    tmux.wait_for("  Close  ")
    click("  Close  ")
    absent("  Close  ")
    record("composer-shortcut", "F4 web is clickable while the composer has focus")



def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", type=int, default=908)
    args = parser.parse_args()
    binary = REPO_ROOT / "target/debug/mj"
    terminal_binary = shutil.which("tmux")
    if terminal_binary is None:
        raise SystemExit("tmux must be on PATH")
    lab = Lab(binary, "mouse-commands", args.seed)
    evidence = Evidence(lab.root, binary, args.seed, ((40, 10), (72, 18), (140, 40), (200, 60)))
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
