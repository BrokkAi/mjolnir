"""Check review choice discovery through real terminal input and ACP traffic."""
import json
import time

from reliability_lab import ScenarioFailure
from tui_components_actions import absent, click, command, record


def requests(lab):
    path = lab.runtime_root / "fake-acp.log"
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def expect_discoveries(lab, offset, count):
    messages = requests(lab)[offset:]
    starts = sum(message.get("method") == "initialize" for message in messages)
    if starts != count:
        raise ScenarioFailure(f"expected {count} review adapter starts, got {starts}: {messages}")
    forbidden = [message for message in messages
                 if message.get("method") == "session/prompt"
                 or (message.get("method") == "session/set_config_option"
                     and message.get("params", {}).get("configId") == "effort")]
    if forbidden:
        raise ScenarioFailure(f"settings unexpectedly prompted or applied effort: {forbidden}")


def select_on_row(tmux, row_label, value):
    screen = tmux.wait_for(value)
    rows = screen.splitlines()
    y = next(index for index, row in enumerate(rows) if row_label in row and value in row)
    tmux.mouse_click(rows[y].index(value) + 1, y)


def exercise_choices(lab, tmux, evidence):
    """The caller leaves a loaded fake profile selected in an unsaved draft."""
    tmux.wait_for("Choices loaded")
    offset = len(requests(lab))
    select_on_row(tmux, "Tier", "Quick")
    select_on_row(tmux, "Effort", "High")
    tmux.send_key("Enter")
    time.sleep(0.8)
    expect_discoveries(lab, offset, 0)
    record(tmux, evidence, "review-local-edits", "change tier and effort; Enter on effort", "no adapter starts, prompts, or effort applications")

    offset = len(requests(lab))
    select_on_row(tmux, "Model", "Tiny fixture")
    tmux.wait_for("Choices loaded")
    # A fresh request may briefly leave the old frame on screen.
    tmux.wait_until(lambda: sum(m.get("method") == "initialize" for m in requests(lab)[offset:]) == 1, "one model discovery")
    tmux.wait_for("Choices loaded")
    time.sleep(0.4)
    expect_discoveries(lab, offset, 1)
    record(tmux, evidence, "review-model-discovery", "select Tiny fixture", "one adapter discovers model-specific efforts")

    offset = len(requests(lab))
    select_on_row(tmux, "Model", "Profile default")
    tmux.wait_for("Choices loaded")
    select_on_row(tmux, "Model", "Tiny fixture")
    tmux.send_key("Enter")
    time.sleep(0.8)
    expect_discoveries(lab, offset, 0)
    record(tmux, evidence, "review-model-cache", "revisit default and Tiny; Enter", "both cached models avoid new adapter starts")

    offset = len(requests(lab))
    click(tmux, "  Refresh choices  ")
    tmux.send_key("F1")
    tmux.wait_for("Keyboard shortcuts")
    tmux.wait_until(lambda: sum(m.get("method") == "initialize" for m in requests(lab)[offset:]) == 1, "one explicit refresh")
    time.sleep(0.8)
    tmux.send_key("Escape")
    absent(tmux, "Keyboard shortcuts")
    tmux.wait_for("Choices loaded")
    expect_discoveries(lab, offset, 1)
    record(tmux, evidence, "review-refresh-help", "Refresh choices; Help while loading", "one refresh completes while Help covers the dialog")


def exercise_cached_reopen(lab, tmux, evidence):
    # First open primes the persisted profile even if an earlier draft selected
    # another one; the second must reuse those successfully discovered choices.
    command(tmux, "review settings", "Automatic review")
    tmux.wait_for("Choices loaded")
    click(tmux, "  Cancel  ")
    absent(tmux, "╭ Setup")
    offset = len(requests(lab))
    command(tmux, "review settings", "Automatic review")
    tmux.wait_for("Choices loaded")
    time.sleep(0.8)
    expect_discoveries(lab, offset, 0)
    record(tmux, evidence, "review-reopen-cache", "close and reopen review settings", "cached choices appear without adapter starts")
    click(tmux, "  Refresh choices  ")
    click(tmux, "  Save Setup  ")
    absent(tmux, "╭ Setup")
    record(tmux, evidence, "review-save-during-refresh", "Refresh choices then immediately Save", "Save closes the form while the background request is cancelled")
    command(tmux, "review settings", "Automatic review")
    tmux.wait_for("Choices loaded")
    click(tmux, "  Cancel  ")
    absent(tmux, "╭ Setup")


def exercise_offline_save(lab, tmux, evidence):
    original = lab.snapshot()["review_config"]
    offset = len(requests(lab))
    command(tmux, "review settings", "Automatic review")
    click(tmux, "  Refresh choices  ")
    tmux.wait_for("connected session")
    # Existing choices remain useful even when no worker can refresh them.
    tmux.wait_for("Tiny fixture")
    click(tmux, "  Save Setup  ")
    absent(tmux, "╭ Setup")
    expect_discoveries(lab, offset, 0)
    if lab.snapshot()["review_config"] != original:
        raise ScenarioFailure("offline Save changed the unedited review settings")
    record(tmux, evidence, "review-offline-save", "Refresh without a connected session; Save", "cached choices remain and Save closes the dialog without an adapter start")
