#!/usr/bin/env python3
"""Live acceptance harness for the terminal component migration.

The harness runs the real ``mj`` executable in a private tmux server. It uses
the same disposable fake ACP lab as the reliability scenarios, but keeps the
tmux socket in its disposable runtime and configuration, captures, and copied
runtime below the generated artifact directory. No ambient Mjolnir or ACP state is read.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import time
from typing import Callable, Iterable

from reliability_lab import Lab, ScenarioFailure


REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
DIMENSIONS = ((40, 10), (72, 18), (140, 40), (200, 60))
DEFAULT_TIMEOUT = 15.0


def _write_text(path: pathlib.Path, text: str) -> None:
    temporary = path.with_suffix(path.suffix + ".new")
    temporary.write_text(text)
    temporary.replace(path)


def git_output(arguments: list[str]) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=REPO_ROOT,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return result.stdout.strip()


class Evidence:
    """Persist every screen assertion while the run is still in progress."""

    def __init__(
        self,
        root: pathlib.Path,
        binary: pathlib.Path,
        seed: int,
        dimensions: tuple[tuple[int, int], ...],
    ):
        self.root = root
        self.capture_root = root / "captures"
        self.capture_root.mkdir(mode=0o700)
        self.manifest_path = root / "live-evidence.json"
        self.started = time.monotonic()
        self.manifest: dict[str, object] = {
            "format_version": 1,
            "seed": seed,
            "commit": git_output(["rev-parse", "HEAD"]),
            "binary": str(binary),
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            "dimensions": [
                {"columns": columns, "rows": rows} for columns, rows in dimensions
            ],
            "events": [],
            "outcome": "running",
        }
        self._save()

    def _save(self) -> None:
        _write_text(
            self.manifest_path,
            json.dumps(self.manifest, indent=2, sort_keys=True) + "\n",
        )

    def capture(self, label: str, screen: str) -> str:
        safe = re.sub(r"[^A-Za-z0-9_.-]+", "-", label).strip("-") or "screen"
        path = self.capture_root / f"{len(self.manifest['events']):03d}-{safe}.txt"
        _write_text(path, screen)
        return str(path.relative_to(self.root))

    def event(
        self,
        label: str,
        input_description: str,
        expected: str,
        actual: str,
        capture: str | None = None,
    ) -> None:
        events = self.manifest["events"]
        assert isinstance(events, list)
        events.append(
            {
                "at_seconds": round(time.monotonic() - self.started, 3),
                "label": label,
                "input": input_description,
                "expected": expected,
                "actual": actual,
                **({"capture": capture} if capture else {}),
            }
        )
        self._save()

    def finish(self, outcome: str, failure: str | None = None) -> None:
        self.manifest["outcome"] = outcome
        if failure:
            self.manifest["failure"] = failure
        self._save()


class TmuxController:
    """Own one tmux server and only the sessions created through it."""

    def __init__(
        self,
        socket_path: pathlib.Path,
        config_path: pathlib.Path,
        environment: dict[str, str],
        binary: pathlib.Path,
    ):
        self.socket_path = socket_path
        self.config_path = config_path
        self.environment = environment
        self.binary = binary
        self.session: str | None = None

    def run(self, *arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                "tmux",
                "-S",
                str(self.socket_path),
                "-f",
                str(self.config_path),
                *arguments,
            ],
            cwd=REPO_ROOT,
            env=self.environment,
            check=check,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def start(self, session: str, columns: int, rows: int) -> None:
        if self.session is not None:
            raise ScenarioFailure(f"tmux session {self.session!r} is still owned")
        self.run(
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            str(columns),
            "-y",
            str(rows),
            "--",
            str(self.binary),
        )
        self.session = session

    def has_session(self) -> bool:
        if self.session is None:
            return False
        return self.run("has-session", "-t", self.session, check=False).returncode == 0

    def capture(self) -> str:
        if self.session is None:
            return ""
        # Keep physical rows intact: the coordinates returned by locate_text
        # are sent back as SGR mouse coordinates, so joining wrapped rows would
        # shift the release point below a narrow dialog.
        result = self.run("capture-pane", "-p", "-t", self.session, check=False)
        return result.stdout if result.returncode == 0 else ""

    def pane_pid(self) -> int:
        if self.session is None:
            raise ScenarioFailure("no tmux session to inspect")
        result = self.run("display-message", "-p", "-t", self.session, "#{pane_pid}")
        try:
            return int(result.stdout.strip())
        except ValueError as error:
            raise ScenarioFailure(
                f"tmux returned an invalid pane pid: {result.stdout!r}"
            ) from error

    def send_key(self, key: str) -> None:
        if self.session is None:
            raise ScenarioFailure("no tmux session for key input")
        self.run("send-keys", "-t", self.session, key)

    def send_text(self, text: str) -> None:
        if self.session is None:
            raise ScenarioFailure("no tmux session for text input")
        self.run("send-keys", "-t", self.session, "-l", text)

    def send_raw(self, data: str) -> None:
        # tmux sends literal UTF-8 bytes to the pane PTY with -l. This is
        # intentionally a PTY path rather than a dashboard test seam.
        self.send_text(data)

    def resize(self, columns: int, rows: int) -> None:
        if self.session is None:
            raise ScenarioFailure("no tmux session to resize")
        self.run("resize-window", "-t", self.session, "-x", str(columns), "-y", str(rows))
        time.sleep(0.15)

    def release(self) -> None:
        if self.session is None:
            return
        session = self.session
        failure: ScenarioFailure | None = None
        try:
            if self.has_session():
                self.send_key("M-q")
                self.wait_until(lambda: not self.has_session(), "tmux dashboard to detach")
        except ScenarioFailure as error:
            failure = error
        finally:
            self.session = None
            # A server with no sessions normally exits by itself. This command
            # is scoped to the unique socket and is the fallback for a wedged
            # pane, including failure paths.
            self.run("kill-server", check=False)
            deadline = time.monotonic() + 2
            while self.socket_path.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
        if self.run("list-sessions", check=False).returncode == 0:
            raise ScenarioFailure(f"owned tmux server survived release of {session}")
        # tmux can leave an unbound Unix socket after its server exits.
        self.socket_path.unlink(missing_ok=True)
        if failure is not None:
            raise failure

    def wait_until(
        self,
        predicate: Callable[[], bool],
        description: str,
        timeout: float = DEFAULT_TIMEOUT,
    ) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.05)
        raise ScenarioFailure(
            f"timed out waiting for {description}; pane={self.capture()[-4000:]!r}"
        )

    def wait_for(
        self, marker: str, description: str | None = None, timeout: float = DEFAULT_TIMEOUT
    ) -> str:
        description = description or f"{marker!r} on screen"
        last = ""

        def found() -> bool:
            nonlocal last
            last = self.capture()
            return marker in last

        try:
            self.wait_until(found, description, timeout)
        except ScenarioFailure as error:
            raise ScenarioFailure(f"{error}; last capture={last[-4000:]!r}") from error
        return last

    def wait_for_any(
        self, markers: Iterable[str], description: str, timeout: float = DEFAULT_TIMEOUT
    ) -> str:
        candidates = tuple(markers)
        last = ""

        def found() -> bool:
            nonlocal last
            last = self.capture()
            return any(marker in last for marker in candidates)

        try:
            self.wait_until(found, description, timeout)
        except ScenarioFailure as error:
            raise ScenarioFailure(f"{error}; last capture={last[-4000:]!r}") from error
        return last

    def mouse_event(self, code: int, x: int, y: int, release: bool = False) -> None:
        suffix = "m" if release else "M"
        self.send_raw(f"\x1b[<{code};{x + 1};{y + 1}{suffix}")

    def mouse_click(self, x: int, y: int) -> None:
        self.mouse_event(0, x, y)
        self.mouse_event(0, x, y, release=True)

    def mouse_drag_outside(self, x: int, y: int) -> None:
        self.mouse_event(0, x, y)
        self.mouse_event(32, 0, 0)
        self.mouse_event(0, 0, 0, release=True)


def locate_text(screen: str, text: str, *, last: bool = False) -> tuple[int, int]:
    matches: list[tuple[int, int]] = []
    for row, line in enumerate(screen.splitlines()):
        column = line.find(text)
        if column >= 0:
            matches.append((column, row))
    if not matches:
        raise ScenarioFailure(f"could not locate {text!r} in capture:\n{screen}")
    return matches[-1] if last else matches[0]


def shlex_quote(value: str) -> str:
    return "'" + value.replace("'", "'\\''") + "'"


def install_fake_podman(lab: Lab) -> None:
    """Keep local-Podman capacity probes inside this lab."""

    fixture_bin = lab.runtime_root / "bin"
    shim = fixture_bin / "podman"
    log_path = lab.runtime_root / "fake-podman.log"
    shim.write_text(
        "#!/bin/sh\n"
        f"printf '%s\\n' \"$*\" >> {shlex_quote(str(log_path))}\n"
        "case \"$1 $2\" in\n"
        "  'info '*|'version '*) exit 0 ;;\n"
        "  'image exists'|'container exists') exit 1 ;;\n"
        "  'image inspect'|'container inspect') printf 'fixture\\n'; exit 0 ;;\n"
        "  *) exit 0 ;;\n"
        "esac\n"
    )
    shim.chmod(0o700)


def overlay_container_target(lab: Lab) -> None:
    path = lab.config / "config.toml"
    body = path.read_text()
    pattern = r'(?m)^\[targets\.localhost\]\nkind = "local-bare"\n'
    replacement = (
        '[targets.localhost]\n'
        'kind = "local-podman"\n'
        'image = "local/hel-live-fixture:component"\n'
        'pull_policy = "never"\n'
    )
    changed, count = re.subn(pattern, replacement, body)
    if count != 1:
        raise ScenarioFailure(
            "fixture setup gap: expected exactly one local-bare localhost target; "
            "the container editor needs a container-backed target"
        )
    _write_text(path, changed)


def next_session_id(snapshot: dict[str, object], title: str) -> str:
    sessions = snapshot.get("sessions", [])
    if isinstance(sessions, list):
        for item in sessions:
            if isinstance(item, dict) and item.get("title") == title:
                return str(item["id"])
    raise ScenarioFailure(f"could not find disposable session {title!r}")


def create_session(
    lab: Lab, tmux: TmuxController, evidence: Evidence, seed: int, port: int
) -> None:
    """Create a running fake-ACP session for dashboard and chat controls."""

    tmux.start("dashboard-create", 140, 40)
    screen = tmux.wait_for("Workspaces", "workspace picker")
    evidence.event(
        "workspace-picker",
        "initial launch",
        "Workspaces visible",
        "Workspaces visible",
        evidence.capture("workspace-picker", screen),
    )
    tmux.send_key("Enter")
    time.sleep(0.05)
    tmux.send_key("Enter")
    screen = tmux.wait_for_any(
        ("Sessions", "Prompt (no live session)"),
        "dashboard after workspace selection",
    )
    evidence.event(
        "dashboard-ready",
        "Enter, Enter",
        "Sessions dashboard visible",
        "Sessions dashboard visible",
        evidence.capture("dashboard-ready", screen),
    )

    code, _ = lab.wait_daemon_status(port)
    lab.base_url = f"http://127.0.0.1:{port}"
    status, _ = lab.request("POST", "/auth/session", {"code": code})
    if status != 204:
        raise ScenarioFailure(f"fixture viewer login returned {status}")

    snapshot = lab.snapshot()
    workspaces = snapshot.get("workspaces", [])
    if not isinstance(workspaces, list) or len(workspaces) != 1:
        raise ScenarioFailure(f"expected one disposable workspace, got {workspaces!r}")
    title = f"live-components-{seed}"
    status, _ = lab.request(
        "POST",
        "/api/actions",
        {
            "action": "new",
            "workspace_id": workspaces[0]["id"],
            "profile_id": "fake",
            "bundle_id": "fixture",
            "target_id": "localhost",
            "title": title,
            "project_directory": str(lab.project),
        },
    )
    if status != 202:
        raise ScenarioFailure(f"fixture new-session action returned {status}")
    lab.wait_snapshot(
        lambda value: any(
            item.get("title") == title
            and item.get("state") == "running"
            and not item.get("has_error")
            for item in value.get("sessions", [])
        ),
        "fake-ACP session to run",
    )
    tmux.wait_for(title, "new session title")

    session_id = next_session_id(lab.snapshot(), title)
    prompt = f"component prompt {seed}"
    status, _ = lab.request(
        "POST",
        "/api/actions",
        {"action": "prompt", "session_id": session_id, "text": prompt},
    )
    if status != 202:
        raise ScenarioFailure(f"fixture prompt action returned {status}")
    tmux.wait_for(f"reliability reply: {prompt}", "delayed fake ACP reply")

    from tui_components_chat import run_chat_controls
    run_chat_controls(lab, tmux, evidence, session_id)
    screen = tmux.capture()
    evidence.event("session-running", f"API new/prompt for {title!r}", "running session visible", "running session visible", evidence.capture("session-running", screen))
    from tui_components_dialogs import run_dialog_acceptance
    run_dialog_acceptance(lab, tmux, evidence)
    tmux.release()


def dashboard_dimensions(tmux: TmuxController, evidence: Evidence) -> None:
    for columns, rows in DIMENSIONS:
        tmux.resize(columns, rows)
        screen = tmux.wait_for_any(
            ("Sessions", "live-components", "Terminal too small"),
            f"dashboard at {columns}x{rows}",
            timeout=5,
        )
        capture = evidence.capture(f"dashboard-{columns}x{rows}", screen)
        evidence.event(
            f"resize-{columns}x{rows}",
            f"tmux resize-window -x {columns} -y {rows}",
            "dashboard or explicit minimum-size message is rendered",
            "minimum-size message" if "Terminal too small" in screen else "dashboard rendered",
            capture,
        )


def run_workflow(
    lab: Lab, tmux: TmuxController, evidence: Evidence, seed: int, port: int
) -> None:
    create_session(lab, tmux, evidence, seed, port)
    # The daemon owns the in-memory configuration. Stop it before the
    # lab-only target overlay so the reattached dashboard and its daemon read
    # the container-backed fixture consistently.
    lab.stop_daemon()
    install_fake_podman(lab)
    overlay_container_target(lab)

    tmux.start("dashboard-components", 140, 40)
    screen = tmux.wait_for_any(("Workspaces", "Sessions"), "workspace or remembered dashboard after reattach")
    if "Workspaces" in screen:
        tmux.send_key("Enter")
        time.sleep(0.05)
        tmux.send_key("Enter")
    tmux.wait_for_any(("Sessions", "live-components"), "dashboard after reattach")

    dashboard_dimensions(tmux, evidence)
    tmux.resize(140, 40)
    tmux.wait_for("Sessions", "dashboard restored to 140x40")

    # Select the actual target row, independent of the restored pane focus.
    x, y = locate_text(tmux.capture(), "localhost", last=True)
    tmux.mouse_click(x, y)
    tmux.send_key("Enter")
    target_screen = tmux.wait_for("Target actions", "target actions dialog")
    evidence.event(
        "target-actions",
        "Tab, Enter",
        "Target actions dialog visible",
        "Target actions dialog visible",
        evidence.capture("target-actions", target_screen),
    )
    tmux.send_key("F1")
    help_screen = tmux.wait_for("Keys ·", "nested help over target actions")
    evidence.event(
        "nested-help-open",
        "F1",
        "help overlay visible",
        "help overlay visible",
        evidence.capture("nested-help-open", help_screen),
    )
    tmux.send_key("Escape")
    target_screen = tmux.wait_for("Target actions", "target actions restored after help")
    evidence.event(
        "nested-help-close",
        "Escape",
        "target actions restored",
        "target actions restored",
        evidence.capture("nested-help-close", target_screen),
    )
    tmux.send_key("Escape")
    tmux.wait_for("Sessions", "dashboard after target actions")

    # F2 is the user-facing route to the migrated representative form.
    tmux.send_key("F2")
    tmux.wait_for("Commands", "command palette")
    tmux.send_text("container settings")
    palette_screen = tmux.wait_for("Container settings", "container settings palette entry")
    evidence.event(
        "palette-container-settings",
        "F2, type `container settings`",
        "Container settings command visible",
        "Container settings command visible",
        evidence.capture("palette-container-settings", palette_screen),
    )
    tmux.send_key("Enter")
    editor_screen = tmux.wait_for("Edit container", "container editor")
    evidence.event(
        "container-editor-open",
        "Enter on Container settings",
        "container editor visible",
        "container editor visible",
        evidence.capture("container-editor-open", editor_screen),
    )

    for columns, rows in DIMENSIONS:
        tmux.resize(columns, rows)
        screen = tmux.wait_for_any(("Edit container", "Terminal too small"), "resize with container form open")
        evidence.event(f"container-resize-{columns}x{rows}", f"resize to {columns}x{rows}", "form or minimum-size message is rendered", "minimum-size message" if "Terminal too small" in screen else "form rendered", evidence.capture(f"container-resize-{columns}x{rows}", screen))
    tmux.resize(140, 40)
    tmux.wait_for("Edit container", "container form restored after resizing")

    # Exercise fields, mount creation, checkbox toggling, and list focus with
    # real terminal key reports. The mount uses the disposable project path.
    tmux.send_text("2")
    tmux.send_key("Tab")
    tmux.send_text("1g")
    tmux.send_key("Tab")
    tmux.send_text(str(lab.project))
    tmux.send_key("Tab")
    tmux.send_text("/workspace")
    tmux.send_key("Enter")
    mount_screen = tmux.wait_for("/workspace", "attached directory row")
    evidence.event(
        "container-mount-added",
        "type `2`, Tab, type `1g`, Tab, project path, Tab, `/workspace`, Enter",
        "attached directory row visible",
        "attached directory row visible",
        evidence.capture("container-mount-added", mount_screen),
    )
    tmux.send_key("Tab")
    tmux.send_key("Space")
    checkbox_screen = tmux.wait_for("[x]", "read-only checkbox toggled")
    evidence.event(
        "container-read-only",
        "Tab, Space",
        "read-only checkbox checked",
        "read-only checkbox checked",
        evidence.capture("container-read-only", checkbox_screen),
    )
    tmux.send_key("Tab")
    list_screen = tmux.wait_for("Attached directories", "mount list focus")
    evidence.event(
        "container-list-focus",
        "Tab",
        "attached directory list remains visible",
        "attached directory list remains visible",
        evidence.capture("container-list-focus", list_screen),
    )

    # A press followed by a drag outside must keep the modal open; releasing
    # inside then activates Cancel once. Coordinates come from the pane.
    cancel_x, cancel_y = locate_text(list_screen, "Cancel", last=True)
    tmux.mouse_drag_outside(cancel_x, cancel_y)
    # Give the PTY reader one scheduling turn before checking a marker that
    # was already present before the gesture.
    time.sleep(0.1)
    drag_screen = tmux.wait_for("Edit container", "drag-outside release to leave editor open")
    evidence.event(
        "container-drag-outside",
        f"SGR mouse down ({cancel_x},{cancel_y}), drag to (0,0), release",
        "editor remains open after outside release",
        "editor remains open",
        evidence.capture("container-drag-outside", drag_screen),
    )
    cancel_x, cancel_y = locate_text(drag_screen, "Cancel", last=True)
    tmux.mouse_click(cancel_x, cancel_y)
    tmux.wait_until(lambda: "Edit container" not in tmux.capture(), "mouse Cancel to close editor")
    closed_screen = tmux.capture()
    evidence.event(
        "container-mouse-cancel",
        f"SGR mouse click ({cancel_x},{cancel_y})",
        "editor closes and dashboard returns",
        "dashboard returned" if "Sessions" in closed_screen else "editor closed",
        evidence.capture("container-mouse-cancel", closed_screen),
    )

    # Verify ordinary help and palette after a migrated modal changes focus.
    tmux.send_key("F1")
    help_screen = tmux.wait_for("Keys ·", "help after editor")
    evidence.event(
        "help-after-editor",
        "F1",
        "help overlay visible",
        "help overlay visible",
        evidence.capture("help-after-editor", help_screen),
    )
    tmux.send_key("Escape")
    tmux.wait_for("Sessions", "dashboard after help")
    tmux.send_key("F2")
    palette_screen = tmux.wait_for("Commands", "palette after editor")
    evidence.event(
        "palette-after-editor",
        "F2",
        "command palette visible",
        "command palette visible",
        evidence.capture("palette-after-editor", palette_screen),
    )
    tmux.send_key("Escape")
    tmux.release()

    tmux.start("dashboard-sigterm", 140, 40)
    screen = tmux.wait_for_any(("Workspaces", "Sessions"), "workspace or remembered dashboard for SIGTERM test")
    if "Workspaces" in screen:
        tmux.send_key("Enter")
        time.sleep(0.05)
        tmux.send_key("Enter")
    tmux.wait_for("Sessions", "dashboard for SIGTERM test")
    pid = tmux.pane_pid()
    with contextlib.suppress(ProcessLookupError):
        os.kill(pid, signal.SIGTERM)
    tmux.wait_until(lambda: not tmux.has_session(), "SIGTERM dashboard process")
    evidence.event(
        "sigterm-dashboard",
        f"SIGTERM pane pid {pid}",
        "tmux pane exits cleanly",
        "tmux pane exited cleanly",
    )
    tmux.session = None
    tmux.run("kill-server", check=False)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", required=True, type=int)
    parser.add_argument(
        "--hel",
        type=pathlib.Path,
        default=REPO_ROOT / "target" / "debug" / "mj",
        help="actual mj executable (default: target/debug/mj)",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    binary = args.hel.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        print(
            f"live tmux setup gap: executable is missing or not executable: {binary}",
            file=sys.stderr,
        )
        return 2

    # Lab uses short temporary runtime paths for Unix socket limits. Build outputs
    # and the final preserved evidence remain under target.
    lab = Lab(binary, "tui-components", args.seed)
    evidence = Evidence(lab.root, binary, args.seed, DIMENSIONS)
    tmux_config = lab.root / "tmux.conf"
    _write_text(
        tmux_config,
        "set-option -g status off\n"
        "set-option -g mouse off\n"
        "set-option -g history-limit 2000\n"
        "set-option -g default-terminal xterm-256color\n",
    )
    fake_home = lab.runtime_root / "home"
    fake_tmp = lab.runtime_root / "tmp"
    fake_home.mkdir()
    fake_tmp.mkdir()
    environment = lab.environment()
    # Do not let host credentials reach a test-owned daemon or helper, even
    # though the fake profile itself supplies the ACP bridge environment.
    for key in (
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "KIMI_API_KEY",
    ):
        environment.pop(key, None)
    environment.update(
        {
            "HOME": str(fake_home),
            "CODEX_HOME": str(lab.profile),
            "MJ_WORKER_BINARY": str((REPO_ROOT / "target/debug/mj-worker").resolve()),
            "TMPDIR": str(fake_tmp),
            "TERM": "xterm-256color",
            "LC_ALL": "C.UTF-8",
            "PATH": f"{lab.runtime_root / 'bin'}:/usr/bin:/bin",
        }
    )
    tmux = TmuxController(lab.runtime_root / "tmux.sock", tmux_config, environment, binary)
    print(f"live-tmux: artifacts={lab.root}", flush=True)
    try:
        port = lab.prepare(fake_acp_delay_ms=300)
        config_path = lab.config / "config.toml"
        config_text = config_path.read_text()
        profile = config_text.split("[profiles.fake]", 1)[1].split("[bundles.fixture]", 1)[0]
        reviewer_home = lab.runtime_root / "reviewer"
        reviewer_home.mkdir()
        profile = profile.replace(json.dumps(str(lab.profile)), json.dumps(str(reviewer_home)))
        config_path.write_text(config_text + "\n[profiles.reviewer]" + profile + '\n[review]\nenabled = false\ntier = "extended"\nprofile = "reviewer"\n')
        (lab.runtime_root / "fake_acp.py").write_text((REPO_ROOT / "tests/e2e/tui_components_acp.py").read_text())
        run_workflow(lab, tmux, evidence, args.seed, port)
        evidence.finish("passed")
        lab.trace["outcome"] = "passed"
        lab.trace["finished_at"] = lab.timestamp()
        lab.write_trace()
        print(f"live-tmux: passed seed={args.seed}", flush=True)
        exit_code = 0
    except BaseException as error:
        evidence.finish("failed", str(error))
        lab.trace["outcome"] = "failed"
        lab.trace["failure"] = str(error)
        lab.trace["finished_at"] = lab.timestamp()
        lab.write_trace()
        print(f"live-tmux: failed: {error}", file=sys.stderr)
        exit_code = 1
    finally:
        # Teardown order matters: stop the owned pane and daemon before copying
        # or removing runtime state. The socket path is unique to this run.
        cleanup_errors = []
        operations = [
            ("tmux", lambda: tmux.release() if tmux.session is not None else None),
            ("daemon", lab.stop_daemon),
            ("owned processes", lab.cleanup_owned),
            ("process capture", lab.capture_process_tree),
            ("integrity", lab.integrity),
            ("preserve runtime", lab.preserve_runtime),
        ]
        for label, operation in operations:
            try:
                operation()
            except Exception as error:
                cleanup_errors.append(f"{label}: {error}")
        # Retain working files if stopping their owners failed.
        if not cleanup_errors:
            try:
                lab.remove_runtime()
            except Exception as error:
                cleanup_errors.append(f"remove runtime: {error}")
        if cleanup_errors:
            failure = "; ".join(cleanup_errors)
            evidence.finish("failed", failure)
            print(f"live-tmux: cleanup failed: {failure}", file=sys.stderr)
            exit_code = 1
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
