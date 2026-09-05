#!/usr/bin/env python3
"""Drive the real Hel web viewer concurrently with a terminal dashboard."""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import pathlib
import subprocess
import sys
import time
import traceback

from reliability_lab import Lab, ScenarioFailure, TIMEOUT


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--seed", required=True, type=int)
    parser.add_argument("--hel", required=True, type=pathlib.Path)
    return parser.parse_args()


def start_dashboard(lab: Lab):
    client = lab.start_tui("tui-1")
    client.wait_for("Workspaces")
    client.send(b"\r\r")
    client.wait_for("Sessions")
    return client


def qr_url(lab: Lab) -> str:
    reply = lab.daemon_request({"action": "status"})
    if not isinstance(reply, dict) or reply.get("reply") != "status":
        raise ScenarioFailure(f"unexpected daemon status reply: {reply!r}")
    phone = reply.get("value", {}).get("phone_status", {})
    url = phone.get("qr_login_url") if isinstance(phone, dict) else None
    if not isinstance(url, str) or not url.startswith("https://"):
        raise ScenarioFailure(f"daemon did not publish an HTTPS QR login URL: {phone!r}")
    return url


def wait_marker_or_exit(marker: pathlib.Path, browser: subprocess.Popen[bytes]) -> None:
    # The browser walks the whole normal operator flow before it goes offline:
    # the quota and targets pages, the four-step new-session wizard, a real
    # provision, and a conversation. That is minutes of honest work, not the
    # single page load this wait was first sized for.
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        if marker.exists():
            return
        code = browser.poll()
        if code is not None:
            raise ScenarioFailure(f"Playwright exited before browser/TUI synchronization ({code})")
        time.sleep(0.05)
    raise ScenarioFailure("Playwright did not reach its offline synchronization point")


# The footer names the keys the focused pane owns, so it is the surface's own
# report of where the keyboard is. Border styles say the same thing, but a
# partially redrawn frame can show two panes bordered alike for one frame,
# whereas the footer is one line that is always rewritten whole.
PANE_RING = ("Sessions", "Prompt", "Targets", "Quota")
SESSIONS_FOCUSED = "Enter open \u2502 Alt-N new \u00b7 Alt-S resume \u00b7 Alt-A read"


def focus_sessions(client) -> None:
    """Put the keyboard on the Sessions pane and prove it landed there.

    Which pane starts with the keyboard depends on the surface's state, so walk
    the ring and read the footer rather than assuming one keystroke is enough.
    """
    for _ in range(len(PANE_RING) * 2):
        if SESSIONS_FOCUSED in client.text():
            return
        client.send(b"\t")
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if SESSIONS_FOCUSED in client.text():
                return
            time.sleep(0.05)
    raise ScenarioFailure(
        f"the keyboard never reached the Sessions pane: {client.text()[-4000:]}"
    )


def stop_from_dashboard(client) -> None:
    focus_sessions(client)
    client.send(b"\x1bOQ")
    client.wait_for("type to filter \u00b7 Up/Down")
    client.send(b"stop\r")
    client.wait_for("Stop session?")
    client.send(b"\r")
    # A stop needs the daemon's session manager to have adopted the session,
    # and adoption is asynchronous: a session the browser created moments ago
    # can still be unmanaged when the first stop reaches it. The surface offers
    # Retry stop for exactly that, so take it rather than failing on a
    # condition that resolves itself.
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if "Stop could not complete" not in client.text():
            return
        # Cancel, Force stop, Retry stop: two steps right of the default.
        client.send(b"\x1b[C\x1b[C\r")
        time.sleep(2)
    raise ScenarioFailure(f"the stop never completed: {client.text()[-4000:]}")



def run_layout_matrix(lab: Lab, web_root: pathlib.Path, environment: dict[str, str]) -> None:
    """Drive the layout and accessibility checks at three viewport widths.

    This runs after the reliability scenario against the same live daemon, so
    it costs one extra browser rather than a second lab, and it sees the real
    pages with real sessions on them rather than an empty shell.
    """
    log = (lab.root / "layout.log").open("wb")
    matrix_environment = dict(environment)
    matrix_environment["MJ_BROWSER_SPEC"] = "{layout,quota}.spec.js"
    matrix = subprocess.Popen(
        [str(web_root / "node_modules/.bin/playwright"), "test"],
        cwd=web_root,
        env=matrix_environment,
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    lab.record_process("started", "playwright-layout", matrix.pid)
    try:
        return_code = matrix.wait(timeout=TIMEOUT * 8)
    except subprocess.TimeoutExpired as error:
        raise ScenarioFailure("the layout matrix did not finish") from error
    finally:
        log.close()
    if return_code != 0:
        raise ScenarioFailure(
            f"the layout matrix failed with exit code {return_code}: "
            f"{(lab.root / 'layout.log').read_text()[-4000:]}"
        )
    lab.record_process("stopped", "playwright-layout", matrix.pid)


def run(lab: Lab) -> None:
    port = lab.prepare(phone_tls=True)
    dashboard = start_dashboard(lab)
    code, _ = lab.wait_daemon_status(port)
    lab.base_url = f"https://127.0.0.1:{port}"
    status, _ = lab.request("POST", "/auth/session", {"code": code})
    if status != 204:
        raise ScenarioFailure(f"Python observer login returned {status}")
    login_url = qr_url(lab)
    title = f"browser-reliability-{lab.seed}"
    ready_marker = lab.runtime_root / "browser-ready"
    changed_marker = lab.runtime_root / "tui-changed"
    web_root = pathlib.Path(__file__).resolve().parent / "web"
    browser_log = (lab.root / "browser.log").open("wb")
    environment = lab.environment()
    environment.update(
        {
            "MJ_BROWSER_BASE_URL": lab.base_url,
            "MJ_BROWSER_CODE": code,
            "MJ_BROWSER_QR_URL": login_url,
            "MJ_BROWSER_TITLE": title,
            "MJ_BROWSER_PROJECT_DIRECTORY": str(lab.project),
            "MJ_BROWSER_READY_MARKER": str(ready_marker),
            "MJ_TUI_CHANGED_MARKER": str(changed_marker),
            "MJ_BROWSER_TRACE": str(lab.root / "browser-trace.zip"),
            "MJ_BROWSER_SCREENSHOT": str(lab.root / "browser-failure.png"),
        }
    )
    browser = subprocess.Popen(
        [str(web_root / "node_modules/.bin/playwright"), "test"],
        cwd=web_root,
        env=environment,
        stdout=browser_log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    lab.record_process("started", "playwright", browser.pid)
    try:
        wait_marker_or_exit(ready_marker, browser)
        dashboard.wait_for(title)
        dashboard.resize(18, 72)
        time.sleep(0.2)
        dashboard.resize(40, 150)
        time.sleep(0.5)
        dashboard.wait_for(title)
        lab.record_action("dashboard-resized", rows=40, columns=150)
        stop_from_dashboard(dashboard)
        snapshot = lab.wait_snapshot(
            lambda value: any(
                item.get("title") == title and item.get("state") == "stopped"
                for item in value.get("sessions", [])
            ),
            "TUI-stopped browser session",
        )
        session = next(item for item in snapshot["sessions"] if item["title"] == title)
        changed_marker.write_text("TUI stop reached durable state\n")
        lab.record_action("tui-stopped-session", session_id=session["id"])
        try:
            # The browser drives a full resume after the TUI stop — navigate to
            # the resume page, resume the session, wait for it to provision,
            # then stop it again — so it needs materially longer here than the
            # single reconnect this wait was originally sized for.
            return_code = browser.wait(timeout=TIMEOUT * 8)
        except subprocess.TimeoutExpired as error:
            raise ScenarioFailure("Playwright did not finish after SSE reconnection") from error
        if return_code != 0:
            raise ScenarioFailure(f"Playwright failed with exit code {return_code}")
        lab.record_process("stopped", "playwright", browser.pid)
        run_layout_matrix(lab, web_root, environment)
        (lab.root / "browser-transcript.json").write_text(
            json.dumps(snapshot, indent=2, sort_keys=True) + "\n"
        )
        quit_elapsed = dashboard.quit()
        lab.record_process("stopped", "tui-1", dashboard.process.pid)
        if quit_elapsed >= 2:
            raise ScenarioFailure(f"dashboard quit took {quit_elapsed:.3f}s")
        lab.stop_daemon()
        lab.integrity()
        leaks = lab.owned_pids()
        if leaks:
            raise ScenarioFailure(f"owned processes remained after cleanup: {leaks}")
        lab.trace["finished_at"] = lab.timestamp()
        lab.trace["outcome"] = "passed"
        lab.write_trace()
    finally:
        if browser.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(browser.pid, 15)
            with contextlib.suppress(subprocess.TimeoutExpired):
                browser.wait(timeout=2)
        browser_log.close()


def main() -> int:
    args = parse_args()
    lab = Lab(args.hel, "browser-tui-convergence", args.seed)
    print(f"browser reliability: artifacts={lab.root}", flush=True)
    try:
        run(lab)
    except BaseException as error:
        (lab.root / "failure-traceback.txt").write_text(traceback.format_exc())
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
        print(f"browser reliability: failed: {error}", file=sys.stderr)
        return 1
    lab.capture_process_tree()
    lab.preserve_runtime()
    lab.remove_runtime()
    print("browser reliability: passed clients=2 sse_reconnect=1 leaks=0", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
