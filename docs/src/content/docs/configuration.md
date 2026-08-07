---
title: Configuration
description: Choose a Codex and Claude team, then configure its models, ACP servers, review, and appearance.
---

Open `/mjconfig` to edit settings from the TUI. The **Team** tab chooses who
codes and who reviews; model and ACP-server changes are available in the other
tabs. Team and adapter changes apply to a new session. Credentials and adapter
capabilities are probed whenever a new session roster is resolved;
`mj models refresh` runs that probe as a standalone diagnostic.

The config schema is versioned. The current schema is `version = 3`; a
`version = 2` file is migrated in place on load. Any other version starts from
fresh defaults rather than guessing a field-by-field migration.

The guided product explanation has one monotonic `onboarding_version`, separate
from the config schema. Mjolnir compares it only with the latest onboarding:
someone several versions behind sees the current flow once, never a replay of
every missed flow. Finishing or explicitly skipping records the latest version;
canceling fresh setup leaves onboarding incomplete.

## Minimal config

What the **Codex coder + Claude reviewer** team writes:

```toml
version = 3

[agent]
model = "auto"
acp_source = "codex-acp"
discrete_review = true

[review]
model = "auto"
acp_source = "claude-acp"

[subagents]
model = "auto"
acp_source = "claude-acp"
max_parallel = 6
auto_failover = true
```

`[agent]` is the primary agent: the session that owns every user turn. It cannot
be disabled. `[review]` configures the discrete-review model; review
is still enabled or disabled with `agent.discrete_review`, and its depth is
chosen with `agent.review_tier`. `[subagents]`
configures the default backing for `create_subagent`; set `model = "disabled"`
(or `"none"`) to turn subagents off entirely.

| Key | Meaning |
| --- | --- |
| `agent.model` | Primary model, or `auto` |
| `agent.acp_source` | Optional exact ACP source constraint; keeps `auto` selection inside that adapter |
| `agent.acp_priority` | ACP source preference when several enabled adapters offer the primary model |
| `agent.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `agent.session_defaults` | Per-ACP saved session-option defaults for new primary sessions |
| `agent.discrete_review` | Run the end-of-turn discrete review |
| `agent.review_tier` | Review depth: `quick` (default) sends one general reviewer and validates its findings; `extended` runs the adversarial supervisor with on-demand Norse specialist lanes and spends far more tokens |
| `review.model` | Review supervisor model, or `auto` |
| `review.acp_source` | Optional exact ACP source constraint for the review seat |
| `review.acp_priority` | ACP source preference for the review supervisor model |
| `review.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `review.session_defaults` | Per-ACP saved session-option defaults for new review sessions |
| `subagents.model` | Default subagent model, `auto`, or `disabled` |
| `subagents.acp_source` | Optional exact ACP source constraint for default workers and their failover pool |
| `subagents.acp_priority` | Independent ACP source preference for the default worker model |
| `subagents.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `subagents.session_defaults` | Per-ACP saved session-option defaults for newly created subagents |
| `subagents.max_parallel` | Concurrent subagents, default 6, maximum 16 |
| `subagents.auto_failover` | Move the default pool to the next roster route when the current ACP source's quota runs low; the model may stay the same |
| `subagents.progress_wake_minutes` | Minutes a primary parked on running subagents may go without a report before it is woken with their progress alone; default 20, `0` disables. Config file only |

Explicit model IDs can be selected in `/mjconfig`; availability is checked
when the next session starts. A `max_parallel` above 16 is a configuration
error, not a silently clamped value.

Onboarding, the **Team** tab in `/mjconfig`, and **Ctrl+Tab** during a session
all offer the same four configurations:

| Team | Primary (coder) | Subagents and review (reviewer) |
| --- | --- | --- |
| **Codex** | Codex | Codex |
| **Claude** | Claude | Claude |
| **Codex coder + Claude reviewer** | Codex | Claude |
| **Claude coder + Codex reviewer** | Claude | Codex |

Choosing a team keeps all three model selections on Auto, pins the primary
seat to the coder, pins the subagent and review seats to the reviewer, enables
discrete review and subagent failover, and enables the required built-in ACP
routes. After saving from **Ctrl+Tab**, start the offered new session to use the
new team immediately. See [Teams and adversarial review](/teams/).

ACP priority lists default to `codex-acp`, then `claude-acp`,
preserving the automatic behavior of earlier configurations. When a source is
not constrained, advanced deployments can configure stable source IDs directly:

```toml
[agent]
acp_priority = ["codex-acp", "claude-acp"]

[review]
acp_priority = ["claude-acp", "codex-acp"]

[subagents]
acp_priority = ["codex-acp", "claude-acp"]
```

The ACP Servers tab controls eligibility. Priority only decides which enabled
adapter supplies a selected model when more than one advertises it.
Sources absent from a saved list are appended in discovery order.

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

The ACP Servers tab exposes the built-in Codex and Claude adapters, which
can stay on Auto or be explicitly enabled or disabled.

```toml
[acp.policies]
codex-acp = "auto"
claude-acp = "disabled"
```

Adapters inherit Mjolnir's environment and use the workspace as their
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

## Memories

Mjolnir keeps short, durable facts (at most 2,000 bytes each) across sessions
in `memories.json`, next to the config. Memory integration applies only to
**Codex primary sessions**: Claude Code and custom adapters keep their own
native memory systems, so mjolnir neither injects memories nor exposes the
save tools there. The store and its management commands work regardless of
the active adapter.

Memories are global or scoped to the enclosing project (worktree sessions
share the parent project's memories), and are created explicitly: ask the
agent to remember something (it calls the `memory_save` tool exposed by the
in-process `mj-memory` MCP server), or run `/memory add` in the TUI or
`mj memory add` from the CLI. The relevant entries are injected at the start
of the next Codex primary session in a `<mj-memory>` block, size-bounded with
the oldest entries dropped first. Side conversations, subagents, and review
lanes never see or write memories.

The feature is optional. A master switch plus two toggles control it, all on
by default:

```toml
[memory]
enabled = true           # master switch; false disables the feature entirely
use_memories = true      # inject stored memories into new primary sessions
generate_memories = true # expose the memory_save / memory_forget tools
```

Set `enabled = false` (or run `/memory off` in the TUI) to switch memory off
entirely — no injection and no tools, regardless of the other toggles. The
store and the management commands below keep working while disabled, and
`/memory` and `mj memory list` call out the disabled state. Toggle the
sub-switches with `/memory use on|off` and `/memory generate on|off`; all
changes apply to sessions started afterwards. `/memory` lists the stored
entries, `/memory forget <id>` deletes one, and `/memory clear confirm` (or
`mj memory clear --yes`) deletes everything.

## Appearance and session controls

Theme, spinner, thought-output, and feature-tip preferences are persistent.
Thought output defaults to **Default**, which summarizes completed thoughts and
shows a bounded tail while a thought is streaming. Choose **Full** under
**Appearance** in the TUI or web `/mjconfig`, or set `thought_output = "full"`
at the top level of the config file, to show all available thought text in both
transcripts. Feature tips are enabled by default and appear occasionally
between completed turns; disable them under **Appearance** or set
`feature_hints = false` in the top level of the config file.

The **Agent**, **Reviewer**, and **Subagents** tabs list the selectable session
options advertised by that role's selected ACP source. Each role stores its
defaults separately. Compatible primary changes are also sent to the running
primary session when `/mjconfig` is saved; the UI calls out the active value
when it differs from the selected default. Team, reviewer, and subagent changes
apply only to sessions started later, never to ones that are already running. A
saved value that a newly selected adapter no longer advertises stays intact and
is shown as unavailable until you select a compatible value.

The same role-scoped defaults can be written directly in TOML:

```toml
[agent.session_defaults."codex-acp"]
"config:service_tier" = "priority"

[review.session_defaults."codex-acp"]
"config:service_tier" = "flex"

[subagents.session_defaults."codex-acp"]
"config:service_tier" = "default"
```

Platform config locations come from the operating system rather than a literal
cross-platform `~/.config` contract. See [Storage and network
activity](/storage-network/).
