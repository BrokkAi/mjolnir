#!/usr/bin/env python3
"""Verify unified selection across real TUI startup, clicks, filters and restart.

Build mj and mj-worker in the dev profile first. Runs only in the named
selection-consistency instance with the existing lab's disposable directories.
"""

from __future__ import annotations

import argparse
import pathlib

from reliability_lab import Lab, ScenarioFailure
from tui_components_tmux import REPO_ROOT, TmuxController, locate_text

INSTANCE = "selection-consistency"


class SelectionLab(Lab):
    def environment(self):
        env = super().environment()
        env.update({
            "MJ_INSTANCE": INSTANCE,
            "MJOLNIR_NO_UPDATE_CHECK": "1",
            "MJ_WORKER_BINARY": str(REPO_ROOT / "target/debug/mj-worker"),
            "SESSIONWIKI_DATA": str(self.runtime_root / "sessionwiki"),
            "TERM": "xterm-256color",
        })
        return env

    def command(self, *args, **kwargs):
        return super().command("--instance", INSTANCE, *args, **kwargs)


class SelectionTerminal(TmuxController):
    def __init__(self, *args, project):
        super().__init__(*args)
        self.project = project

    def start(self, session, columns=180, rows=44):
        self.run("new-session", "-d", "-s", session, "-x", str(columns),
                 "-y", str(rows), "-c", str(self.project), "--",
                 str(self.binary), "--instance", INSTANCE)
        self.session = session


def exercise(lab, terminal):
    port = lab.prepare()
    with (lab.config / "config.toml").open("a") as config:
        config.write('\n[review]\nenabled = false\n')
    terminal.start("selection-setup")
    terminal.wait_for("Sessions")
    code, _ = lab.wait_daemon_status(port)
    status, _ = lab.request("POST", "/auth/session", {"code": code})
    if status != 204:
        raise ScenarioFailure(f"login failed: {status}")
    workspace = lab.snapshot()["workspaces"][0]["id"]
    for title, marker in [("Selection A", "alpha-selection-marker"),
                          ("Selection B", "beta-selection-marker")]:
        status, result = lab.request("POST", "/api/actions", {
            "action": "new", "workspace_id": workspace, "profile_id": "fake",
            "bundle_id": "fixture", "target_id": "localhost", "title": title,
            "project_directory": str(lab.project),
        })
        if status != 202:
            raise ScenarioFailure(f"create failed: {status} {result}")
        snapshot = lab.wait_snapshot(lambda value: any(
            s.get("title") == title and s.get("state") == "running"
            for s in value.get("sessions", [])), f"{title} running")
        session = next(s for s in snapshot["sessions"] if s["title"] == title)
        status, result = lab.request("POST", "/api/actions", {
            "action": "prompt", "session_id": session["id"], "text": marker,
        })
        if status != 202:
            raise ScenarioFailure(f"prompt failed: {status} {result}")
        def replied():
            status, value = lab.request("GET", f"/api/conversations/{session['id']}")
            return status == 200 and any(
                f"reliability reply: {marker}" in line
                for entry in value.get("entries", []) for line in entry.get("lines", []))
        terminal.wait_until(replied, f"{title} replied")
    terminal.release()

    def assert_selected(title, marker, label):
        def matches():
            screen = terminal.capture()
            lines = screen.splitlines()
            if not lines:
                return False
            boundary = lines[0].find("╮") + 1
            if boundary <= 0:
                return False
            return (any("›" in line[:boundary] and title in line[:boundary] for line in lines)
                    and marker in "\n".join(line[boundary:] for line in lines))
        terminal.wait_until(matches, f"{title} marker and conversation agree")
        (lab.root / f"{label}.txt").write_text(terminal.capture())

    def click_row(title):
        x, y = locate_text(terminal.capture(), title)
        terminal.mouse_click(x + 2, y)

    terminal.start("selection-fresh")
    assert_selected("Selection B", "beta-selection-marker", "fresh-start")
    click_row("Selection A")
    assert_selected("Selection A", "alpha-selection-marker", "list-click")
    # Split A into a pin and an empty Browse, then select B in the new pane.
    terminal.send_key("C-b")
    terminal.send_key("v")
    terminal.wait_until(lambda: "◆" in terminal.capture(), "A pinned by split")
    click_row("Selection B")
    assert_selected("Selection B", "beta-selection-marker", "split-browse")
    # Click A's conversation body, excluding the sidebar's transcript preview.
    lines = terminal.capture().splitlines()
    boundary = lines[0].find("╮") + 1
    y, line = next((y, line) for y, line in enumerate(lines)
                   if "alpha-selection-marker" in line[boundary:])
    x = line.index("alpha-selection-marker", boundary)
    terminal.mouse_click(x + 2, y)
    assert_selected("Selection A", "alpha-selection-marker", "conversation-click")
    # A remains the selected row even when the list filter matches only B.
    click_row("Selection A")
    terminal.send_key("/")
    terminal.send_text("Selection B")
    terminal.wait_for("Outside filter")
    assert_selected("Selection A", "alpha-selection-marker", "filtered-active")
    terminal.send_key("Escape")
    terminal.send_key("Escape")
    terminal.release()
    terminal.start("selection-restored")
    assert_selected("Selection A", "alpha-selection-marker", "restored-start")
    terminal.wait_for("beta-selection-marker")
    terminal.release()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mj", type=pathlib.Path, default=REPO_ROOT / "target/debug/mj")
    args = parser.parse_args()
    lab = SelectionLab(args.mj, "unified-selection", 1)
    config = lab.root / "tmux.conf"
    config.write_text("set -g status off\nset -g mouse off\nset -g default-terminal xterm-256color\n")
    terminal = SelectionTerminal(lab.runtime_root / "tmux.sock", config, lab.environment(), args.mj.resolve(), project=lab.project)
    print(f"selection-consistency: artifacts={lab.root}", flush=True)
    try:
        exercise(lab, terminal)
        lab.trace["outcome"] = "passed"
    except BaseException as error:
        lab.trace["outcome"] = "failed"
        lab.trace["failure"] = str(error)
        (lab.root / "failure.txt").write_text(terminal.capture())
        raise
    finally:
        lab.trace["finished_at"] = lab.timestamp()
        lab.write_trace()
        # Stop owners before preserving or removing any working files.
        errors = []
        for cleanup in [terminal.release, lab.stop_daemon, lab.cleanup_owned,
                        lab.integrity, lab.preserve_runtime]:
            try:
                cleanup()
            except Exception as error:
                errors.append(str(error))
        if errors:
            raise ScenarioFailure("cleanup: " + "; ".join(errors))

    print("selection-consistency: passed", flush=True)
