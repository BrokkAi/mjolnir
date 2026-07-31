#!/usr/bin/env node

// One deterministic ACP fixture plays the primary agent or a subagent according
// to the model Mjolnir selects before the first prompt. It also makes probe
// sessions cheap.
import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";

const resultPath = process.env.MJ_E2E_PRIMARY_RESULT;
const primaryLog = process.env.MJ_E2E_PRIMARY_LOG;
const nestedLog = process.env.MJ_E2E_NESTED_LOG;
const mode = process.env.MJ_E2E_MODE ?? "complete";
const parallelMode = mode === "parallel";
// Two subagents in the first turn give the discrete-review gate the >1 handoff
// it needs, and give the batching path two reports to fold into one injection.
const subagentCount = parallelMode || mode === "review" || mode === "details" ? 2 : 1;
const longMessage = (prefix, fill, suffix) => `${prefix} ${fill.repeat(720)} ${suffix}`;
const subagentPrompt = mode === "details"
  ? longMessage("DELEGATION_LONG_PREFIX", "d", "DELEGATION_LONG_SUFFIX")
  : process.env.MJ_E2E_SUBAGENT_PROMPT ?? "Return SUBAGENT_E2E_OK";
let selectedModel = "gpt-5.6-sol";
let reasoning = "medium";
let mcpServer = null;
let mcpSessionId = null;
let mcpReady = null;
let promptRequestId = null;
let terminalRequestId = null;
let directiveCount = 0;
let primaryPromptSeen = false;
let injectionCount = 0;
let clientCapabilities = null;
let sessionMcpServers = [];
let launchCount = 0;
const launches = [];

const MCP_SERVER = "mj-subagents";
const SPAWN_TOOL = "create_subagent";

const modelOptions = [
  ["gpt-5.6-sol", "GPT-5.6-Sol"],
  ["gpt-5.5", "GPT-5.5"],
  ["gpt-5.6-terra", "GPT-5.6-Terra"],
  ["gpt-5.6-luna", "GPT-5.6-Luna"],
  ["fable", "Fable 5"],
  ["opus[1m]", "Opus 4.8"],
  ["sonnet", "Sonnet 5"],
];

function configOptions() {
  return [
    { id: "model", name: "Model", category: "model", type: "select", currentValue: selectedModel,
      options: modelOptions.map(([value, name]) => ({ value, name })) },
    { id: "reasoning", name: "Reasoning", category: "thought_level", type: "select", currentValue: reasoning,
      options: ["low", "medium", "high"].map((value) => ({ value, name: value[0].toUpperCase() + value.slice(1) })) },
  ];
}

function send(message) { process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`); }
function append(path, value) { if (path) fs.appendFileSync(path, `${value}\n`); }
function update(update) { send({ method: "session/update", params: { sessionId: "fixture-session", update } }); }
function isSubagent() { return selectedModel === "gpt-5.6-luna"; }
function log(value) { append(isSubagent() ? nestedLog : primaryLog, value); }

function mcpHeaders(includeAuth = true, sessionId = null) {
  const headers = { "content-type": "application/json", accept: "application/json, text/event-stream" };
  if (includeAuth) for (const header of mcpServer.headers ?? []) headers[header.name] = header.value;
  if (sessionId) headers["mcp-session-id"] = sessionId;
  return headers;
}

function parseMcpResponse(text) {
  const trimmed = text.trim();
  if (!trimmed) return null;
  if (trimmed.startsWith("{")) return JSON.parse(trimmed);
  return trimmed.split(/\r?\n/).filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trim()).filter(Boolean)
    .map((line) => JSON.parse(line)).at(-1) ?? null;
}

async function postMcp(body, includeAuth = true) {
  const response = await fetch(mcpServer.url, { method: "POST", headers: mcpHeaders(includeAuth, mcpSessionId), body: JSON.stringify(body) });
  const message = parseMcpResponse(await response.text());
  mcpSessionId = response.headers.get("mcp-session-id") ?? mcpSessionId;
  return { status: response.status, message };
}

async function prepareMcp() {
  const unauthorized = await postMcp({ jsonrpc: "2.0", id: "bad", method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "fixture", version: "1" } } }, false);
  const initialized = await postMcp({ jsonrpc: "2.0", id: "init", method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "fixture", version: "1" } } });
  if (initialized.status !== 200 || !mcpSessionId) throw new Error("MCP initialize failed");
  await postMcp({ jsonrpc: "2.0", method: "notifications/initialized", params: {} });
  // The subagent policy travels via the MCP server's instructions, not as
  // text appended to the first user prompt.
  const instructions = initialized.message?.result?.instructions ?? "";
  if (!instructions.includes("<mj-subagent-policy>")) {
    throw new Error("mj-subagents server instructions are missing the subagent policy");
  }
  if (instructions.includes("Available agents and models:")) {
    throw new Error("MCP server instructions leaked the model inventory");
  }
  directiveCount += 1; append(primaryLog, `session-directive:${directiveCount}`);
  const listed = await postMcp({ jsonrpc: "2.0", id: "list", method: "tools/list", params: {} });
  const tools = listed.message?.result?.tools ?? [];
  const spawn = tools.find((tool) => tool.name === SPAWN_TOOL);
  if (!spawn) throw new Error(`${SPAWN_TOOL} missing`);
  if (!tools.some((tool) => tool.name === "subagent_cancel")) throw new Error("subagent_cancel missing");
  if (spawn.description?.includes("Available agents and models:")) {
    throw new Error("create_subagent description leaked the model inventory");
  }
  const properties = spawn.inputSchema?.properties ?? {};
  if ("agent" in properties || "model" in properties) {
    throw new Error("create_subagent schema still exposes per-call model routing");
  }
  append(primaryLog, `tools:${tools.map((tool) => tool.name).sort().join(",")}`);
  return unauthorized.status;
}

function finishPrimary(text) {
  if (promptRequestId === null) return;
  update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
  const requestId = promptRequestId;
  promptRequestId = null;
  send({ id: requestId, result: { stopReason: "end_turn" } });
}

function subagentResult() {
  return mode === "details"
    ? longMessage("SUBAGENT_LONG_PREFIX", "e", "SUBAGENT_LONG_SUFFIX")
    : "SUBAGENT_E2E_OK";
}

function primaryReviewResult() {
  return mode === "details"
    ? longMessage("PRIMARY_LONG_PREFIX", "t", "PRIMARY_LONG_SUFFIX")
    : "PRIMARY FINAL REVIEWED";
}

// The discrete review runs three kinds of read-only session: one intent
// analyst on the subagent pool, the specialist lanes, and the adversarial
// supervisor on the primary's model. All three arrive through this same
// fixture, so they are recognized by prompt shape rather than by model.
// Answering them immediately keeps the fan-out deterministic: a session that
// fell through to the ordinary subagent path would sit in the
// permission/terminal dance until its timeout.
function answerReviewSession(text) {
  if (text.includes("You are a read-only intent analyst")) {
    append(primaryLog, "review-intent:1");
    finishPrimary("Goal\nfixture intent brief");
    return true;
  }
  const lane = text.match(/specialist review lane in a fresh, read-only session: `([\w-]+)`/);
  if (lane) {
    append(primaryLog, `review-lane:${lane[1]}`);
    // Exactly one copy of the lane packet: `wx` lets the first lane create the
    // file and makes every other lane fail instead of interleaving its own
    // write. The assertions read the reviewed diff out of this file.
    if (process.env.MJ_E2E_REVIEW_LOG) {
      try {
        fs.writeFileSync(process.env.MJ_E2E_REVIEW_LOG, text, { flag: "wx" });
      } catch (error) {
        if (error.code !== "EEXIST") throw error;
      }
    }
    finishPrimary("[P2] subagent-change.txt:1 -- fixture lane finding (evidence: source-reviewed)");
    return true;
  }
  if (text.includes("You are the adversarial review supervisor for one completed user turn")) {
    append(primaryLog, "review-synthesis:1");
    // Anything but the clean sentinel on the first line means "findings", which
    // is what sends the corrective prompt back to the primary.
    finishPrimary(
      "[P2] subagent-change.txt:1 -- fixture synthesis finding (evidence: source-reviewed; lanes: fixture)",
    );
    return true;
  }
  return false;
}

// Both shapes the review can take: the fan-out's corrective prompt when the
// synthesis produced findings, and the single-prompt fallback used when the
// fan-out itself failed.
function isDiscreteReviewPrompt(text) {
  return text.includes("<review_findings") || text.includes("Perform a discrete review");
}

function recordLaunches() {
  if (resultPath) fs.writeFileSync(resultPath, JSON.stringify({ ...launches.at(-1), launches }));
}

// create_subagent returns immediately; the primary must NOT wait for a result
// here. It records the started acknowledgement and ends its turn. mj injects
// each finished subagent's report as a new user message later.
async function launchSubagents() {
  const unauthorizedStatus = await mcpReady;
  for (let index = 0; index < subagentCount; index += 1) {
    launchCount += 1;
    const toolSentAt = Date.now();
    const called = await postMcp({
      jsonrpc: "2.0",
      id: `call-${launchCount}`,
      method: "tools/call",
      params: { name: SPAWN_TOOL, arguments: { prompt: subagentPrompt, label: `lane-${launchCount}` } },
    });
    const toolReceivedAt = Date.now();
    const response = called.message?.result;
    launches.push({ response, toolSentAt, toolReceivedAt, unauthorizedStatus });
    recordLaunches();
    const text = response?.content?.map((item) => item.text ?? "").join("") ?? "";
    append(primaryLog, `launched:${response?.structuredContent?.subagentId ?? "?"}`);
    if (response?.isError) {
      finishPrimary(`PRIMARY LAUNCH FAILED: ${text}`);
      return;
    }
  }
  if (mode === "cancel") {
    // Exercise the subagent_cancel tool against a subagent that is hanging
    // mid-turn: wait for its prompt to start, cancel it, and record the
    // catch-up tail the tool result carries.
    const nestedLog = process.env.MJ_E2E_NESTED_LOG;
    const id = launches.at(-1)?.response?.structuredContent?.subagentId;
    for (let poll = 0; poll < 100; poll += 1) {
      try { if (fs.readFileSync(nestedLog, "utf8").includes("prompt-started")) break; } catch {}
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    const cancelled = await postMcp({
      jsonrpc: "2.0",
      id: "cancel-1",
      method: "tools/call",
      params: { name: "subagent_cancel", arguments: { subagent_id: id } },
    });
    const result = cancelled.message?.result;
    const cancelText = result?.content?.map((item) => item.text ?? "").join("") ?? "";
    append(primaryLog, `cancel-result:${result?.isError ? "ERROR:" : ""}${cancelText.replaceAll("\n", " ")}`);
    finishPrimary("PRIMARY CANCELLED SUBAGENT");
    return;
  }
  finishPrimary(`PRIMARY STARTED ${launchCount} SUBAGENTS`);
}

function startSubagentTurn(prompt) {
  if (process.env.MJ_E2E_NESTED_PID) fs.writeFileSync(process.env.MJ_E2E_NESTED_PID, String(process.pid));
  log("prompt-started");
  log(`subagent-prompt:${prompt.slice(0, 200)}`);
  update({ sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "fixture reasoning" } });
  if (mode === "cancel" || mode === "inline-stream") {
    fs.writeFileSync(
      path.join(process.env.MJ_E2E_WORKSPACE, "subagent-partial.txt"),
      "partial change by the subagent\n",
    );
    return;
  }
  if (mode === "failed") {
    fs.writeFileSync(
      path.join(process.env.MJ_E2E_WORKSPACE, "subagent-partial.txt"),
      "partial change by the subagent\n",
    );
    send({ id: promptRequestId, error: { code: -32603, message: "fixture subagent failure" } });
    promptRequestId = null;
    return;
  }
  requestSubagentPermission();
}

function requestSubagentPermission() {
  send({ id: "permission-1", method: "session/request_permission", params: {
    sessionId: "fixture-session", toolCall: { toolCallId: "nested-tool", title: "allow fixture command", kind: "execute" },
    options: [{ optionId: "allow-once", name: "Allow once", kind: "allow_once" }, { optionId: "reject-once", name: "Reject", kind: "reject_once" }],
  }});
}

const input = readline.createInterface({ input: process.stdin });
input.on("close", () => process.exit(0));
input.on("line", (line) => {
  const message = JSON.parse(line);
  if (message.method === "initialize") {
    clientCapabilities = message.params?.clientCapabilities ?? null;
    send({ id: message.id, result: { protocolVersion: 1, agentCapabilities: { mcpCapabilities: { http: process.env.MJ_E2E_HTTP_UNSUPPORTED !== "1", sse: false } }, agentInfo: { name: "subagent-fixture", version: "1" } } });
  } else if (message.method === "session/new") {
    sessionMcpServers = message.params?.mcpServers ?? [];
    mcpServer = sessionMcpServers.find((server) => server.name === MCP_SERVER);
    send({ id: message.id, result: { sessionId: "fixture-session", configOptions: configOptions() } });
  } else if (message.method === "session/set_config_option") {
    if (message.params.configId === "model") selectedModel = message.params.value;
    if (message.params.configId === "reasoning") reasoning = message.params.value;
    log(`config:${message.params.configId}=${message.params.value}`);
    send({ id: message.id, result: { configOptions: configOptions() } });
  } else if (message.method === "session/prompt") {
    if (mcpServer && !mcpReady) mcpReady = prepareMcp();
    promptRequestId = message.id;
    const text = message.params?.prompt?.[0]?.text ?? "";
    // Review lanes and the synthesis pass are separate read-only sessions; they
    // are answered before the primary/subagent branches because a lane happens
    // to run on the same model as the subagent pool.
    if (answerReviewSession(text)) return;
    if (reasoning !== "high") { send({ id: message.id, error: { code: -32602, message: "High was not selected before prompt" } }); return; }
    if (!isSubagent() && !primaryPromptSeen) {
      primaryPromptSeen = true;
      if (process.env.MJ_E2E_PRIMARY_PID) fs.writeFileSync(process.env.MJ_E2E_PRIMARY_PID, String(process.pid));
      // The policy lives in the MCP server instructions; prompt text must
      // stay exactly what the user typed.
      if (text.includes("<mj-subagent-policy>")) {
        send({ id: message.id, error: { code: -32602, message: "the subagent policy must stay out of prompt text" } });
        return;
      }
      update({ sessionUpdate: "usage_update", used: 12000, size: 128000 });
      update({ sessionUpdate: "usage_update", used: 2000, size: 128000 });
    }
    if (isSubagent()) {
      startSubagentTurn(text);
    } else if (text.includes("<subagent_result")) {
      // mj pushed a finished subagent's report back into the primary session.
      injectionCount += 1;
      append(primaryLog, `injection:${injectionCount}:${text}`);
      const blocks = text.match(/<subagent_result /g)?.length ?? 0;
      append(primaryLog, `injected-blocks:${blocks}`);
      finishPrimary(`PRIMARY RECEIVED: ${subagentResult()}`);
    } else if (isDiscreteReviewPrompt(text)) {
      append(primaryLog, `discrete-review:${text}`);
      update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: primaryReviewResult() } });
      send({ id: promptRequestId, result: { stopReason: "end_turn" } });
      promptRequestId = null;
    } else if (mode === "no-change") {
      finishPrimary("PRIMARY NO CHANGE");
    } else {
      void launchSubagents().catch((error) => { if (resultPath) fs.writeFileSync(resultPath, JSON.stringify({ error: String(error) })); finishPrimary(`PRIMARY FAILED: ${error.message}`); });
    }
  } else if (message.id === "permission-1") {
    log(`permission:${JSON.stringify(message.result)}`);
    terminalRequestId = "terminal-1";
    send({ id: terminalRequestId, method: "terminal/create", params: { sessionId: "fixture-session", command: "/bin/sh", args: ["-lc", "printf nested-terminal-output; printf 'changed by the subagent\\n' >> subagent-change.txt"], cwd: process.env.MJ_E2E_WORKSPACE } });
  } else if (message.id === terminalRequestId) {
    update({ sessionUpdate: "tool_call", toolCallId: "nested-tool", title: "fixture terminal command", kind: "execute", status: "in_progress", content: [{ type: "terminal", terminalId: message.result.terminalId }] });
    setTimeout(() => {
      update({ sessionUpdate: "tool_call_update", toolCallId: "nested-tool", status: "completed" });
      update({ sessionUpdate: "tool_call", toolCallId: "codex-meta-tool", title: "fixture codex metadata command", kind: "execute", status: "in_progress", content: [{ type: "terminal", terminalId: "codex-meta-tool" }] });
      update({ sessionUpdate: "tool_call_update", toolCallId: "codex-meta-tool", status: "completed", _meta: {
        terminal_output: { terminal_id: "codex-meta-tool", data: "codex-metadata-terminal-output" },
        terminal_exit: { terminal_id: "codex-meta-tool", exit_code: 0, signal: null },
      } });
      update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: subagentResult() } });
      log(`completion:${Date.now()}`); send({ id: promptRequestId, result: { stopReason: "end_turn" } });
      promptRequestId = null;
    }, 250);
  } else if (message.method === "session/cancel") {
    log("cancel-received");
    if (promptRequestId !== null) {
      const requestId = promptRequestId;
      promptRequestId = null;
      send({ id: requestId, result: { stopReason: "cancelled" } });
    }
  }
});
// `clientCapabilities` is retained for future read-only assertions; touch it so
// the fixture keeps failing loudly if the field disappears from `initialize`.
if (clientCapabilities === undefined) throw new Error("client capabilities were never observed");
