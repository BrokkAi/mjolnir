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


def save_review_settings(lab, tmux, evidence):
    original = lab.snapshot()["review_config"]
    command(tmux, "review settings", "Automatic review")
    # The fixture starts disabled, so changing tier can be saved independently
    # of target discovery. Validate persistence and disappearance separately.
    click(tmux, "Quick")
    click(tmux, "[ Save ]")
    absent(tmux, "┌ Review settings")
    lab.wait_snapshot(lambda value: value["review_config"]["tier"].lower() == "quick", "review tier persisted")
    record(tmux, evidence, "review-save-dismissed", "choose Quick and click Save", "settings close and persisted tier changes")
    command(tmux, "review settings", "Automatic review")
    click(tmux, "Extended" if original["tier"].lower() == "extended" else "Quick")
    click(tmux, "[ Save ]")
    absent(tmux, "┌ Review settings")
    lab.wait_snapshot(lambda value: value["review_config"] == original, "original review settings restored")


def save_target_id(lab, tmux, evidence):
    from tui_components_tmux import locate_text
    for old, new in [("localhost", "localhost-probe"), ("localhost-probe", "localhost")]:
        x, y = locate_text(tmux.capture(), old, last=True)
        tmux.mouse_click(x, y)
        tmux.send_key("Enter")
        tmux.wait_for("Target actions")
        click(tmux, "[ Rename ]")
        tmux.wait_for("Rename target ID")
        tmux.send_key("C-u")
        tmux.send_key("Enter")
        tmux.wait_for("Configuration ID cannot be empty")
        tmux.send_text(new)
        tmux.wait_for(new)
        click(tmux, "[ Save ]")
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
    click(tmux, "[ Save ]")
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
    click(tmux, "[ Cancel ]")
    absent(tmux, "Edit container")


def stop_and_resume(lab, tmux, evidence, session_id):
    command(tmux, "stop session", "Stop session?")
    click(tmux, "[ Cancel ]")
    absent(tmux, "Stop session?")
    if not any(row["id"] == session_id and row["state"] == "running" for row in lab.snapshot()["sessions"]):
        raise ScenarioFailure("Cancel stopped the session")
    command(tmux, "stop session", "Stop session?")
    click(tmux, "[ Stop ]")
    absent(tmux, "Stop session?")
    lab.wait_snapshot(lambda value: any(row["id"] == session_id and row["state"] == "stopped" for row in value["sessions"]), "session stopped with recovery copy")
    record(tmux, evidence, "stop-confirm-dismissed", "Cancel then reopen and Stop", "Cancel retains the session; Stop closes confirmation and stops it")
    tmux.send_key("M-s")
    tmux.wait_for("Resume a session")
    tmux.send_key("Enter")
    tmux.wait_for("Resume · 1/3 profile")
    tmux.send_key("Enter")
    tmux.wait_for("Resume · 2/3 new target")
    record(tmux, evidence, "resume-target-before-next", "advance Profile", "target selector and Next visible")
    click(tmux, "[ Next ]")
    tmux.wait_for("Resume · 3/3 review")
    record(tmux, evidence, "resume-first-review", "click Next", "review visible before Back")
    click(tmux, "[ Back ]")
    tmux.wait_for("Resume · 2/3 new target")
    record(tmux, evidence, "resume-target-before-next", "advance Profile", "target selector and Next visible")
    click(tmux, "[ Next ]")
    tmux.wait_for("Resume · 3/3 review")
    click(tmux, "[ Resume ]")
    absent(tmux, "Resume · 3/3 review")
    lab.wait_snapshot(lambda value: any(row["id"] == session_id and row["state"] == "running" for row in value["sessions"]), "same session resumed")
    record(tmux, evidence, "resume-submit-dismissed", "Back, Next, Resume", "final Resume closes its wizard and the same session runs again")


def destroy_confirmation(lab, tmux, evidence):
    sessions = lab.snapshot()["sessions"]
    if len(sessions) != 1:
        raise ScenarioFailure("destructive fixture probe requires exactly one owned session")
    session_id = sessions[0]["id"]
    command(tmux, "force destroy session", "FORCE DESTROY")
    click(tmux, "[ Cancel ]")
    absent(tmux, "FORCE DESTROY")
    if not any(row["id"] == session_id for row in lab.snapshot()["sessions"]):
        raise ScenarioFailure("Cancel destroyed the fixture session")
    command(tmux, "force destroy session", "FORCE DESTROY")
    tmux.send_key("Enter")
    tmux.wait_for("FORCE DESTROY")
    wrong = "00000000" if session_id[:8] != "00000000" else "11111111"
    tmux.send_text(wrong)
    tmux.send_key("Enter")
    tmux.wait_for("FORCE DESTROY")
    tmux.send_key("C-u")
    tmux.send_text(session_id[:8])
    tmux.wait_for(session_id[:8])
    click(tmux, "[ Force destroy ]")
    absent(tmux, "FORCE DESTROY")
    lab.wait_snapshot(lambda value: not any(row["id"] == session_id for row in value["sessions"]), "typed destroy removes only the owned fixture session")
    record(tmux, evidence, "typed-destroy-dismissed", "Cancel; reopen; invalid confirmation; correct short ID; Force destroy", "invalid confirmation retains the dialog; valid confirmation closes it and removes the fixture session")


def create_through_dialog(lab, tmux, evidence):
    before = {row["id"] for row in lab.snapshot().get("sessions", [])}
    tmux.send_key("M-n")
    tmux.wait_for("New session · 1/4 profile")
    tmux.send_key("Home")
    tmux.send_key("Enter")
    tmux.wait_for("New session · 2/4 target")
    click(tmux, "[ Next ]")
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
    click(tmux, "[ Next ]")
    tmux.wait_for("New session · 4/4 review")
    click(tmux, "[ Back ]")
    tmux.wait_for("New session · 3/4 local project")
    tmux.wait_for(str(lab.project))
    click(tmux, "[ Next ]")
    tmux.wait_for("New session · 4/4 review")
    click(tmux, "[ Create ]")
    lab.wait_snapshot(lambda value: any(row["id"] not in before for row in value.get("sessions", [])), "Create creates a durable session")
    record(tmux, evidence, "new-create-effect", "click Create on final review", "a new durable session exists")
    absent(tmux, "New session · 4/4 review")
    time.sleep(0.2)
    added = [row for row in lab.snapshot().get("sessions", []) if row["id"] not in before]
    if len(added) != 1:
        raise ScenarioFailure(f"Create produced {len(added)} sessions")
    record(tmux, evidence, "new-create-dismissed", "wait after Create", "review dialog closes and exactly one session was created")
    return added[0]["id"]
