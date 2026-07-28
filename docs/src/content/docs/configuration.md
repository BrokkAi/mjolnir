---
title: Configuration
description: Configure the primary agent, subagents, ACP servers, review, and appearance.
---

Open `/mjconfig` to edit settings from the TUI. `/models` opens the same editor
on the Agents tab. Model and ACP-server changes apply to the next session.

The config schema is versioned. The current schema is `version = 3`; a
`version = 2` file is migrated in place on load. Any other version starts from
fresh defaults rather than guessing a field-by-field migration.

## Minimal config

```toml
version = 3

[agent]
model = "auto"
discrete_review = true

[subagents]
model = "auto"
max_parallel = 6
auto_failover = true
```

`[agent]` is the primary agent: the session that owns every user turn. It cannot
be disabled. `[subagents]` configures the default backing for `create_subagent`;
set `model = "disabled"` (or `"none"`) to turn subagents off entirely.

| Key | Meaning |
| --- | --- |
| `agent.model` | Primary model, or `auto` |
| `agent.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `agent.discrete_review` | Run the end-of-turn discrete review |
| `subagents.model` | Default subagent model, `auto`, or `disabled` |
| `subagents.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `subagents.max_parallel` | Concurrent subagents, default 6, maximum 16 |
| `subagents.auto_failover` | Move the default pool to another roster model when the current provider's quota runs low |

Explicit model IDs can come from `/models`; availability is checked when the
next session starts. A `max_parallel` above 16 is a configuration error, not a
silently clamped value.

## Migrating from version 2

A `version = 2` file (`[thor]`, `[eitri]`, `[loki]`, `[council]`) is mapped onto
the current schema the first time this build loads it, and the migrated result
is written back to the same path:

| v2 | v3 |
| --- | --- |
| `thor.model`, `thor.reasoning_effort`, `thor.discrete_review` | `agent.*` |
| `eitri.model`, `eitri.reasoning_effort` | `subagents.*` |
| `eitri.max_parallel_explores` | `subagents.max_parallel` |
| `council.auto_failover` | `subagents.auto_failover` |
| `[loki]`, `council.permission_mode` | dropped |

`theme`, `spinner`, `[acp]`, and `[ragnarok]` carry over unchanged. If the
migrated file cannot be written back, the session still runs on the migrated
values in memory.

## ACP policy

Built-in adapters can stay on Auto or be explicitly enabled or disabled. Custom
servers accept a command, arguments, environment values, origin, and policy.

```toml
[acp.policies]
codex-acp = "auto"
claude-acp = "disabled"

[[acp.servers]]
id = "custom:company"
label = "Company agent"
command = "/opt/company/bin/acp-server"
args = ["--stdio"]
origin = "custom"
policy = "enabled"
```

Custom commands inherit Mjolnir's environment and use the workspace as their
working directory. See [Data and trust boundaries](/data-boundaries/).

## One-shot overrides

Headless runs can override models without changing the saved file:

```bash
mj --print \
  --model provider/model-id \
  --subagent-model disabled \
  "summarize this repository"
```

Overrides require explicit model IDs; `auto` is not accepted. Each accepts an
optional `+<effort>` suffix (`--model provider/model-id+high`). The saved
configuration remains unchanged.

## Appearance and session controls

Theme and spinner preferences are persistent.
ACP option defaults are edited through the **Agents** and **Subagents** panels in
`/mjconfig`. Agent defaults are saved and also attempted on the active primary;
subagent defaults apply when new workers launch. Saved values that an adapter no
longer advertises are retained and labelled stale. When no live primary exists,
the editor uses its bounded per-role adapter/model metadata cache until the next
live primary or worker discovery refreshes it.

Platform config locations come from the operating system rather than a literal
cross-platform `~/.config` contract. See [Storage and network
activity](/storage-network/).
