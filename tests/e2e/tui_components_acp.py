#!/usr/bin/python3
"""Deterministic ACP peer for live terminal forms; never calls a provider."""
import json
import os
import pathlib
import sys
import time

if sys.argv[1:] == ["--version"]:
    print("@agentclientprotocol/codex-acp 1.8.0")
    raise SystemExit(0)

session_id = f"components-{os.getpid()}"
options = [
    {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "tiny", "options": [{"value": "tiny", "name": "Tiny fixture"}, {"value": "wide", "name": "Wide fixture"}]},
    {"id": "effort", "name": "Effort", "type": "select", "currentValue": "low", "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
]


def send(message):
    print(json.dumps(message, separators=(",", ":")), flush=True)


def read_message():
    line = sys.stdin.readline()
    if not line:
        return None
    message = json.loads(line)
    with open(os.environ["MJ_FAKE_ACP_LOG"], "a", encoding="utf-8") as log:
        log.write(json.dumps(message) + "\n")
    return message


def response(ident, result):
    send({"jsonrpc": "2.0", "id": ident, "result": result})


def update(text):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})


def ask(kind):
    if kind == "form":
        method = "elicitation/create"
        params = {"sessionId": session_id, "mode": "form", "message": "Component form: edit the label and choose the options.", "requestedSchema": {
            "type": "object", "required": ["a_label"], "properties": {
                "a_label": {"type": "string", "title": "Label"},
                "b_enabled": {"type": "boolean", "title": "Enabled", "default": False},
                "c_choice": {"type": "string", "title": "Choice", "oneOf": [{"const": "first", "title": "First"}, {"const": "second", "title": "Second"}]},
                "d_several": {"type": "array", "title": "Several", "items": {"type": "string", "enum": ["Alpha", "Beta", "Gamma"]}},
            }}}
    else:
        method = "session/request_permission"
        params = {"sessionId": session_id, "toolCall": {"toolCallId": "live-plan", "kind": "switch_mode", "title": "Ready to code?", "rawInput": {"plan": "Component plan\n\nInspect reusable controls, then validate terminal input.", "planFilePath": "/workspace/plan.md"}}, "options": [{"optionId": value, "name": value, "kind": "reject_once" if value == "reject" else "allow_always"} for value in ["default", "auto", "bypassPermissions", "reject"]]}
    ident = f"live-{kind}"
    send({"jsonrpc": "2.0", "id": ident, "method": method, "params": params})
    while (message := read_message()) is not None:
        if message.get("id") == ident:
            update(f"component {kind} response: " + json.dumps(message.get("result", {})))
            return "end_turn"
        if message.get("method") == "session/cancel":
            return "cancelled"
        if message.get("id") is not None:
            response(message["id"], {})
    return "cancelled"


while (message := read_message()) is not None:
    method, ident = message.get("method"), message.get("id")
    if method == "initialize":
        result = {"protocolVersion": 1}
    elif method in ("session/new", "session/load"):
        time.sleep(int(os.environ.get("MJ_FAKE_ACP_DELAY_MS", "0")) / 1000)
        session_id = message.get("params", {}).get("sessionId", session_id)
        rollout_dir = pathlib.Path(os.environ["CODEX_HOME"]) / "sessions" / "2026" / "09" / "06"
        rollout_dir.mkdir(parents=True, exist_ok=True)
        with (rollout_dir / f"rollout-{session_id}.jsonl").open("a") as rollout:
            rollout.write(json.dumps({"type": "session_meta", "payload": {"session_id": session_id}}) + "\n")
        result = {"sessionId": session_id, "configOptions": options, "modes": {"currentModeId": "agent", "availableModes": [{"id": value, "name": value} for value in ["default", "agent", "agent-full-access"]]}}
    elif method == "session/set_config_option":
        params = message.get("params", {})
        for option in options:
            if option["id"] == params.get("configId"):
                option["currentValue"] = params.get("value")
        result = {"configOptions": options}
    elif method == "session/prompt":
        blocks = message.get("params", {}).get("prompt", [])
        text = " ".join(block.get("text", "") for block in blocks if block.get("type") == "text")
        text = text.rsplit("</mj-project-memory>", 1)[-1].strip()
        if text == "live component form":
            result = {"stopReason": ask("form")}
        elif text == "live component plan":
            result = {"stopReason": ask("plan")}
        else:
            if text == "live review changes":
                with pathlib.Path("README.md").open("a") as output:
                    output.write("\nA deterministic change for live review controls.\n")
            time.sleep(int(os.environ.get("MJ_FAKE_ACP_PROMPT_DELAY_MS", "0")) / 1000)
            update("reliability reply: " + text)
            result = {"stopReason": "end_turn"}
    elif ident is None:
        continue
    else:
        result = {}
    response(ident, result)
