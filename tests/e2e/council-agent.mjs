#!/usr/bin/env node

// One deterministic ACP fixture plays Thor or Eitri according to the model
// Mjolnir selects before the first prompt. It also makes probe sessions cheap.
import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";

const resultPath = process.env.MJ_E2E_PRIMARY_RESULT;
const primaryLog = process.env.MJ_E2E_PRIMARY_LOG;
const nestedLog = process.env.MJ_E2E_NESTED_LOG;
const mode = process.env.MJ_E2E_MODE ?? "complete";
const exploreMode = mode === "explore" || mode === "explore-cancel";
const longMessage = (prefix, fill, suffix) => `${prefix} ${fill.repeat(720)} ${suffix}`;
const instructions = mode === "details"
  ? longMessage("DELEGATION_LONG_PREFIX", "d", "DELEGATION_LONG_SUFFIX")
  : process.env.MJ_E2E_CODE_AGENT_INSTRUCTIONS ?? "Return CODEAGENT_E2E_OK";
let selectedModel = "gpt-5.6-sol";
let reasoning = "medium";
let mcpServer = null;
let mcpToolName = null;
let mcpSessionId = null;
let mcpReady = null;
let promptRequestId = null;
let terminalRequestId = null;
let directiveCount = 0;
let lokiIntervened = false;
let clientCapabilities = null;
let sessionMcpServers = [];
let primaryHandoffCount = 0;
const eitriResponses = [];

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
function isEitri() { return selectedModel === "gpt-5.6-luna"; }
function isLoki() { return selectedModel === "fable"; }
function log(value) { append(isEitri() ? nestedLog : primaryLog, value); }

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
  const listed = await postMcp({ jsonrpc: "2.0", id: "list", method: "tools/list", params: {} });
  if (!(listed.message?.result?.tools ?? []).some((tool) => tool.name === mcpToolName)) throw new Error(`${mcpToolName} missing`);
  return unauthorized.status;
}

function finishPrimary(text) {
  update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
  send({ id: promptRequestId, result: { stopReason: "end_turn" } });
}

function eitriResult() {
  return mode === "details"
    ? longMessage("EITRI_LONG_PREFIX", "e", "EITRI_LONG_SUFFIX")
    : "CODEAGENT_E2E_OK";
}

function thorReviewResult() {
  return mode === "details"
    ? longMessage("THOR_LONG_PREFIX", "t", "THOR_LONG_SUFFIX")
    : "PRIMARY FINAL REVIEWED";
}

async function callEitri() {
  primaryHandoffCount += 1;
  const unauthorizedStatus = await mcpReady;
  const toolSentAt = Date.now();
  const name = exploreMode ? "explore_agent" : "code_agent";
  const arguments_ = exploreMode
    ? { prompt: "very thorough: trace the deterministic fixture architecture" }
    : { instructions };
  const called = await postMcp({ jsonrpc: "2.0", id: "call", method: "tools/call", params: { name, arguments: arguments_ } });
  const toolReceivedAt = Date.now();
  const response = called.message?.result;
  const record = { response, toolSentAt, toolReceivedAt, unauthorizedStatus };
  eitriResponses.push(record);
  if (resultPath) fs.writeFileSync(resultPath, JSON.stringify({ ...record, responses: eitriResponses }));
  const text = response?.content?.map((item) => item.text ?? "").join("") ?? "";
  if (!response?.isError && primaryHandoffCount === 1 && (mode === "thor-review" || mode === "details")) {
    await callEitri();
    return;
  }
  finishPrimary(response?.isError ? `PRIMARY CANCELLED: ${text}` : `PRIMARY RECEIVED: ${text}`);
}

function startEitriTurn(prompt) {
  if (process.env.MJ_E2E_NESTED_PID) fs.writeFileSync(process.env.MJ_E2E_NESTED_PID, String(process.pid));
  log("prompt-started");
  if (exploreMode) {
    const fsCapabilities = clientCapabilities?.fs ?? {};
    const readOnly = clientCapabilities?.terminal === false
      && fsCapabilities.readTextFile === true
      && fsCapabilities.writeTextFile === false;
    log(`explore-runtime:read-only=${readOnly}:mcp-servers=${sessionMcpServers.length}`);
    log(`explore-prompt:${prompt}`);
    if (!readOnly || sessionMcpServers.length !== 0
        || !prompt.includes("READ-ONLY EXPLORATION")
        || !prompt.includes("Thoroughness level: very thorough")
        || !prompt.includes("trace the deterministic fixture architecture")) {
      send({ id: promptRequestId, error: { code: -32602, message: "invalid explore runtime contract" } });
      return;
    }
    update({ sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "fixture exploration reasoning" } });
    update({ sessionUpdate: "tool_call", toolCallId: "explore-search", title: "search fixture architecture", kind: "search", status: "in_progress" });
    update({ sessionUpdate: "tool_call_update", toolCallId: "explore-search", status: "completed", content: [{ type: "content", content: { type: "text", text: "/fixture/entry.mjs" } }] });
    if (mode === "explore-cancel") return;
    setTimeout(() => {
      update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "EXPLORE_E2E_OK" } });
      log(`completion:${Date.now()}`);
      send({ id: promptRequestId, result: { stopReason: "end_turn" } });
    }, 250);
    return;
  }
  update({ sessionUpdate: "agent_thought_chunk", content: { type: "text", text: "fixture reasoning" } });
  if (mode === "cancel" || mode === "inline-stream" || mode === "failed") {
    fs.writeFileSync(
      path.join(process.env.MJ_E2E_WORKSPACE, "eitri-partial.txt"),
      "partial change by Eitri\n",
    );
    if (mode === "failed") {
      send({ id: promptRequestId, error: { code: -32603, message: "fixture Eitri failure" } });
    }
    return;
  }
  requestEitriPermission();
}

function requestEitriPermission() {
  send({ id: "permission-1", method: "session/request_permission", params: {
    sessionId: "fixture-session", toolCall: { toolCallId: "nested-tool", title: "allow fixture command", kind: "execute" },
    options: [{ optionId: "allow-once", name: "Allow once", kind: "allow_once" }, { optionId: "reject-once", name: "Reject", kind: "reject_once" }],
  }});
}

async function runLokiTurn(text) {
  append(process.env.MJ_E2E_LOKI_LOG, `prompt:${text}`);
  let critique = null;
  if (!lokiIntervened && mode === "loki-eitri" && text.includes("### Eitri session update")) {
    lokiIntervened = true;
    critique = "Eitri must retry after the fixture critique.";
  } else if (!lokiIntervened && mode === "loki-thor" && text.includes("### Thor session update")) {
    lokiIntervened = true;
    critique = "Thor must retry after the fixture critique.";
  }
  if (critique) {
    await mcpReady;
    const advised = await postMcp({ jsonrpc: "2.0", id: `advise-${Date.now()}`, method: "tools/call", params: { name: "advise", arguments: { note: critique } } });
    append(process.env.MJ_E2E_LOKI_LOG, `advise:${critique}:${JSON.stringify(advised.message?.result ?? advised.message)}`);
  } else {
    append(process.env.MJ_E2E_LOKI_LOG, "no-advice");
  }
  update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "NO_ADVICE" } });
  send({ id: promptRequestId, result: { stopReason: "end_turn" } });
}

const input = readline.createInterface({ input: process.stdin });
input.on("close", () => process.exit(0));
input.on("line", (line) => {
  const message = JSON.parse(line);
  if (message.method === "initialize") {
    clientCapabilities = message.params?.clientCapabilities ?? null;
    send({ id: message.id, result: { protocolVersion: 1, agentCapabilities: { mcpCapabilities: { http: process.env.MJ_E2E_HTTP_UNSUPPORTED !== "1", sse: false } }, agentInfo: { name: "council-fixture", version: "1" } } });
  } else if (message.method === "session/new") {
    sessionMcpServers = message.params?.mcpServers ?? [];
    mcpServer = sessionMcpServers.find((server) => server.name === "mj-code-agent" || server.name === "mj-loki-advisor");
    mcpToolName = mcpServer?.name === "mj-loki-advisor" ? "advise" : (exploreMode ? "explore_agent" : "code_agent");
    send({ id: message.id, result: { sessionId: "fixture-session", configOptions: configOptions() } });
    if (mcpServer) mcpReady = prepareMcp();
  } else if (message.method === "session/set_config_option") {
    if (message.params.configId === "model") selectedModel = message.params.value;
    if (message.params.configId === "reasoning") reasoning = message.params.value;
    log(`config:${message.params.configId}=${message.params.value}`);
    send({ id: message.id, result: { configOptions: configOptions() } });
  } else if (message.method === "session/prompt") {
    promptRequestId = message.id;
    const text = message.params?.prompt?.[0]?.text ?? "";
    if (reasoning !== "high") { send({ id: message.id, error: { code: -32602, message: "High was not selected before prompt" } }); return; }
    if (text.includes("<mj-code-agent-policy>")) {
      if (process.env.MJ_E2E_PRIMARY_PID) fs.writeFileSync(process.env.MJ_E2E_PRIMARY_PID, String(process.pid));
      directiveCount += 1; append(primaryLog, `session-directive:${directiveCount}`);
      const policyOffset = text.indexOf("\n\n<mj-code-agent-policy>");
      if (policyOffset <= 0 || !text.endsWith("</mj-code-agent-policy>")) {
        send({ id: message.id, error: { code: -32602, message: "policy was not appended to the first user prompt" } });
        return;
      }
      if (directiveCount === 1) {
        update({ sessionUpdate: "usage_update", used: 12000, size: 128000 });
        update({ sessionUpdate: "usage_update", used: 2000, size: 128000 });
      }
    }
    if (isEitri()) {
      startEitriTurn(text);
    } else if (isLoki()) {
      void runLokiTurn(text).catch((error) => {
        append(process.env.MJ_E2E_LOKI_LOG, `error:${error.stack ?? error}`);
        send({ id: promptRequestId, error: { code: -32603, message: error.message } });
      });
    } else if (text.includes("Perform Thor's discrete review")) {
      append(primaryLog, `discrete-review:${text}`);
      update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: thorReviewResult() } });
      send({ id: promptRequestId, result: { stopReason: "end_turn" } });
    } else if (mode === "no-change") {
      finishPrimary("PRIMARY NO CHANGE");
    } else {
      void callEitri().catch((error) => { if (resultPath) fs.writeFileSync(resultPath, JSON.stringify({ error: String(error) })); finishPrimary(`PRIMARY FAILED: ${error.message}`); });
    }
  } else if (message.id === "permission-1") {
    log(`permission:${JSON.stringify(message.result)}`);
    terminalRequestId = "terminal-1";
    send({ id: terminalRequestId, method: "terminal/create", params: { sessionId: "fixture-session", command: "/bin/sh", args: ["-lc", "printf nested-terminal-output; printf 'changed by Eitri\\n' > eitri-change.txt"], cwd: process.env.MJ_E2E_WORKSPACE } });
  } else if (message.id === terminalRequestId) {
    update({ sessionUpdate: "tool_call", toolCallId: "nested-tool", title: "fixture terminal command", kind: "execute", status: "in_progress", content: [{ type: "terminal", terminalId: message.result.terminalId }] });
    setTimeout(() => {
      update({ sessionUpdate: "tool_call_update", toolCallId: "nested-tool", status: "completed" });
      update({ sessionUpdate: "tool_call", toolCallId: "codex-meta-tool", title: "fixture codex metadata command", kind: "execute", status: "in_progress", content: [{ type: "terminal", terminalId: "codex-meta-tool" }] });
      update({ sessionUpdate: "tool_call_update", toolCallId: "codex-meta-tool", status: "completed", _meta: {
        terminal_output: { terminal_id: "codex-meta-tool", data: "codex-metadata-terminal-output" },
        terminal_exit: { terminal_id: "codex-meta-tool", exit_code: 0, signal: null },
      } });
      update({ sessionUpdate: "agent_message_chunk", content: { type: "text", text: eitriResult() } });
      log(`completion:${Date.now()}`); send({ id: promptRequestId, result: { stopReason: "end_turn" } });
    }, 250);
  } else if (message.method === "session/cancel") {
    log("cancel-received");
    if (promptRequestId !== null) send({ id: promptRequestId, result: { stopReason: "cancelled" } });
  }
});
