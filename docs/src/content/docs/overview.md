---
title: Architecture and boundaries
description: What Mjolnir owns around Codex and Claude, how subagents fit, and where provider boundaries remain.
---

Mjolnir (`mj`) is a native, self-hosted interface and control plane for Codex
and Claude. It owns the terminal, remote server, local session state, and
coordination around the agent while its ACP bridge owns the provider-specific
model session.

Choose Codex or Claude as both coder and reviewer, or split those roles between
them. The same architecture can host other Agent Client Protocol (ACP) servers
as advanced routes for specialists, comparison, or replacement.

## The boundary

| Mjolnir owns | ACP adapters and provider agents own |
| --- | --- |
| Inline and fullscreen terminal UI | Provider authentication and model APIs |
| User input, session controls, and permission presentation | Provider-specific tools and session behavior |
| Model selection, subagent lifecycle, and review timing | Model reasoning and generated content |
| Mjolnir-hosted filesystem, terminal, and subagent MCP tools | Any adapter-hosted tools and their policies |
| Session provenance, worktrees, and remote-control state | Provider data retention and service terms |

This division keeps the terminal workflow stable when the selected model is
available through more than one adapter.

## Architecture

```text
user
  │
  ▼
primary agent ──── create_subagent ────▶ subagent #1  (fresh session, writes)
  │                                 └──▶ subagent #2  (fresh session, writes)
  │                                            │
  └──── owns every user turn                   └──── report injected back as a
                                                     user message when it finishes
```

The primary agent owns every user turn and cannot be disabled. Subagents are
launched on demand, run in the background, and push their reports back into the
primary session. The primary model and the default subagent model are selected
independently from launchable routes; subagents can be turned off entirely.

## Good first uses

- Work in one repository from an inline terminal interface.
- Let the primary agent hand bounded work to several fresh contexts at once.
- Isolate a session in a linked Git worktree and resume it later.
- Run the same setup headlessly or through Mjolnir's remote viewer.

Mjolnir is not a model provider, a hosted model service, or a guarantee that an
agent will make a correct change. Its remote-control plane is self-hosted;
provider requests still use OpenAI or Anthropic. Provider cost, capability,
and data handling still apply. Start with [Install and run](/install/), then
use the checked
[10-minute Codex evaluation](/evaluate/) in a disposable repository.

## Interfaces

| Surface | Start with | Best for |
| --- | --- | --- |
| Interactive terminal | `mj` | Daily coding, permissions, session controls |
| Isolated terminal | `mj --worktree` | Changes that should not touch the current checkout |
| Headless | `mj --print ...` | Scripts and machine-readable output |
| Resume | `mj resume` | Returning to an ACP session with saved route provenance |
| Remote viewer | `mj server` | Driving the same session from another browser or device |

Continue with [Subagents](/subagents/) for delegation semantics or [Other
agents and models](/adapters/) for advanced discovery and selection.
