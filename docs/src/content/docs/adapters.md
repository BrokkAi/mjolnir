---
title: ACP adapters and models
description: How Mjolnir discovers providers, probes ACP capabilities, and selects Council routes.
---

Mjolnir selects a model for each Council role, then chooses a launchable Agent
Client Protocol adapter that can provide it. A compatible Council adapter must
advertise ACP Streamable HTTP MCP support so Thor can call Mjolnir's Eitri
tools.

## Built-in routes

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
launchable Council.

Probe results and the live DeepSWE ranking are cached for 24 hours. A bundled
snapshot is available when the ranking endpoint cannot be refreshed. Read
[Storage and network activity](/storage-network/) for paths and endpoints.

## Auto selection

- Thor prefers the strongest launchable eligible row.
- Loki prefers a strong model from another provider, then another model, then
  reuses Thor when necessary.
- Eitri prefers a cost-efficient qualifying model on the current quality
  frontier, but can reuse another Council model.
- Unranked custom models are selectable explicitly but do not participate in
  Auto or Ragnarok.

Availability, credentials, cached capabilities, and the current ranking can
change the result. Use `/council` to record what actually launched.

## Custom ACP servers

```toml
version = 2

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
and run in the active workspace directory. Configuration order sets custom-route
precedence. Use an absolute command path where possible and avoid putting secret
values directly in a committed config file.

ACP servers are model agents. They are not the same as MCP servers: Mjolnir does
not expose a generic user-facing MCP-server list here. Its internal MCP server
exists to give Thor authenticated access to Eitri orchestration tools.
