---
title: Overview
description: What Mjolnir owns, how the Council fits together, and when to use it.
---

Mjolnir (`mj`) is a native terminal client for Agent Client Protocol (ACP)
servers. It owns the interface and the coordination around agents while an ACP
adapter owns the provider-specific model session.

## The boundary

| Mjolnir owns | ACP adapters and provider agents own |
| --- | --- |
| Inline and fullscreen terminal UI | Provider authentication and model APIs |
| User input, session controls, and permission presentation | Provider-specific tools and session behavior |
| Council selection, delegation, and review timing | Model reasoning and generated content |
| Mjolnir-hosted filesystem, terminal, and Council MCP tools | Any adapter-hosted tools and their policies |
| Session provenance, worktrees, and remote-control state | Provider data retention and service terms |

This division keeps the terminal workflow stable when the selected model is
available through more than one adapter.

## Council architecture

```text
user
  │
  ▼
Thor ───── bounded implementation ────▶ Eitri
  │                                        │
  ├──── owns the user turn                 └──── result + diff return to Thor
  │
  └──── transcript checkpoints ───────▶ Loki
                                           └──── best-effort review advice
```

Thor always owns the foreground user turn. Eitri and Loki can be disabled.
Their exact models are selected independently from launchable routes.

## Good first uses

- Work in one repository from an inline terminal interface.
- Let a coordinator hand a bounded change to a fresh implementation context.
- Keep a second model reviewing work without blocking the active agent.
- Isolate a session in a linked Git worktree and resume it later.
- Run the same Council headlessly or through Mjolnir's remote viewer.

Mjolnir is not a model provider, a hosted agent service, or a guarantee that an
agent will make a correct change. Provider cost, capability, and data handling
still apply. Start with [Install and run](/install/), then use the checked
[10-minute evaluation](/evaluate/) in a disposable repository.

## Interfaces

| Surface | Start with | Best for |
| --- | --- | --- |
| Interactive terminal | `mj` | Daily coding, permissions, session controls |
| Isolated terminal | `mj --worktree` | Changes that should not touch the current checkout |
| Headless | `mj --print ...` | Scripts and machine-readable output |
| Resume | `mj resume` | Returning to an ACP session with saved route provenance |
| Remote viewer | `mj server` | Driving the same Council from another browser or device |

Continue with [Thor, Eitri, and Loki](/council/) for role semantics or [ACP
adapters and models](/adapters/) for discovery and selection.
