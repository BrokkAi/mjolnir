---
title: Other agents and models
description: Add specialist models, alternative agents, Anvil, or custom ACP servers after the Codex path works.
---

Codex is Mjolnir's recommended primary experience. You can keep Codex in every
seat or add another agent when a subtask, independent review, or provider
fallback benefits from it.

Under the hood, Mjolnir selects a model for the primary agent and a default model for
subagents, then chooses a launchable Agent Client Protocol adapter that can
provide each. An adapter must advertise ACP Streamable HTTP MCP support to enter
the roster at all: that is how Mjolnir attaches the `mj-subagents` server the
primary agent calls `create_subagent` through. An adapter that does not is
excluded with `ACP server does not advertise mcpCapabilities.http`, and with no
qualifying adapter no model is launchable.

## Available routes

| Route | Discovery | Launch notes |
| --- | --- | --- |
| Codex | Existing OpenAI/Codex credentials | Runs the Codex ACP bridge through `npx`; sign-in actions require the official `codex` CLI |
| Claude | Existing Anthropic/Claude credentials | Runs the Claude ACP bridge through `npx`; sign-in actions require the official `claude` CLI |
| Kimi Code | Existing Kimi credentials or `/mjconfig` sign-in | Mjolnir can install the official binary from the ACP registry |
| Anvil | Bundled sibling, development override, or managed copy | Mjolnir can install the release-specific managed runtime in the background |

Credential discovery checks supported local credential files and environment
variables without logging secret values or launching the npm bridges. First
launch can still require Node.js, npm, network access, and provider
authentication.

## Probing and caching

Native routes with fresh capability cache entries can bind immediately. Other
routes are probed in the background and appear in `/models` or the ACP Servers
tab when their catalog is ready. A wedged probe does not block an otherwise
launchable session.

Probe results and the live DeepSWE ranking are cached for 24 hours. A bundled
snapshot is available when the ranking endpoint cannot be refreshed. Read
[Storage and network activity](/storage-network/) for paths and endpoints.

`mj models refresh` clears the persisted ACP capability cache. In an active
session, `/models refresh` also clears Mjolnir's process-local probe results;
run `/new` or `/clear` afterward to resolve models and reprobe every enabled
adapter. The current session keeps its already-bound models.

## Auto selection and Codex priority

- The primary prefers the strongest launchable eligible row.
- The review supervisor prefers the strongest distinct model after the primary,
  first from another provider when available, then from the primary provider if
  needed.
- The default subagent model prefers a cost-efficient qualifying model on the
  current quality frontier, but can reuse the primary's model.
- When several adapters offer the selected model, the primary, review, and
  subagent seats apply their independent ACP priority lists. All lists default
  to Codex, Claude, Kimi, then Anvil.
- Unranked custom models are selectable explicitly but do not participate in
  Auto or Ragnarok.

Availability, credentials, cached capabilities, and the current ranking can
change the result. Auto chooses across launchable ranked models; adapter
priority decides between adapters that provide the selected model. Therefore,
adding another detected provider can change an unconstrained Auto-resolved seat
even though Codex is first in adapter priority. Set the seat's ACP source to
Codex in `/mjconfig` to retain Auto model selection within Codex, and use
`/agents` to record what actually launched.

## Custom ACP servers

```toml
version = 3

[[acp.servers]]
id = "custom:company"
label = "company"
command = "/opt/company/bin/acp-server"
args = ["--stdio"]
origin = "custom"

[acp.servers.env]
COMPANY_REGION = "dev"
```

Custom commands launch directly without a shell, inherit Mjolnir's environment,
and run in the active workspace directory. New custom routes follow the saved
seat priorities until the user reorders them. Use an absolute command path
where possible and avoid putting secret values directly in a committed config
file.

ACP servers are model agents. They are not the same as MCP servers: Mjolnir does
not expose a generic user-facing MCP-server list here. Its internal `mj-subagents`
MCP server exists only to give the primary agent authenticated access to
`create_subagent` and `subagent_cancel`.
