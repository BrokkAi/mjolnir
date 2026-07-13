# Mjolnir Code-Agent MCP Tool

Interactive Mjolnir sessions automatically start an authenticated Streamable
HTTP MCP server on a random loopback port and include it in the primary ACP
session's `mcpServers`. The server exposes one model-visible tool:

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

Primary ACP adapters must advertise `mcpCapabilities.http`; Mjolnir fails
clearly before opening a session when they do not.

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
