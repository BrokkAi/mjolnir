---
title: Other agents and models
description: How Mjolnir resolves models and adapters for each seat.
---

Mjolnir ships two first-class routes, Codex and Claude, and a
[team](/teams/) decides which one fills each seat. This page covers what
happens underneath — discovery, probing, and model resolution.

Under the hood, Mjolnir selects a model for the primary agent and a default model for
subagents, then chooses a launchable Agent Client Protocol adapter that can
provide each. Mjolnir attaches its `mj-subagents` server — the one the primary
agent calls `create_subagent` through — as a stdio MCP server, the baseline
transport every ACP adapter supports.

## Available routes

| Route | Discovery | Launch notes |
| --- | --- | --- |
| Codex | Existing OpenAI/Codex credentials | Runs the bridge and its bundled compatible Codex CLI through `npx` |
| Claude | Existing Anthropic/Claude credentials | Runs the bridge and its bundled compatible Claude Code executable through `npx` |

Credential discovery checks supported local credential files and environment
variables without logging secret values. Roster resolution launches every
selected adapter, including the npm bridges, before the roster is used. First
launch can require Node.js, npm, network access, and provider authentication.

## Probing

Selected routes are probed concurrently, and roster resolution waits for all
of them before returning. Each probe opens an ACP connection and creates a
disposable session to collect models and session options. Mjolnir does not
persist or reuse ACP capability results between resolutions.

The live DeepSWE ranking is separate from adapter capabilities and remains
cached for 24 hours. A bundled snapshot is available when the ranking endpoint
cannot be refreshed. Read [Storage and network activity](/storage-network/)
for paths and endpoints.

`mj models refresh` performs an immediate roster resolution, probes every
enabled adapter, and reports the available model count. Normal startup and
`/new` or `/clear` resolutions perform the same adapter probes automatically.

## Team selection and automatic models

Onboarding, `/mjconfig`, and **Shift+Tab** expose four first-class teams: Codex,
Claude, Codex coder + Claude reviewer, and Claude coder + Codex reviewer. A
team pins the primary route to its coder and the review and subagent routes to
its reviewer while leaving model selection on Auto.

- The primary prefers the strongest launchable eligible row.
- The review supervisor and default subagent model exclude the primary first,
  then prefer the cheapest cost-efficient model on the current quality frontier
  that meets the Sonnet quality floor. If no distinct model clears that floor,
  they choose the strongest distinct frontier model that costs less than the
  primary; otherwise, they reuse the primary.
- When several adapters offer the selected model, the primary, review, and
  subagent seats apply their independent ACP priority lists. All lists default
  to Codex, then Claude.
- Adapter-advertised models without a leaderboard row (for example Claude's
  `haiku`) are selectable explicitly but do not participate in Auto.

Availability, credentials, advertised capabilities, and the current ranking
can change the result. Auto chooses across launchable ranked models; adapter
priority decides between adapters that provide the selected model. Therefore,
adding another detected provider can change an unconstrained Auto-resolved
seat even though Codex is first in adapter priority. Choose a Team preset to
retain Auto model selection within its assigned provider, and use `/agents` to
record what actually launched.

## Codex and Claude only

The ACP Servers panel intentionally contains only Codex and Claude; Mjolnir
does not support user-configured ACP servers. Legacy `[[acp.servers]]`
sections in `config.toml` are ignored on load and dropped on the next save.

ACP servers are model agents. They are not the same as MCP servers: Mjolnir does
not expose a generic user-facing MCP-server list here. Its internal `mj-subagents`
MCP server exists only to give the primary agent authenticated access to
`create_subagent` and `subagent_cancel`.
