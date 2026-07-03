#!/usr/bin/env node
// Minimal ACP agent over stdio (ndjson JSON-RPC). Enough for mj to open a
// session and run prompt turns; echoes the prompt back as an agent message.
// A prompt containing the word "slow" delays its reply so tests can observe
// the in-flight "working" state and the cancel path.
import { createInterface } from "node:readline";

const rl = createInterface({ input: process.stdin });

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function promptText(params) {
  return (params.prompt || [])
    .filter((b) => b && b.type === "text")
    .map((b) => b.text || "")
    .join(" ");
}

let sessionCounter = 0;
// Sessions currently sleeping on a "slow" turn, so session/cancel can abort
// them promptly (and return the ACP-mandated `cancelled` stop reason).
const inFlight = new Map();

rl.on("line", async (line) => {
  line = line.trim();
  if (!line) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  const { method, id } = msg;
  if (method === "initialize") {
    send({ jsonrpc: "2.0", id, result: { protocolVersion: 1 } });
  } else if (method === "session/new") {
    sessionCounter += 1;
    // Advertise a couple of select config options so the viewer renders its
    // searchable comboboxes and the config-change path is exercisable.
    send({
      jsonrpc: "2.0",
      id,
      result: {
        sessionId: `stub-${sessionCounter}-${Date.now().toString(36)}`,
        configOptions: [
          {
            id: "model",
            name: "Model",
            type: "select",
            currentValue: "model-1",
            options: [
              { value: "model-1", name: "Model One" },
              { value: "model-2", name: "Model Two" },
              { value: "model-3", name: "Model Three" },
            ],
          },
          {
            id: "mode",
            name: "Mode",
            type: "select",
            currentValue: "chat",
            options: [
              { value: "chat", name: "Chat" },
              { value: "agent", name: "Agent" },
            ],
          },
        ],
      },
    });
  } else if (method === "session/set_config_option") {
    // Accept config changes so mj's set-config path completes cleanly.
    send({ jsonrpc: "2.0", id, result: {} });
  } else if (method === "session/cancel") {
    const sid = msg.params && msg.params.sessionId;
    const pending = inFlight.get(sid);
    if (pending) {
      clearTimeout(pending.timer);
      inFlight.delete(sid);
      pending.finish("cancelled");
    }
  } else if (method === "session/prompt") {
    const sid = msg.params.sessionId;
    const text = promptText(msg.params);
    const update = (u) =>
      send({ jsonrpc: "2.0", method: "session/update", params: { sessionId: sid, update: u } });
    const finish = (stopReason) => {
      if (stopReason === "end_turn") {
        // A thought chunk (renders as a "thought" entry), then a rich
        // markdown reply that exercises every markdown-lite branch: heading,
        // bold, inline code, link, bare url, fenced code block, and both
        // list kinds. The literal "stub reply: <text>" keeps existing
        // assertions working.
        update({
          sessionUpdate: "agent_thought_chunk",
          content: { type: "text", text: "considering the request" },
        });
        const rich = [
          "stub reply: " + text,
          "",
          "# Heading",
          "",
          "Some **bold** and `inline code` and a [link](https://example.com/docs) plus https://example.org bare.",
          "",
          "- first bullet",
          "- second bullet",
          "",
          "1. step one",
          "2. step two",
          "",
          "```js",
          "const x = 1;",
          "```",
        ].join("\n");
        update({
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: rich },
        });
      }
      send({ jsonrpc: "2.0", id, result: { stopReason } });
    };
    if (text.includes("slow")) {
      const timer = setTimeout(() => {
        inFlight.delete(sid);
        finish("end_turn");
      }, 6000);
      inFlight.set(sid, { timer, finish });
    } else {
      finish("end_turn");
    }
  } else if (id !== undefined && id !== null) {
    send({
      jsonrpc: "2.0",
      id,
      error: { code: -32601, message: `unsupported: ${method}` },
    });
  }
});
