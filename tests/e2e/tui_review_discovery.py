"""Check that review settings open and close without touching an agent adapter."""
import json

from reliability_lab import ScenarioFailure
from tui_components_actions import absent, click, open_review_settings, record


def requests(lab):
    path = lab.runtime_root / "fake-acp.log"
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def expect_no_adapter_traffic(lab, offset):
    """Settings read the daemon's cached choices, so they start no adapter."""
    messages = requests(lab)[offset:]
    if messages:
        raise ScenarioFailure(f"review settings reached an agent adapter: {messages}")


def exercise_offline_cancel(lab, tmux, evidence):
    """With the session stopped there is no worker to ask, yet choices remain."""
    original = lab.snapshot()["review_config"]
    offset = len(requests(lab))
    open_review_settings(tmux)
    click(tmux, "Add reviewer")
    # The model offered comes from the daemon's cache of what each profile
    # advertises, so it is there although no worker is running.
    tmux.wait_for("tiny ▾")
    click(tmux, "  Cancel  ")
    absent(tmux, "× Settings")
    expect_no_adapter_traffic(lab, offset)
    if lab.snapshot()["review_config"] != original:
        raise ScenarioFailure("cancelling review settings changed them")
    record(tmux, evidence, "review-offline-cancel", "stop the session; open Code Review; Add reviewer; Cancel", "cached choices are offered without a worker, no adapter starts, and Cancel keeps the settings")
