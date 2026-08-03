---
title: Configuration
description: Configure the primary agent, subagents, ACP servers, review, and appearance.
---

Open `/mjconfig` to edit settings from the TUI. `/models` opens the same editor
on the Agents tab. Model and ACP-server changes apply to the next session.
Use `/models refresh` when credentials or adapter capabilities changed and you
need the next `/new` or `/clear` to probe every enabled adapter again. The
equivalent non-interactive command is `mj models refresh`.

The config schema is versioned. The current schema is `version = 3`; a
`version = 2` file is migrated in place on load. Any other version starts from
fresh defaults rather than guessing a field-by-field migration.

The guided product explanation has its own `onboarding_version`, separate from
the config schema. This lets a major workflow change explain what is new without
forcing an existing user through provider setup or treating education as a
storage migration.

## Minimal config

```toml
version = 3

[agent]
model = "auto"
discrete_review = true

[review]
model = "auto"

[subagents]
model = "auto"
max_parallel = 6
auto_failover = true
```

`[agent]` is the primary agent: the session that owns every user turn. It cannot
be disabled. `[review]` configures the discrete-review supervisor model; review
is still enabled or disabled with `agent.discrete_review`. `[subagents]`
configures the default backing for `create_subagent`; set `model = "disabled"`
(or `"none"`) to turn subagents off entirely.

| Key | Meaning |
| --- | --- |
| `agent.model` | Primary model, or `auto` |
| `agent.acp_priority` | ACP source preference when several enabled adapters offer the primary model |
| `agent.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `agent.discrete_review` | Run the end-of-turn discrete review |
| `review.model` | Review supervisor model, or `auto` |
| `review.acp_priority` | ACP source preference for the review supervisor model |
| `review.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `subagents.model` | Default subagent model, `auto`, or `disabled` |
| `subagents.acp_priority` | Independent ACP source preference for the default worker model |
| `subagents.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `subagents.max_parallel` | Concurrent subagents, default 6, maximum 16 |
| `subagents.auto_failover` | Move the default pool to the next roster route when the current ACP source's quota runs low; the model may stay the same |
| `subagents.progress_wake_minutes` | Minutes a primary parked on running subagents may go without a report before it is woken with their progress alone; default 20, `0` disables. Config file only |

Explicit model IDs can come from `/models`; availability is checked when the
next session starts. A `max_parallel` above 16 is a configuration error, not a
silently clamped value.

ACP priority lists default to `codex-acp`, `claude-acp`, `kimi`, then `anvil`,
preserving the automatic behavior of earlier configurations. Reorder or reset
them independently from the ACP Priority tab, or configure stable source IDs
directly:

```toml
[agent]
acp_priority = ["codex-acp", "claude-acp", "anvil", "kimi"]

[review]
acp_priority = ["claude-acp", "codex-acp", "anvil", "kimi"]

[subagents]
acp_priority = ["anvil", "codex-acp", "claude-acp", "kimi"]
```

The ACP Servers tab still controls eligibility. Priority only decides which
enabled adapter supplies a selected model when more than one advertises it.
Sources absent from a saved list are appended in discovery order, so installing
a new adapter does not unexpectedly move it ahead of an explicit preference.

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
  --review-model provider/review-model-id \
  --subagent-model disabled \
  "summarize this repository"
```

Overrides require explicit model IDs; `auto` is not accepted. Each accepts an
optional `+<effort>` suffix (`--model provider/model-id+high`). The saved
configuration remains unchanged.

## Appearance and session controls

Theme and spinner preferences are persistent. Agent-owned ACP session defaults
are listed per configured server on the **ACP Sessions** tab in `/mjconfig`.
Model and thought-level selection remain in Mjolnir's **Agents** configuration.
Saved ACP session defaults take effect when that server starts a new session.

Platform config locations come from the operating system rather than a literal
cross-platform `~/.config` contract. See [Storage and network
activity](/storage-network/).
