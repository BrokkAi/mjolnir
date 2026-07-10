// Minimal MCP client over a spawned stdio server command. The fixtures use it
// the way a real ACP adapter would: spawn the advertised `mj mcp-proxy`
// command with the advertised env and speak line-delimited JSON-RPC on its
// stdio.
import { spawn } from "node:child_process";
import readline from "node:readline";

export function spawnMcpServer(server, { envOverrides = {} } = {}) {
  const env = { ...process.env };
  for (const variable of server.env ?? []) env[variable.name] = variable.value;
  Object.assign(env, envOverrides);
  const child = spawn(server.command, server.args ?? [], { env, stdio: ["pipe", "pipe", "inherit"] });
  const pending = new Map();
  const exit = new Promise((resolve) => {
    child.on("exit", (code, signal) => {
      for (const [, entry] of pending) entry.reject(new Error(`mcp server exited: code=${code} signal=${signal}`));
      pending.clear();
      resolve({ code, signal });
    });
  });
  const lines = readline.createInterface({ input: child.stdout });
  lines.on("line", (line) => {
    let message;
    try { message = JSON.parse(line); } catch { return; }
    const entry = message.id !== undefined ? pending.get(message.id) : undefined;
    if (entry) { pending.delete(message.id); entry.resolve(message); }
  });
  function request(body, timeoutMs = 30000) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { pending.delete(body.id); reject(new Error(`mcp request ${body.id} timed out`)); }, timeoutMs);
      pending.set(body.id, {
        resolve: (message) => { clearTimeout(timer); resolve(message); },
        reject: (error) => { clearTimeout(timer); reject(error); },
      });
      child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", ...body })}\n`);
    });
  }
  function notify(body) { child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", ...body })}\n`); }
  function kill() { try { child.kill(); } catch {} }
  return { child, request, notify, kill, exit };
}

function initializeBody(id) {
  return { id, method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "e2e-fixture", version: "1" } } };
}

export async function initializeMcp(client) {
  const initialized = await client.request(initializeBody("initialize"));
  if (!initialized.result) throw new Error(`MCP initialize failed: ${JSON.stringify(initialized)}`);
  client.notify({ method: "notifications/initialized", params: {} });
  return initialized.result;
}

// The proxy must fail without ever serving tools when its bearer token is
// wrong — the stdio equivalent of the old loopback 401 check.
export async function verifyWrongTokenIsRejected(server) {
  const bad = spawnMcpServer(server, { envOverrides: { MJ_MCP_PROXY_TOKEN: "wrong-token" } });
  const outcome = await Promise.race([
    bad.exit.then(() => "rejected"),
    bad.request(initializeBody("unauthorized")).then(
      (message) => (message.error ? "rejected" : "served"),
      () => "rejected",
    ),
  ]);
  bad.kill();
  return outcome === "rejected";
}
