"""Real dialog submissions: verify both domain effects and modal dismissal."""
import re
import time
import tomllib
import sqlite3

from reliability_lab import TIMEOUT_SCALE, ScenarioFailure


def record(tmux, evidence, label, inputs, expected):
    screen = tmux.capture()
    evidence.event(label, inputs, expected, expected, evidence.capture(label, screen))


def click(tmux, label):
    from tui_components_tmux import locate_text
    tmux.wait_for(label)
    # A control takes clicks once the frame that drew it has registered it.
    # Right after a step changes, a click on the first letter can land before
    # that, so let the dashboard settle.
    time.sleep(0.3 * TIMEOUT_SCALE)
    x, y = locate_text(tmux.wait_for(label), label, last=True)
    tmux.mouse_click(x + 2, y)


def absent(tmux, marker):
    tmux.wait_until(lambda: marker not in tmux.capture(), f"{marker!r} dialog dismissed")


def command(tmux, query, dialog):
    tmux.send_prefix(":")
    tmux.wait_for("Commands")
    tmux.send_text(query)
    tmux.wait_for(query.capitalize())
    tmux.send_key("Enter")
    tmux.wait_for(dialog)


def palette_viewport(tmux, evidence):
    """Exercise fit, overflow, filtering, and resize in the actual palette."""
    def popup_text():
        rows = tmux.capture().splitlines()
        top = next(index for index, line in enumerate(rows) if " Commands " in line)
        left = rows[top].index("╭")
        right = rows[top].index("╮", left)
        bottom = next(index for index in range(top + 1, len(rows))
                      if len(rows[index]) > right and rows[index][left] == "╰" and rows[index][right] == "╯")
        return "\n".join(line[left:right + 1] for line in rows[top:bottom + 1])

    # The palette lists a "Recent" group first, then every command in its
    # group: about 85 rows, so a roomy terminal has to be tall.
    tmux.resize(140, 100)
    # Palette groups follow the focused pane; use Sessions for this viewport probe.
    click(tmux, "Sessions")
    tmux.send_prefix(":")
    tmux.wait_for("Open settings")
    screen = popup_text()
    # Open settings is also listed under Recent, so look at its last place.
    settings = re.search(r"Settings\s+─", screen)
    anywhere = re.search(r"Anywhere\s+─", screen)
    order = [
        settings.start() if settings else -1,
        screen.rfind("Open settings"),
        anywhere.start() if anywhere else -1,
    ]
    if -1 in order or order != sorted(order):
        raise ScenarioFailure(f"Open settings is not in its Settings section:\n{screen}")
    if "Review settings…" in screen:
        raise ScenarioFailure("obsolete review settings palette entry remains")
    tmux.wait_for("Detach from this terminal")
    if "▐" in popup_text():
        raise ScenarioFailure("roomy palette unnecessarily shows a scrollbar")
    record(tmux, evidence, "palette-roomy", "Ctrl+B : at 140x100", "Settings and the final commands fit together")
    tmux.resize(100, 18)
    tmux.wait_for("Commands")
    tmux.wait_until(lambda: "▐" in popup_text(), "overflow palette scrollbar")
    # Tab moves from the query to the shared list; End must reveal its tail.
    tmux.send_key("Tab")
    tmux.send_key("End")
    tmux.wait_for("Detach from this terminal")
    tmux.wait_until(lambda: "› Dictation" in popup_text(), "the final Dictation command selected after End")
    record(tmux, evidence, "palette-scroll-end", "resize 100x18; Tab, End", "the final command rows remain reachable")
    tmux.send_key("BTab")
    tmux.send_text("open settings")
    tmux.wait_for("Open settings")
    tmux.wait_until(lambda: "▐" not in popup_text(), "filtered palette fits without scrollbar")
    record(tmux, evidence, "palette-filter-small", "Shift-Tab; type open settings", "filtered settings entry fits in the short palette")
    tmux.send_key("Enter")
    tmux.wait_for("× Settings")
    # Settings has no Cancel button; Escape closes it (through the dirty guard).
    tmux.send_key("Escape")
    absent(tmux, "× Settings")
    tmux.resize(140, 40)


def open_review_settings(tmux):
    tmux.send_prefix("s")
    tmux.wait_for("Code Review")
    # One click on a Settings row opens its page.
    click(tmux, "Code Review")
    tmux.wait_for("Add reviewer")


def save_review_settings(lab, tmux, evidence):
    """Review runs before push exactly when reviewers are configured."""
    original = lab.snapshot()["review_config"]
    if original["reviewers"]:
        raise ScenarioFailure("the review settings scenario starts without reviewers")
    open_review_settings(tmux)
    # Autoconfigure only picks models it knows to be supported, which a fixture
    # profile's are not, so add the first advertised model by hand.
    click(tmux, "Add reviewer")
    click(tmux, "Save Settings")
    absent(tmux, "× Settings")
    lab.wait_snapshot(lambda value: value["review_config"]["trigger"] == "push", "review turned on by its reviewers")
    record(
        tmux,
        evidence,
        "review-save-dismissed",
        "Add a reviewer and click Save",
        "settings close and review runs before push",
    )
    open_review_settings(tmux)
    for _ in lab.snapshot()["review_config"]["reviewers"]:
        click(tmux, "Remove")
    click(tmux, "Save Settings")
    absent(tmux, "× Settings")
    lab.wait_snapshot(lambda value: value["review_config"] == original, "original review settings restored")


def save_target_id(lab, tmux, evidence):
    from tui_components_tmux import locate_text
    for old, new in [("localhost", "localhost-probe"), ("localhost-probe", "localhost")]:
        x, y = locate_text(tmux.capture(), old, last=True)
        tmux.mouse_click(x, y)
        tmux.send_key("Enter")
        tmux.wait_for("Target actions")
        click(tmux, "  Rename  ")
        tmux.wait_for("Rename target ID")
        tmux.send_key("C-u")
        tmux.send_key("Enter")
        tmux.wait_for("Configuration ID cannot be empty")
        tmux.send_text(new)
        tmux.wait_for(new)
        click(tmux, "  Save  ")
        absent(tmux, "Rename target ID")
        absent(tmux, "Target actions")
        tmux.wait_until(lambda: new in tomllib.loads((lab.config / "config.toml").read_text())["targets"], "target ID persisted")
        tmux.wait_for(new)
        record(tmux, evidence, "target-id-save-" + new, "empty validation; type ID; Save", "Save closes the ID editor and its parent; config contains the new ID")


def save_container_settings(lab, tmux, evidence):
    command(tmux, "container settings", "Edit container")
    tmux.send_text("4")
    tmux.send_key("Tab")
    tmux.send_text("2g")
    tmux.wait_for("2g")
    click(tmux, "  Save  ")
    absent(tmux, "Edit container")
    # Notices can be replaced by attach progress. Check the durable write
    # itself before reopening, rather than depending on a transient notice.
    def persisted():
        with sqlite3.connect(f"file:{lab.data / 'mj.sqlite3'}?mode=ro", uri=True) as db:
            return db.execute(
                "SELECT container_cpus, container_memory FROM sessions"
            ).fetchall() == [("4", "2g")]
    tmux.wait_until(persisted, "container settings persisted")
    command(tmux, "container settings", "Edit container")
    tmux.wait_for("2g")
    record(tmux, evidence, "container-save-reopen", "Save 4 CPUs / 2g; reopen settings", "Save closes the editor and reopening loads the stored settings")
    click(tmux, "  Cancel  ")
    absent(tmux, "Edit container")


def stop_and_resume(lab, tmux, evidence, session_id, dialogs_only=False):
    tmux.send_prefix(":")
    tmux.wait_for("Commands")
    tmux.send_text("suspend session")
    tmux.wait_for("Suspend session")
    tmux.send_key("Escape")
    absent(tmux, " Commands ")
    if not any(row["id"] == session_id and row["state"] == "running" for row in lab.snapshot()["sessions"]):
        raise ScenarioFailure("Closing the palette suspended the session")
    tmux.send_prefix(":")
    tmux.wait_for("Commands")
    tmux.send_text("suspend session")
    tmux.wait_for("Suspend session")
    # Enter runs the entry; a clone whose publication is unknown asks first.
    tmux.send_key("Enter")
    tmux.wait_for("Suspend session?")
    click(tmux, "  Suspend session  ")
    absent(tmux, "Suspend session?")
    lab.wait_snapshot(lambda value: any(row["id"] == session_id and row["state"] == "suspended" for row in value["sessions"]), "session suspended with recovery copy")
    record(tmux, evidence, "stop-command-dismissed", "Close palette; reopen, Enter on Suspend session, confirm", "Closing the palette retains the session; running the command and confirming suspends it")
    from tui_review_discovery import exercise_offline_save
    if not dialogs_only:
        exercise_offline_save(lab, tmux, evidence)
    tmux.send_prefix("g")
    # A suspended session is on the Mjolnir tab, the second of the picker.
    tmux.wait_for("Running sessions")
    tmux.send_key("Right")
    # "No stopped Mjolnir sessions" shows while the list loads; the title with
    # the sort order replaces it once the rows are there.
    tmux.wait_for("Mjolnir sessions · newest first")
    # The arrow left the focus on the tab strip, so use the button.
    click(tmux, "  Resume  ")
    tmux.wait_for("Resume · 1/3 profile")
    tmux.send_key("Enter")
    tmux.wait_for("Resume · 2/3 new target")
    record(tmux, evidence, "resume-target-before-next", "advance Profile", "target selector and Next visible")
    click(tmux, "  Next  ")
    tmux.wait_for("Resume · 3/3 review")
    record(tmux, evidence, "resume-first-review", "click Next", "review visible before Back")
    click(tmux, "  Back  ")
    tmux.wait_for("Resume · 2/3 new target")
    record(tmux, evidence, "resume-target-before-next", "advance Profile", "target selector and Next visible")
    click(tmux, "  Next  ")
    tmux.wait_for("Resume · 3/3 review")
    click(tmux, "  Resume  ")
    absent(tmux, "Resume · 3/3 review")
    lab.wait_snapshot(lambda value: any(row["id"] == session_id and row["state"] == "running" for row in value["sessions"]), "same session resumed")
    record(tmux, evidence, "resume-submit-dismissed", "Back, Next, Resume", "final Resume closes its wizard and the same session runs again")


def destroy_confirmation(lab, tmux, evidence):
    sessions = lab.snapshot()["sessions"]
    if len(sessions) != 1:
        raise ScenarioFailure("destructive fixture probe requires exactly one owned session")
    session_id = sessions[0]["id"]
    # A confirmation names the session by its title, and by id only without one.
    named = f"Session: {sessions[0]['title']}"
    # Destroying is confirmed by naming it: Cancel and the default Enter keep
    # the session, and only the destroy button removes it.
    command(tmux, "destroy session", "Destroy session?")
    tmux.wait_for(named)
    click(tmux, "  Cancel  ")
    absent(tmux, "Destroy session?")
    if not any(row["id"] == session_id for row in lab.snapshot()["sessions"]):
        raise ScenarioFailure("Cancel destroyed the fixture session")
    command(tmux, "destroy session", "Destroy session?")
    tmux.send_key("Enter")
    absent(tmux, "Destroy session?")
    if not any(row["id"] == session_id for row in lab.snapshot()["sessions"]):
        raise ScenarioFailure("The default confirmation destroyed the fixture session")
    command(tmux, "destroy session", "Destroy session?")
    tmux.wait_for(named)
    click(tmux, "  Destroy session  ")
    absent(tmux, "Destroy session?")
    lab.wait_snapshot(lambda value: not any(row["id"] == session_id for row in value["sessions"]), "confirmed destruction removes only the owned fixture session")
    record(tmux, evidence, "destroy-confirmation-dismissed", "Cancel; default Enter; reopen and Destroy session", "Cancel and default Enter retain the session; Destroy session closes the dialog and removes the fixture session")


def create_through_dialog(lab, tmux, evidence):
    before = {row["id"] for row in lab.snapshot().get("sessions", [])}
    tmux.send_key("C-b")
    tmux.send_key("c")
    tmux.wait_for("New session · 1/4 profile")
    tmux.send_key("Home")
    tmux.send_key("Enter")
    tmux.wait_for("New session · 2/4 target")
    click(tmux, "  Next  ")
    tmux.wait_for("New session · 3/4 local project")
    # Clear the launch-directory default before checking empty-input validation.
    tmux.send_key("C-u")
    tmux.send_key("Enter")
    tmux.wait_for("Project directory cannot be empty")
    tmux.send_text("relative")
    tmux.send_key("Enter")
    tmux.wait_for("Enter an absolute path or a path starting with ~/")
    record(tmux, evidence, "new-invalid-project", "submit empty and relative paths", "validation stays in the editable project step")
    tmux.send_key("C-u")
    tmux.send_raw("\x1b[200~" + str(lab.project) + "\x1b[201~")
    tmux.wait_for(str(lab.project))
    click(tmux, "  Next  ")
    tmux.wait_for("New session · 4/4 review")
    click(tmux, "  Back  ")
    tmux.wait_for("New session · 3/4 local project")
    tmux.wait_for(str(lab.project))
    click(tmux, "  Next  ")
    tmux.wait_for("New session · 4/4 review")
    tmux.wait_for("Delegation")
    tmux.wait_for("Set up Mjolnir sub-agents")
    click(tmux, "Native")
    tmux.wait_for(" Values ")
    tmux.wait_for("Mjolnir")
    record(tmux, evidence, "create-delegation-disabled", "open Delegation", "Native and unavailable Mjolnir are shown with setup guidance")
    tmux.send_key("Escape")
    click(tmux, "  Create  ")
    lab.wait_snapshot(lambda value: any(row["id"] not in before for row in value.get("sessions", [])), "Create creates a durable session")
    record(tmux, evidence, "new-create-effect", "click Create on final review", "a new durable session exists")
    absent(tmux, "New session · 4/4 review")
    time.sleep(0.2)
    added = [row for row in lab.snapshot().get("sessions", []) if row["id"] not in before]
    if len(added) != 1:
        raise ScenarioFailure(f"Create produced {len(added)} sessions")
    record(tmux, evidence, "new-create-dismissed", "wait after Create", "review dialog closes and exactly one session was created")
    if not before:
        tmux.wait_for("Select a session to open its conversation.")
        record(tmux, evidence, "new-create-keeps-selection", "wait for background creation", "Create leaves the conversation empty until explicitly selected")
        from tui_components_tmux import locate_text
        title = added[0]["title"]
        x, y = locate_text(tmux.wait_for(title), title)
        tmux.mouse_click(x + 2, y)
        tmux.send_key("Enter")
    return added[0]["id"]
