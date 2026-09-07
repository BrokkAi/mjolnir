#!/usr/bin/env python3
"""Deterministic MSP peer for Mjolnir tests through the real muse-acp binary."""
import json
import os
from pathlib import Path
import sys

SESSION = "01991be0-0000-7000-8000-000000000001"
turn = 0
workspace = os.getcwd()
mode = "promptUnmatched"


def send(value):
    print(json.dumps(value), flush=True)


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


def session():
    return {"sessionId": SESSION, "workspaceRoot": workspace, "modelId": "muse-test",
            "activeTurnId": None, "approvalMode": {"mode": mode, "source": "explicit", "lastCommandId": None}}


def finish(terminal="completed"):
    notify("turn/completed", {"sessionId": SESSION, "turnId": f"turn-{turn}", "terminal": terminal})


for line in sys.stdin:
    request = json.loads(line)
    method, params = request.get("method"), request.get("params", {})
    if "id" not in request or not method:
        continue
    with Path(os.environ["MJ_MUSE_TEST_LOG"]).open("a") as log:
        log.write(json.dumps(request) + "\n")
    result = {}
    after = None
    if method == "initialize":
        result = {"schema": {"fingerprint": "test"}, "capabilities": {}}
    elif method == "model/list":
        result = {"models": [{"modelId": "muse-test", "displayLabel": "Muse test"}], "source": "fixture"}
    elif method == "session/start":
        workspace = params.get("workspaceRoot", workspace)
        mode = params.get("approvalMode", mode)
        result = {"session": session(), "viewCursor": "cursor-0"}
    elif method == "session/resume":
        result = {"session": session(), "viewCursor": "cursor-1", "pendingRequests": [],
                  "history": {"mode": "inline", "items": [{"itemId": "old", "kind": "agentMessage", "text": "must not replay"}]}}
    elif method == "session/setApprovalMode":
        mode = params.get("mode", "promptUnmatched")
        result = {"effectiveMode": {"mode": mode, "source": "explicit", "lastCommandId": None}}
    elif method == "turn/start":
        turn += 1
        result = {"commandId": params.get("commandId"), "status": "accepted", "turnId": f"turn-{turn}",
                  "disposition": "started", "startedNewTurn": True}
        scenario = os.environ.get("MJ_MUSE_TEST_SCENARIO", "chat")
        if scenario == "chat":
            after = "chat"
        elif scenario == "permission":
            after = "permission"
        elif scenario == "question":
            after = "question"
    elif method in ("approval/decide", "userInput/answer", "userInput/cancel"):
        after = "finish"
    elif method == "turn/cancel":
        after = "cancel"
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})
    base = {"sessionId": SESSION, "turnId": f"turn-{turn}"}
    if after == "chat":
        notify("item/completed", {**base, "item": {"itemId": f"message-{turn}", "kind": "agentMessage", "status": "completed", "text": "Muse test reply"}})
        finish()
    elif after == "permission":
        notify("approval/requested", {"sessionId": SESSION, "approvalId": "permission-1", "toolCallId": "tool-1",
            "toolName": "workspace-shell", "subject": {"kind": "shell", "command": "touch example.txt"},
            "availableChoices": [{"choiceId": "allow", "label": "Allow once", "decision": "approved", "scope": "once"},
                                 {"choiceId": "deny", "label": "Deny", "decision": "denied", "scope": "once"}]})
    elif after == "question":
        notify("userInput/requested", {"sessionId": SESSION, "userInputId": "question-1", "questions": [
            {"id": "choice", "header": "Choose", "question": "Which implementation?", "selection": {"mode": "single"},
             "options": [{"label": "First"}, {"label": "Second"}]}]})
    elif after == "finish":
        finish()
    elif after == "cancel":
        finish("cancelled")
