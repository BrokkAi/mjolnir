"""Real dialog submissions: verify both domain effects and modal dismissal."""
import time
import tomllib
import sqlite3

from reliability_lab import ScenarioFailure


def record(tmux, evidence, label, inputs, expected):
    screen = tmux.capture()
    evidence.event(label, inputs, expected, expected, evidence.capture(label, screen))


def click(tmux, label):
    from tui_components_tmux import locate_text
    x, y = locate_text(tmux.wait_for(label), label, last=True)
    tmux.mouse_click(x + 2, y)


def absent(tmux, marker):
    tmux.wait_until(lambda: marker not in tmux.capture(), f"{marker!r} dialog dismissed")


def command(tmux, query, dialog):
    tmux.send_key("F2")
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

    tmux.resize(140, 60)
    # Palette groups follow the focused pane; use Sessions for this viewport probe.
    click(tmux, "Sessions")
    tmux.send_key("F2")
    tmux.wait_for("Open setup")
    screen = popup_text()
    if not screen.index("Settings") < screen.index("Open setup") < screen.index("Anywhere"):
        raise ScenarioFailure(f"Setup is not in its Settings section:\n{screen}")
    if "Review settings…" in screen:
        raise ScenarioFailure("obsolete review settings palette entry remains")
    tmux.wait_for("Detach from this terminal")
    if "▐" in popup_text():
        raise ScenarioFailure("roomy palette unnecessarily shows a scrollbar")
    record(tmux, evidence, "palette-roomy", "F2 at 140x60", "Settings and the final commands fit together")
    tmux.resize(100, 18)
    tmux.wait_for("Commands")
    tmux.wait_until(lambda: "▐" in popup_text(), "overflow palette scrollbar")
    # Tab moves from the query to the shared list; End must reveal its tail.
    tmux.send_key("Tab")
    tmux.send_key("End")
    tmux.wait_for("Detach from this terminal")
    tmux.wait_until(lambda: "› Help" in popup_text(), "the final Help command selected after End")
    record(tmux, evidence, "palette-scroll-end", "resize 100x18; Tab, End", "the final command rows remain reachable")
    tmux.send_key("BTab")
    tmux.send_text("open setup")
    tmux.wait_for("Open setup")
    tmux.wait_until(lambda: "▐" not in popup_text(), "filtered palette fits without scrollbar")
    record(tmux, evidence, "palette-filter-small", "Shift-Tab; type open setup", "filtered settings entry fits in the short palette")
    tmux.send_key("Enter")
    tmux.wait_for("╭ Setup")
    click(tmux, "  Cancel  ")
    absent(tmux, "╭ Setup")
    tmux.resize(140, 40)


def open_review_settings(tmux):
    tmux.send_key("F7")
    tmux.wait_for("Code Review")
    click(tmux, "Code Review")
    tmux.send_key("Enter")
    tmux.wait_for("Automatic review")


def save_review_settings(lab, tmux, evidence):
    original = lab.snapshot()["review_config"]
    open_review_settings(tmux)
    # The fixture starts disabled, so changing tier can be saved independently
    # of target discovery. Validate persistence and disappearance separately.
    click(tmux, "Quick")
    tmux.wait_for("One general reviewer; a validator checks any findings.")
    click(tmux, "  Save Setup  ")
    absent(tmux, "╭ Setup")
    lab.wait_snapshot(lambda value: value["review_config"]["tier"].lower() == "quick", "review tier persisted")
    record(tmux, evidence, "review-save-dismissed", "choose Quick and click Save", "settings close and persisted tier changes")
    open_review_settings(tmux)
    click(tmux, "Extended" if original["tier"].lower() == "extended" else "Quick")
    if original["tier"].lower() == "extended":
        tmux.wait_for("A supervisor selects specialist reviewers for deeper coverage.")
    click(tmux, "  Save Setup  ")
    absent(tmux, "╭ Setup")
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


def stop_and_resume(lab, tmux, evidence, session_id):
    tmux.send_key("F2")
    tmux.wait_for("Commands")
    tmux.send_text("stop session")
    tmux.wait_for(" 1 commands ")
    tmux.wait_for("Stop session")
    click(tmux, "  Close  ")
    absent(tmux, " Commands ")
    if not any(row["id"] == session_id and row["state"] == "running" for row in lab.snapshot()["sessions"]):
        raise ScenarioFailure("Closing the palette stopped the session")
    tmux.send_key("F2")
    tmux.wait_for("Commands")
    tmux.send_text("stop session")
    tmux.wait_for(" 1 commands ")
    tmux.wait_for("Stop session")
    click(tmux, "  Run  ")
    absent(tmux, " Commands ")
    lab.wait_snapshot(lambda value: any(row["id"] == session_id and row["state"] == "stopped" for row in value["sessions"]), "session stopped with recovery copy")
    record(tmux, evidence, "stop-command-dismissed", "Close palette then reopen and Run Stop", "Close retains the session; Run stops it and closes the palette")
    from tui_review_discovery import exercise_offline_save
    exercise_offline_save(lab, tmux, evidence)
    tmux.send_key("M-s")
    tmux.wait_for("Resume a session")
    tmux.send_key("Enter")
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
    command(tmux, "delete session", "Delete session?")
    tmux.wait_for(session_id)
    click(tmux, "  No  ")
    absent(tmux, "Delete session?")
    if not any(row["id"] == session_id for row in lab.snapshot()["sessions"]):
        raise ScenarioFailure("No deleted the fixture session")
    command(tmux, "delete session", "Delete session?")
    tmux.send_key("Enter")
    absent(tmux, "Delete session?")
    if not any(row["id"] == session_id for row in lab.snapshot()["sessions"]):
        raise ScenarioFailure("The default confirmation deleted the fixture session")
    command(tmux, "delete session", "Delete session?")
    tmux.wait_for(session_id)
    click(tmux, "  Yes  ")
    absent(tmux, "Delete session?")
    lab.wait_snapshot(lambda value: not any(row["id"] == session_id for row in value["sessions"]), "confirmed deletion removes only the owned fixture session")
    record(tmux, evidence, "delete-confirmation-dismissed", "No; default Enter; reopen and Yes", "No and default Enter retain the session; Yes closes the dialog and removes the fixture session")


def create_through_dialog(lab, tmux, evidence):
    before = {row["id"] for row in lab.snapshot().get("sessions", [])}
    tmux.send_key("M-n")
    tmux.wait_for("New session · 1/4 profile")
    tmux.send_key("Home")
    tmux.send_key("Enter")
    tmux.wait_for("New session · 2/4 target")
    click(tmux, "  Next  ")
    tmux.wait_for("New session · 3/4 local project")
    # Invalid submission must preserve the form and its editable field.
    tmux.send_key("Enter")
    tmux.wait_for("Project directory cannot be empty")
    tmux.send_text("relative")
    tmux.send_key("Enter")
    tmux.wait_for("Project directory must be an absolute")
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
    click(tmux, "  Create  ")
    lab.wait_snapshot(lambda value: any(row["id"] not in before for row in value.get("sessions", [])), "Create creates a durable session")
    record(tmux, evidence, "new-create-effect", "click Create on final review", "a new durable session exists")
    absent(tmux, "New session · 4/4 review")
    time.sleep(0.2)
    added = [row for row in lab.snapshot().get("sessions", []) if row["id"] not in before]
    if len(added) != 1:
        raise ScenarioFailure(f"Create produced {len(added)} sessions")
    record(tmux, evidence, "new-create-dismissed", "wait after Create", "review dialog closes and exactly one session was created")
    return added[0]["id"]
