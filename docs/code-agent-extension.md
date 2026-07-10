# Mjolnir Code-Agent MCP Tool

Interactive Mjolnir sessions serve the tool from an in-process, bearer-token
authenticated loopback HTTP endpoint, but advertise it in the primary ACP
session's `mcpServers` as a **stdio** server — the one MCP transport every ACP
agent must support. The advertised command is `mj mcp-proxy --url <loopback>`,
a thin bridge the adapter spawns; the bearer token travels in the
`MJ_MCP_PROXY_TOKEN` environment variable, never on argv. The server exposes
one model-visible tool:

```json
{
  "name": "code_agent",
  "inputSchema": {
    "type": "object",
    "properties": { "instructions": { "type": "string" } },
    "required": ["instructions"]
  }
}
```

No user configuration, environment variables, or explicit mention of the tool
is required. After the primary loads the MCP tool, Mjolnir sends one hidden
session directive telling it to delegate requests that create, modify, debug,
refactor, or test code. The directive is not prepended to each user prompt. ACP
does not define a compaction event, so Mjolnir repeats the directive before the
next user turn whenever `usage_update.used` drops, indicating that the primary
replaced its context with a compacted history. The same bootstrap is installed
when a session is resumed, loaded, or forked.

Because the advertisement is a stdio command, no optional `mcpCapabilities`
(http/sse) are required of the primary adapter. Loki's `advise` tool uses the
same loopback-plus-proxy mechanism.

When called, Mjolnir starts `npx -y @agentclientprotocol/codex-acp`, opens a
fresh ACP session in the primary session's workspace, streams the nested turn
in the TUI, and keeps the MCP tool call pending. The successful MCP result
contains only Codex's final text message, after which the primary agent resumes
its turn.

Only one nested run is allowed. Invalid parameters are rejected, while busy,
nested-runtime, cancellation, and message-less failures return MCP tool errors.
While the nested turn is active, Ctrl-C cancels it rather than the primary turn.
The nested runtime is not given this MCP server, so it cannot recursively
delegate.

The first version is interactive-only and hard-codes Codex as the nested ACP
agent. Headless, MCP, remote-server, Ragnarok, and other auxiliary runtimes do
not inject the tool.

## End-to-end checks

After building `mj`, run the deterministic two-process PTY harness:

```sh
tests/e2e/deterministic.sh
```

The opt-in live smoke uses the installed Codex credentials and makes one real
model request in a temporary repository:

```sh
tests/e2e/live-codex.sh
```
