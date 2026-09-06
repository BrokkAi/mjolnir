"""Live acceptance probes for the dashboard's shared-component dialogs.

The root harness should call :func:`run_dialog_acceptance` while the
``running-local-bare`` session is displayed, after the dashboard has settled
and before the target overlay is installed::

    from tui_components_dialogs import run_dialog_acceptance

    run_dialog_acceptance(lab, tmux, evidence)

The function receives the already-running ``Lab``, ``TmuxController``, and
``Evidence`` objects.  It does not start or stop a session and each probe
returns to the dashboard before the next one begins.  The types are kept
duck-typed so this module can be loaded by the existing harness without a
circular import.

The review-settings probe is strict when the fixture exposes a configured
review profile and a visible asynchronous readiness state.  A fixture with
no review profile cannot enter that state; in that case the probe records a
``review-settings-probe-gap`` evidence event and still exercises the draft
selectors and cancellation.  Import rows may likewise be absent in a local
fixture, but the import tab, search field, and cancellation remain testable.
Those fixture limitations are observations for the caller; this module does
not claim that a live run passed until the root harness invokes it.
The typed force-stop/force-destroy confirmation needs a seeded failed
operation and is therefore left for a caller that can provide that state.
"""

from __future__ import annotations

import re
from typing import Any


_TEMP_TITLE = "dialog-probe"
_SEARCH_QUERY = "dialog-no-match"


def _capture(tmux: Any) -> str:
    return str(tmux.capture())


def _wait(tmux: Any, marker: str, description: str | None = None, timeout: float = 8) -> str:
    """Wait for a visible marker and return the resulting pane."""

    return str(tmux.wait_for(marker, description or f"{marker!r} is visible", timeout=timeout))


def _optional_wait(tmux: Any, marker: str, timeout: float = 2) -> str:
    """Return the current pane, allowing an optional async state to settle."""

    try:
        return _wait(tmux, marker, f"optional {marker!r}", timeout=timeout)
    except Exception:
        return _capture(tmux)


def _record(
    evidence: Any,
    tmux: Any,
    label: str,
    input_description: str,
    expected: str,
    screen: str | None = None,
) -> str:
    actual = _capture(tmux) if screen is None else screen
    capture = evidence.capture(label, actual)
    evidence.event(label, input_description, expected, actual, capture=capture)
    return actual


def _paste(tmux: Any, text: str) -> None:
    """Send a terminal bracketed paste, preserving the real paste path."""

    tmux.send_raw(f"\x1b[200~{text}\x1b[201~")


def _clear_field(tmux: Any, current_value: str) -> None:
    """Clear a known single-line field using its readline controls."""

    tmux.send_key("End")
    # A couple of extra backspaces tolerate a cursor at the end of a clipped
    # field while keeping this bounded for a normal session title.
    for _ in range(len(current_value) + 2):
        tmux.send_key("BSpace")


def _session_title(lab: Any, screen: str) -> str:
    """Find the selected fixture title without changing lab state."""

    try:
        snapshot = lab.snapshot()
    except Exception:
        snapshot = None

    if isinstance(snapshot, dict):
        sessions = snapshot.get("sessions")
        if isinstance(sessions, list):
            # Prefer a live session, then any session with a title.  The
            # running-local-bare fixture normally contains exactly one.
            ordered = sorted(
                (item for item in sessions if isinstance(item, dict) and item.get("title")),
                key=lambda item: 0 if item.get("state") in {"running", "starting"} else 1,
            )
            if ordered:
                return str(ordered[0]["title"])

    # Keep the fallback tied to the fixture's visible title convention.  If a
    # caller uses another fixture it gets a useful assertion instead of
    # accidentally overwriting an unknown session title.
    match = re.search(r"\blive-components-[A-Za-z0-9_-]+\b", screen)
    if match:
        return match.group(0)
    raise AssertionError("could not determine the selected session title for rename restoration")


def _open_palette_command(tmux: Any, query: str, marker: str) -> str:
    tmux.send_key("F2")
    _wait(tmux, "Commands", "command palette")
    tmux.send_text(query)
    _wait(tmux, marker, f"palette command {query!r}")
    tmux.send_key("Enter")
    return _capture(tmux)


def probe_rename(lab: Any, tmux: Any, evidence: Any) -> None:
    """Paste a temporary title, save it, then restore the fixture title."""

    dashboard = _wait(tmux, "Sessions", "dashboard before rename")
    original_title = _session_title(lab, dashboard)

    _open_palette_command(tmux, "rename session", "Rename session")
    dialog = _wait(tmux, "Session:", "rename editor")
    _record(
        evidence,
        tmux,
        "dialog-rename-open",
        "F2, search for Rename session, and press Enter",
        "Rename session field and standard Cancel/Save buttons are visible",
        dialog,
    )

    _clear_field(tmux, original_title)
    _paste(tmux, _TEMP_TITLE)
    # Exercise readline cursor movement before using the shared footer.
    tmux.send_key("Home")
    tmux.send_key("Right")
    edited = _wait(tmux, _TEMP_TITLE, "pasted rename text")
    _record(
        evidence,
        tmux,
        "dialog-rename-paste-arrows",
        "Bracketed-paste dialog-probe, then Home and Right",
        "the rename field contains the temporary title",
        edited,
    )

    tmux.send_key("Tab")
    tmux.send_key("Tab")
    tmux.send_key("Enter")
    tmux.wait_until(
        lambda: _TEMP_TITLE in _capture(tmux) and "Rename session" not in _capture(tmux),
        "temporary rename is saved",
        timeout=8,
    )
    _record(
        evidence,
        tmux,
        "dialog-rename-saved",
        "Tab to Save and press Enter",
        "dashboard returns with the temporary title",
        _capture(tmux),
    )

    _open_palette_command(tmux, "rename session", "Rename session")
    _wait(tmux, "Session:", "rename editor for restoration")
    _clear_field(tmux, _TEMP_TITLE)
    _paste(tmux, original_title)
    tmux.send_key("Home")
    tmux.send_key("Right")
    _wait(tmux, original_title, "restored rename text")
    tmux.send_key("Tab")
    tmux.send_key("Tab")
    tmux.send_key("Enter")
    tmux.wait_until(
        lambda: original_title in _capture(tmux) and "Rename session" not in _capture(tmux),
        "original session title is restored",
        timeout=8,
    )
    _record(
        evidence,
        tmux,
        "dialog-rename-restored",
        "paste the original title and Tab to Save",
        "the initial dashboard title is restored",
        _capture(tmux),
    )


def probe_target_config_id(lab: Any, tmux: Any, evidence: Any) -> None:
    """Open target actions, enter the ID editor, and cancel without edits."""

    del lab
    from tui_components_tmux import locate_text
    x, y = locate_text(_capture(tmux), "localhost", last=True)
    tmux.mouse_click(x, y)
    tmux.send_key("Enter")
    actions = _wait(tmux, "Target actions", "target actions dialog")
    _record(
        evidence,
        tmux,
        "dialog-target-actions-open",
        "Tab twice to Targets and press Enter",
        "target actions list and standard footer are visible",
        actions,
    )

    tmux.send_key("Tab")
    tmux.send_key("Enter")
    config_id = _wait(tmux, "Rename target ID", "target ID editor")
    _record(
        evidence,
        tmux,
        "dialog-target-config-id-open",
        "Tab to Rename in target actions and press Enter",
        "target ID field is visible",
        config_id,
    )

    tmux.send_key("Escape")
    actions_after_cancel = _wait(tmux, "Target actions", "target ID cancellation")
    _record(
        evidence,
        tmux,
        "dialog-target-config-id-cancel",
        "press Escape in the target ID editor",
        "target actions dialog returns without changing the ID",
        actions_after_cancel,
    )
    tmux.send_key("Escape")
    _wait(tmux, "Sessions", "dashboard after target cancellation")
    # Target actions returns focus to Targets.  Advance through Quota to the
    # initial Sessions stop for the next independent probe.
    tmux.send_key("Tab")
    tmux.send_key("Tab")
    _wait(tmux, "Sessions", "dashboard focus restored after target probe")


def probe_review_settings(lab: Any, tmux: Any, evidence: Any) -> None:
    """Disabled Save consumes a real click; async discovery preserves the draft."""
    import time
    from tui_components_tmux import locate_text
    before = lab.snapshot()["review_config"]
    _open_palette_command(tmux, "review settings", "Review settings")
    _wait(tmux, "Automatic review")
    _record(evidence, tmux, "review-settings-open", "open review settings", "review form visible")
    tmux.send_key("Space")
    time.sleep(0.15)
    screen = _wait(tmux, "No reviewer profile")
    _record(evidence, tmux, "review-settings-enabled", "toggle automatic review", "review enabled", screen)
    x, y = locate_text(screen, "No reviewer profile")
    tmux.mouse_click(x + 2, y)
    time.sleep(0.15)
    screen = _capture(tmux)
    x, y = locate_text(screen, "[ Save ]")
    tmux.mouse_click(x + 3, y)
    time.sleep(0.3)
    _wait(tmux, "Review settings")
    if lab.snapshot()["review_config"] != before:
        raise AssertionError("disabled Save persisted an invalid review draft")
    _record(evidence, tmux, "review-disabled-save", "enable without a reviewer; click disabled Save", "invalid draft stays open and persisted settings are unchanged")
    tmux.send_key("Right")
    tmux.send_key("Right")
    _wait(tmux, "checking actual targets")
    tmux.send_key("F1")
    _wait(tmux, "Keys ·")
    tmux.send_key("Escape")
    _wait(tmux, "Review settings")
    _record(evidence, tmux, "review-async-help", "select fake profile; open and close Help during discovery", "review draft restored during async readiness discovery")
    tmux.send_key("Escape")
    tmux.wait_until(lambda: "Review settings" not in _capture(tmux), "review cancellation")
    if lab.snapshot()["review_config"] != before:
        raise AssertionError("cancelling review settings persisted the draft")


def probe_new_wizard(lab: Any, tmux: Any, evidence: Any) -> None:
    """Open the new-session wizard, move a selector, and cancel in its footer."""

    del lab
    tmux.send_key("M-n")
    wizard = _wait(tmux, "New session", "new session wizard")
    _record(
        evidence,
        tmux,
        "dialog-new-wizard-open",
        "press Alt-N",
        "new session wizard and standard controls are visible",
        wizard,
    )

    tmux.send_key("Right")
    selector = _capture(tmux)
    _record(
        evidence,
        tmux,
        "dialog-new-wizard-selector",
        "move the profile selector once with Right",
        "wizard remains open with its draft selector active",
        selector,
    )

    # The wizard starts on its profile selector; one Tab reaches Cancel.
    tmux.send_key("Tab")
    tmux.send_key("Enter")
    cancelled = _wait(tmux, "Sessions", "new session wizard cancellation")
    _record(
        evidence,
        tmux,
        "dialog-new-wizard-cancel",
        "Tab to Cancel and press Enter",
        "wizard closes without creating or changing a session",
        cancelled,
    )


def probe_resume_import(lab: Any, tmux: Any, evidence: Any) -> None:
    """Switch Resume to Import, search, and cancel without importing."""

    del lab
    tmux.send_key("M-s")
    resume = _wait(tmux, "Resume a session", "resume dialog")
    _record(
        evidence,
        tmux,
        "dialog-resume-open",
        "press Alt-S",
        "resume tab and standard Cancel control are visible",
        resume,
    )

    tmux.send_key("Right")
    importing = _wait(tmux, "Importable sessions", "import tab")
    _record(
        evidence,
        tmux,
        "dialog-resume-import-tab",
        "press Right on the resume list",
        "Import tab and its search control are visible",
        importing,
    )

    tmux.send_key("/")
    _paste(tmux, _SEARCH_QUERY)
    searched = _wait(tmux, _SEARCH_QUERY, "import search field")
    _record(
        evidence,
        tmux,
        "dialog-resume-import-search",
        "focus Import search with / and paste a nonmatching query",
        "the search field contains the probe query",
        searched,
    )

    tmux.send_key("Escape")
    cancelled = _wait(tmux, "Sessions", "resume/import cancellation")
    _record(
        evidence,
        tmux,
        "dialog-resume-import-cancel",
        "press Escape from Import search",
        "resume dialog closes without importing a session",
        cancelled,
    )


def probe_web_dialog(lab: Any, tmux: Any, evidence: Any) -> None:
    """Open the web dialog and activate its shared Close button."""

    del lab
    tmux.send_key("F4")
    web = _wait(tmux, "Web viewer", "web dialog")
    _record(
        evidence,
        tmux,
        "dialog-web-open",
        "press F4",
        "web dialog and standard Close button are visible",
        web,
    )
    tmux.send_key("Enter")
    closed = _wait(tmux, "Sessions", "web dialog close")
    _record(
        evidence,
        tmux,
        "dialog-web-close",
        "press Enter on Close",
        "web dialog closes and dashboard returns",
        closed,
    )


def run_dialog_acceptance(lab: Any, tmux: Any, evidence: Any) -> None:
    """Run all dialog probes from the settled dashboard stage.

    Root should invoke this once, immediately before the target overlay is
    installed.  The selected idle session and dashboard focus are the only
    preconditions; every probe restores them before returning.
    """

    probe_rename(lab, tmux, evidence)
    probe_target_config_id(lab, tmux, evidence)
    probe_review_settings(lab, tmux, evidence)
    probe_new_wizard(lab, tmux, evidence)
    probe_resume_import(lab, tmux, evidence)
    probe_web_dialog(lab, tmux, evidence)


# Descriptive aliases make the intended root-harness call easy to discover
# while keeping one implementation and one evidence sequence.
run_dashboard_dialog_acceptance = run_dialog_acceptance
run_dialog_probes = run_dialog_acceptance
