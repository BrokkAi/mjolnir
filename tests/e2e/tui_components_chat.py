"""Live chat acceptance against the test-owned deterministic ACP peer."""
import json
import time

from reliability_lab import ScenarioFailure

# The composer's top border starts with its microphone chip; the pane has no
# other title to find it by.
COMPOSER_BORDER = "╭  mic"


def run_chat_controls(lab, tmux, evidence, session_id):
    from tui_components_tmux import locate_text

    def record(label, inputs, expected):
        screen = tmux.capture()
        evidence.event(label, inputs, expected, expected, evidence.capture(label, screen))
        return screen

    def focus_prompt():
        screen = tmux.wait_for(COMPOSER_BORDER)
        x, y = locate_text(screen, COMPOSER_BORDER, last=True)
        tmux.mouse_click(x + 2, y + 1)
        time.sleep(0.1)

    def click_text(label):
        screen = tmux.wait_for(label)
        x, y = locate_text(screen, label, last=True)
        tmux.mouse_click(x, y)
        time.sleep(0.15)

    # Reverse search finds a prompt sent from this composer, so the stored
    # row, its full-text index entry and the composer's paged search agree.
    # Prompts sent through the HTTP API are not recorded as history.
    focus_prompt()
    tmux.send_text("history needle prompt")
    tmux.send_key("Enter")
    tmux.wait_for("reliability reply: history needle prompt", "composer prompt answered")
    tmux.send_key("C-r")
    tmux.send_text("ne")
    tmux.wait_for("type 3+ characters", "short history query explained")
    tmux.send_text("edle pro")
    tmux.wait_for("reverse-i-search [all projects]: needle pro  1/1 ", "history search match")
    tmux.wait_for("> history needle prompt", "matched prompt in the composer")
    record("history-search", "send a prompt; Ctrl-R; type ne, then edle pro", "short-query hint, then the stored prompt as 1/1")
    tmux.send_key("M-r")
    tmux.wait_for("reverse-i-search [project]: needle pro  1/1 ", "history search narrowed to the project")
    tmux.send_key("Escape")
    tmux.wait_until(lambda: "reverse-i-search" not in tmux.capture(), "history search cancelled")
    record("history-search-scope", "Alt-R; Escape", "project scope keeps the match; Escape restores the draft")

    focus_prompt()
    tmux.send_key("BTab")
    screen = tmux.wait_for(COMPOSER_BORDER)
    x, y = locate_text(screen, COMPOSER_BORDER, last=True)
    tmux.mouse_event(0, x + 2, y + 1)
    # Prove keyboard input reaches the prompt while the button is still held.
    # Replaying the press on release cannot make this assertion pass.
    tmux.send_text("held-button-focus")
    tmux.wait_for("held-button-focus", "prompt focused before mouse release")
    record("prompt-focus-on-press", "Shift-Tab; Prompt mouse down; type before release", "prompt accepts input while mouse is held")
    tmux.mouse_event(0, x + 2, y + 1, release=True)
    tmux.send_key("End")
    tmux.send_key("C-u")
    tmux.send_text("/model")
    tmux.send_key("Enter")
    tmux.wait_for("Choose a model")
    tmux.send_text("wide")
    tmux.wait_for("Wide fixture")
    tmux.send_key("Tab")
    tmux.send_key("Enter")
    tmux.wait_until(lambda: "Choose a model" not in tmux.capture(), "model selection applied")
    log_path = lab.runtime_root / "fake-acp.log"
    def selected_wide():
        messages = [json.loads(line) for line in log_path.read_text().splitlines() if line.strip()]
        return any(message.get("method") == "session/set_config_option" and message.get("params", {}).get("value") == "wide" for message in messages)
    tmux.wait_until(selected_wide, "real model action received by fake ACP")
    record("chat-model-selection", "click Prompt; /model; type wide; Tab; Enter", "model selection sent to ACP")

    focus_prompt()
    tmux.send_text("/effort")
    tmux.send_key("Enter")
    tmux.wait_for("Choose a effort")
    click_text("High")
    click_text("Apply")
    tmux.wait_until(lambda: "Choose a effort" not in tmux.capture(), "effort picker closed by mouse Apply")
    record("chat-effort-mouse", "/effort; SGR click High; SGR click Apply", "picker selection and Apply work by mouse")

    status, _ = lab.request("POST", "/api/actions", {"action": "prompt", "session_id": session_id, "text": "live component form"})
    if status != 202:
        raise ScenarioFailure(f"form prompt returned {status}")
    tmux.wait_for("Component form")
    # Find the actual field label in the drawn modal. The inline editor follows it.
    screen = tmux.capture()
    x, y = locate_text(screen, "> ", last=True)
    tmux.mouse_click(x + 2, y)
    tmux.send_raw("\x1b[200~live 資料 answer\x1b[201~")
    tmux.wait_for("answer")
    record("chat-elicitation-field", "SGR click Label; bracketed Unicode paste", "question field accepts Unicode paste")
    tmux.send_key("Tab")
    tmux.wait_for("Enabled")
    tmux.send_key("Space")
    tmux.send_key("Tab")
    tmux.wait_for("Second")
    # Mouse-select a standard option, then submit through the real action button.
    click_text("Second")
    screen = tmux.wait_for("Submit")
    x, y = locate_text(screen, "Submit", last=True)
    tmux.mouse_drag_outside(x, y)
    time.sleep(0.15)
    tmux.wait_for("Component form")
    record("chat-elicitation-outside-release", "Submit down; drag outside; release", "question remains open")
    click_text("Submit")
    tmux.wait_for("component form response:")
    record("chat-elicitation-submit", "SGR click Submit", "question response received by ACP")

    status, _ = lab.request("POST", "/api/actions", {"action": "prompt", "session_id": session_id, "text": "live component plan"})
    if status != 202:
        raise ScenarioFailure(f"plan prompt returned {status}")
    click_text("Keep planning")
    click_text("Submit")
    tmux.wait_for("component plan response:")

    status, _ = lab.request("POST", "/api/actions", {"action": "prompt", "session_id": session_id, "text": "live review changes"})
    if status != 202:
        raise ScenarioFailure(f"review fixture prompt returned {status}")
    tmux.wait_for("reliability reply: live review changes")
    focus_prompt()
    tmux.send_text("/review")
    tmux.send_key("Enter")
    tmux.wait_for("Turn review", timeout=30)
    tmux.wait_for("Overview", timeout=30)
    click_text("Overview")
    tmux.send_key("Right")
    time.sleep(0.2)
    record("turn-review-tabs", "/review; click Overview; Right to first role", "role tab accepts keyboard navigation during the live review")
    tmux.send_key("Home")
    time.sleep(0.15)
    record("turn-review-overview", "Home on review tabs", "overview tab remains reachable as roles update")
    click_text("Cancel")
    tmux.wait_until(lambda: "  Cancel  " not in tmux.capture(), "turn review cancellation", timeout=30)
    record("turn-review-cancel", "click Cancel", "review cancellation is delivered through the shared action bar")
