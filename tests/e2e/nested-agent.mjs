#!/usr/bin/env node

import fs from "node:fs";
import readline from "node:readline";

const mode = process.env.MJ_E2E_MODE ?? "complete";
const logPath = process.env.MJ_E2E_NESTED_LOG;
if (process.env.MJ_E2E_NESTED_PID) fs.writeFileSync(process.env.MJ_E2E_NESTED_PID, String(process.pid));
let promptRequestId = null;
let terminalRequestId = null;

function send(message) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
}

function log(value) {
  if (logPath) fs.appendFileSync(logPath, `${value}\n`);
}

function update(update) {
  send({ method: "session/update", params: { sessionId: "nested-session", update } });
}

function finishWithTerminal(terminalId) {
  update({
    sessionUpdate: "tool_call",
    toolCallId: "nested-tool",
    title: "fixture terminal command",
    kind: "execute",
    status: "in_progress",
    content: [{ type: "terminal", terminalId }],
  });
  setTimeout(() => {
    update({
      sessionUpdate: "tool_call_update",
      toolCallId: "nested-tool",
      status: "completed",
    });
    update({
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "CODEAGENT_E2E_OK" },
    });
    log(`completion:${Date.now()}`);
    send({ id: promptRequestId, result: { stopReason: "end_turn" } });
  }, 250);
}

const input = readline.createInterface({ input: process.stdin });
input.on("line", (line) => {
  const message = JSON.parse(line);
  if (message.method === "initialize") {
    if (message.params?.clientCapabilities?._meta?.mj?.codeAgent) {
      send({ id: message.id, error: { code: -32602, message: "recursive capability advertised" } });
      return;
    }
    send({
      id: message.id,
      result: {
        protocolVersion: 1,
        agentCapabilities: {},
        agentInfo: { name: "e2e-nested", version: "1" },
      },
    });
    return;
  }
  if (message.method === "session/new") {
    send({ id: message.id, result: { sessionId: "nested-session" } });
    return;
  }
  if (message.method === "session/prompt") {
    promptRequestId = message.id;
    log("prompt-started");
    update({
      sessionUpdate: "agent_thought_chunk",
      content: { type: "text", text: "fixture reasoning" },
    });
    if (mode === "cancel") return;
    send({
      id: "permission-1",
      method: "session/request_permission",
      params: {
        sessionId: "nested-session",
        toolCall: { toolCallId: "nested-tool", title: "allow fixture command", kind: "execute" },
        options: [
          { optionId: "allow-once", name: "Allow once", kind: "allow_once" },
          { optionId: "reject-once", name: "Reject", kind: "reject_once" },
        ],
      },
    });
    return;
  }
  if (message.id === "permission-1") {
    log(`permission:${JSON.stringify(message.result)}`);
    terminalRequestId = "terminal-1";
    send({
      id: terminalRequestId,
      method: "terminal/create",
      params: {
        sessionId: "nested-session",
        command: "/bin/sh",
        args: ["-lc", "printf nested-terminal-output"],
      },
    });
    return;
  }
  if (message.id === terminalRequestId) {
    finishWithTerminal(message.result.terminalId);
    return;
  }
  if (message.method === "session/cancel") {
    log("cancel-received");
    if (promptRequestId !== null) {
      send({ id: promptRequestId, result: { stopReason: "cancelled" } });
      promptRequestId = null;
    }
  }
});
