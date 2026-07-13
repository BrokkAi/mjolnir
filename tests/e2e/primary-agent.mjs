#!/usr/bin/env node

import fs from "node:fs";
import readline from "node:readline";

const resultPath = process.env.MJ_E2E_PRIMARY_RESULT;
const logPath = process.env.MJ_E2E_PRIMARY_LOG;
const instructions = process.env.MJ_E2E_CODE_AGENT_INSTRUCTIONS ?? "Return CODEAGENT_E2E_OK";
if (process.env.MJ_E2E_PRIMARY_PID) fs.writeFileSync(process.env.MJ_E2E_PRIMARY_PID, String(process.pid));
let promptRequestId = null;
let mcpServer = null;
let mcpSessionId = null;
let mcpReady = null;
let directiveCount = 0;

function send(message) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
}

function appendLog(value) {
  if (logPath) fs.appendFileSync(logPath, `${value}\n`);
}

function mcpHeaders(includeAuth = true, sessionId = null) {
  const headers = {
    "content-type": "application/json",
    accept: "application/json, text/event-stream",
  };
  if (includeAuth) {
    for (const header of mcpServer.headers ?? []) headers[header.name] = header.value;
  }
  if (sessionId) headers["mcp-session-id"] = sessionId;
  return headers;
}

function parseMcpResponse(text) {
  const trimmed = text.trim();
  if (!trimmed) return null;
  if (trimmed.startsWith("{")) return JSON.parse(trimmed);
  const messages = trimmed
    .split(/\r?\n/)
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trim())
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  return messages.at(-1) ?? null;
}

async function postMcp(body, { includeAuth = true, sessionId = null } = {}) {
  const response = await fetch(mcpServer.url, {
    method: "POST",
    headers: mcpHeaders(includeAuth, sessionId),
    body: JSON.stringify(body),
  });
  const text = await response.text();
  return {
    status: response.status,
    sessionId: response.headers.get("mcp-session-id") ?? sessionId,
    message: parseMcpResponse(text),
  };
}

function writeResult(value) {
  if (resultPath) fs.writeFileSync(resultPath, JSON.stringify(value));
}

function finishPrimary(text) {
  send({
    method: "session/update",
    params: {
      sessionId: "primary-session",
      update: {
        sessionUpdate: "agent_message_chunk",
        content: { type: "text", text },
      },
    },
  });
  send({ id: promptRequestId, result: { stopReason: "end_turn" } });
  setTimeout(() => process.exit(0), 500);
}

function updatePrimaryTool(update) {
  send({
    method: "session/update",
    params: {
      sessionId: "primary-session",
      update,
    },
  });
}

async function prepareMcp() {
  const unauthorized = await postMcp(
    {
      jsonrpc: "2.0",
      id: "unauthorized",
      method: "initialize",
      params: {
        protocolVersion: "2025-06-18",
        capabilities: {},
        clientInfo: { name: "e2e-primary", version: "1" },
      },
    },
    { includeAuth: false },
  );
  if (unauthorized.status !== 401) throw new Error(`unauthenticated MCP returned ${unauthorized.status}`);

  const initialized = await postMcp({
    jsonrpc: "2.0",
    id: "initialize",
    method: "initialize",
    params: {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "e2e-primary", version: "1" },
    },
  });
  if (initialized.status !== 200 || !initialized.sessionId || !initialized.message?.result) {
    throw new Error(`MCP initialize failed: ${JSON.stringify(initialized)}`);
  }
  mcpSessionId = initialized.sessionId;
  await postMcp(
    { jsonrpc: "2.0", method: "notifications/initialized", params: {} },
    { sessionId: mcpSessionId },
  );

  const listed = await postMcp(
    { jsonrpc: "2.0", id: "tools-list", method: "tools/list", params: {} },
    { sessionId: mcpSessionId },
  );
  const tools = listed.message?.result?.tools ?? [];
  const tool = tools.find((candidate) => candidate.name === "code_agent");
  const explore = tools.find((candidate) => candidate.name === "explore_agent");
  if (!tool
      || !tool.description?.includes("IMPLEMENTATION DELEGATE")
      || !tool.description?.includes("small local changes")
      || !tool.description?.includes("fresh ACP process/session")) {
    throw new Error(`code_agent tool missing or weakly described: ${JSON.stringify(tools)}`);
  }
  if (!explore
      || !explore.description?.includes("READ-ONLY EXPLORATION DELEGATE")
      || !explore.description?.includes("known path")
      || !explore.description?.includes("quick, medium, or very thorough")) {
    throw new Error(`explore_agent tool missing or weakly described: ${JSON.stringify(tools)}`);
  }
  return { unauthorizedStatus: unauthorized.status };
}

async function callCodeAgent() {
  const { unauthorizedStatus } = await mcpReady;
  const toolSentAt = Date.now();
  appendLog(`tool-call-start:${toolSentAt}`);
  updatePrimaryTool({
    sessionUpdate: "tool_call",
    toolCallId: "primary-code-agent-call",
    title: "mcp.mj-code-agent.code_agent",
    kind: "execute",
    status: "in_progress",
    rawInput: {
      server: "mj-code-agent",
      tool: "code_agent",
      arguments: { instructions },
    },
  });
  const called = await postMcp(
    {
      jsonrpc: "2.0",
      id: "code-agent-call",
      method: "tools/call",
      params: { name: "code_agent", arguments: { instructions } },
    },
    { sessionId: mcpSessionId },
  );
  const toolReceivedAt = Date.now();
  appendLog(`tool-call-finish:${toolReceivedAt}`);
  if (called.status !== 200 || !called.message?.result) {
    throw new Error(`MCP tool call failed: ${JSON.stringify(called)}`);
  }
  const result = called.message.result;
  updatePrimaryTool({
    sessionUpdate: "tool_call_update",
    toolCallId: "primary-code-agent-call",
    status: result.isError ? "failed" : "completed",
    rawOutput: result,
  });
  writeResult({
    response: result,
    toolSentAt,
    toolReceivedAt,
    unauthorizedStatus,
  });
  const text = result.content?.map((content) => content.text ?? "").join("") ?? "";
  if (result.isError) finishPrimary(`PRIMARY CANCELLED: ${text || "error"}`);
  else finishPrimary(`PRIMARY RECEIVED: ${text}`);
}

const input = readline.createInterface({ input: process.stdin });
input.on("close", () => process.exit(0));
input.on("line", (line) => {
  appendLog(line);
  const message = JSON.parse(line);
  if (message.method === "initialize") {
    if (message.params?.clientCapabilities?._meta?.mj?.codeAgent) {
      send({ id: message.id, error: { code: -32602, message: "legacy codeAgent capability still advertised" } });
      return;
    }
    send({
      id: message.id,
      result: {
        protocolVersion: 1,
        agentCapabilities: {
          mcpCapabilities: {
            http: process.env.MJ_E2E_HTTP_UNSUPPORTED !== "1",
            sse: false,
          },
        },
        agentInfo: { name: "e2e-primary", version: "1" },
      },
    });
    return;
  }
  if (message.method === "session/new") {
    const servers = message.params?.mcpServers ?? [];
    mcpServer = servers.find((server) => server.name === "mj-code-agent" && server.type === "http");
    if (!mcpServer || !mcpServer.url?.startsWith("http://127.0.0.1:")) {
      send({ id: message.id, error: { code: -32602, message: "missing loopback HTTP code-agent MCP server" } });
      return;
    }
    if (!(mcpServer.headers ?? []).some((header) => header.name.toLowerCase() === "authorization" && header.value.startsWith("Bearer "))) {
      send({ id: message.id, error: { code: -32602, message: "missing code-agent bearer header" } });
      return;
    }
    send({ id: message.id, result: { sessionId: "primary-session" } });
    mcpReady = prepareMcp();
    return;
  }
  if (message.method === "session/prompt") {
    promptRequestId = message.id;
    const prompt = message.params?.prompt ?? [];
    if (prompt.length === 1 && prompt[0]?.text?.includes("<mj-code-agent-policy>")) {
      directiveCount += 1;
      appendLog(`session-directive:${directiveCount}`);
      if (!prompt[0].text.startsWith("write a hello world program in Python\n\n<mj-code-agent-policy>")) {
        writeResult({ error: `policy was not appended to first user prompt: ${JSON.stringify(prompt)}` });
        finishPrimary("PRIMARY FAILED: misplaced session coordinator directive");
        return;
      }
      if (directiveCount === 1) {
        send({
          method: "session/update",
          params: {
            sessionId: "primary-session",
            update: { sessionUpdate: "usage_update", used: 12000, size: 128000 },
          },
        });
        send({
          method: "session/update",
          params: {
            sessionId: "primary-session",
            update: { sessionUpdate: "usage_update", used: 2000, size: 128000 },
          },
        });
      }
    }
    if (directiveCount !== 1 || prompt.length !== 1 || !prompt[0]?.text?.startsWith("write a hello world program in Python\n\n<mj-code-agent-policy>")) {
      writeResult({ error: `missing session coordinator directive: ${JSON.stringify(prompt)}` });
      finishPrimary("PRIMARY FAILED: missing session coordinator directive");
      return;
    }
    void callCodeAgent().catch((error) => {
      writeResult({ error: String(error?.stack ?? error) });
      finishPrimary(`PRIMARY FAILED: ${error.message}`);
    });
  }
});
