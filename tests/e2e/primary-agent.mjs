#!/usr/bin/env node

import fs from "node:fs";
import readline from "node:readline";
import { initializeMcp, spawnMcpServer, verifyWrongTokenIsRejected } from "./mcp-stdio-client.mjs";

const resultPath = process.env.MJ_E2E_PRIMARY_RESULT;
const logPath = process.env.MJ_E2E_PRIMARY_LOG;
const instructions = process.env.MJ_E2E_CODE_AGENT_INSTRUCTIONS ?? "Return CODEAGENT_E2E_OK";
if (process.env.MJ_E2E_PRIMARY_PID) fs.writeFileSync(process.env.MJ_E2E_PRIMARY_PID, String(process.pid));
let promptRequestId = null;
let mcpServer = null;
let mcp = null;
let mcpReady = null;
let directiveCount = 0;
process.on("exit", () => mcp?.kill());

function send(message) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
}

function appendLog(value) {
  if (logPath) fs.appendFileSync(logPath, `${value}\n`);
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
  const unauthorizedRejected = await verifyWrongTokenIsRejected(mcpServer);
  if (!unauthorizedRejected) throw new Error("proxy served tools despite a wrong bearer token");

  mcp = spawnMcpServer(mcpServer);
  await initializeMcp(mcp);

  const listed = await mcp.request({ id: "tools-list", method: "tools/list", params: {} });
  const tools = listed.result?.tools ?? [];
  const tool = tools.find((candidate) => candidate.name === "code_agent");
  if (!tool
      || !tool.description?.includes("IMPLEMENTATION DELEGATE")
      || !tool.description?.includes("small local changes")
      || !tool.description?.includes("fresh ACP process/session")) {
    throw new Error(`code_agent tool missing or weakly described: ${JSON.stringify(tools)}`);
  }
  return { unauthorizedRejected };
}

async function callCodeAgent() {
  const { unauthorizedRejected } = await mcpReady;
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
  const called = await mcp.request(
    {
      id: "code-agent-call",
      method: "tools/call",
      params: { name: "code_agent", arguments: { instructions } },
    },
    600000,
  );
  const toolReceivedAt = Date.now();
  appendLog(`tool-call-finish:${toolReceivedAt}`);
  if (!called.result) {
    throw new Error(`MCP tool call failed: ${JSON.stringify(called)}`);
  }
  const result = called.result;
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
    unauthorizedRejected,
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
          // No optional MCP transports: the council must reach its servers
          // through the mandatory stdio transport alone.
          mcpCapabilities: {
            http: false,
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
    mcpServer = servers.find((server) => server.name === "mj-code-agent" && !server.type);
    if (!mcpServer || typeof mcpServer.command !== "string" || mcpServer.args?.[0] !== "mcp-proxy") {
      send({ id: message.id, error: { code: -32602, message: "missing stdio code-agent MCP proxy command" } });
      return;
    }
    if (!(mcpServer.env ?? []).some((variable) => variable.name === "MJ_MCP_PROXY_TOKEN" && variable.value)) {
      send({ id: message.id, error: { code: -32602, message: "missing code-agent proxy bearer token env" } });
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
      send({
        method: "session/update",
        params: {
          sessionId: "primary-session",
          update: {
            sessionUpdate: "agent_message_chunk",
            content: { type: "text", text: "MJ_CODE_AGENT_POLICY_READY" },
          },
        },
      });
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
        setTimeout(() => send({ id: message.id, result: { stopReason: "end_turn" } }), 50);
      } else {
        send({ id: message.id, result: { stopReason: "end_turn" } });
      }
      return;
    }
    if (directiveCount !== 2 || prompt.length !== 1 || prompt[0]?.text !== "write a hello world program in Python") {
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
