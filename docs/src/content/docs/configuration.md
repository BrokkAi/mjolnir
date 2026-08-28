---
title: Configuration
description: Choose a Codex and Claude team, then configure its models, ACP servers, review, and appearance.
---

Open `/mjconfig` to edit settings from the TUI. The **Team** tab chooses who
codes and who reviews; model and ACP-server changes are available in the other
tabs. Team and adapter changes apply to a new session. Credentials and adapter
capabilities are probed whenever a new session roster is resolved;
`mj models refresh` runs that probe as a standalone diagnostic.

The config schema is versioned. The current schema is `version = 7`; versions 3
through 6 are migrated in memory on load and reach disk in the current schema
the next time settings are saved — merely reading the file never rewrites it,
so older and newer mj builds can share one config until someone actually saves.
A file written by a *newer* build loads best-effort and is read-only: its
settings still show, saving is refused with a warning, and nothing is
downgraded. An unrecognized older version starts from fresh defaults rather
than guessing a field-by-field migration.

The guided product explanation has one monotonic `onboarding_version`, separate
from the config schema. Mjolnir compares it only with the latest onboarding:
someone several versions behind sees the current flow once, never a replay of
every missed flow. Finishing or explicitly skipping records the latest version;
canceling fresh setup leaves onboarding incomplete.

## Minimal config

What the **Codex coder + Claude reviewer** team writes:

```toml
version = 7
team = "codex_claude"

[agent]
model = "auto"
discrete_review = true

[review]
model = "auto"
permission = "auto"

[subagents]
model = "auto"
permission = "auto"
max_parallel = 6
auto_failover = true

[acp.policies]
claude-acp = "enabled"
codex-acp = "enabled"
```

`team` records the selected Team preset; the coder and reviewer ACP routes are
derived from it on every launch and are never persisted themselves.

The Reviewer and Subagents tabs each have a **Permissions** control. Its
default, `auto`, delegates routine approval decisions to the selected provider:
for Codex this is its native **Approve for me** setting, not a Mjolnir
auto-answer rule. `manual` and `yolo` select the provider's restrictive and
full-access presets.

`[agent]` is the primary agent: the session that owns every user turn. It cannot
be disabled. `[review]` configures the discrete-review model; review
is still enabled or disabled with `agent.discrete_review`, its depth is
chosen with `agent.review_tier`, and its optional Bifrost preprocessing is
controlled by `agent.bifrost_analysis`. `agent.correction_threshold` controls
which validated priorities receive automatic correction. The separate
`agent.mcp_discrete_review` opt-in adds a primary-session MCP tool and prompt
that require a review checkpoint before publishing code; it is off by default
and does not change the end-of-turn review. `[subagents]`
configures the default backing for `create_subagent`; set `model = "disabled"`
(or `"none"`) to turn subagents off entirely.

| Key | Meaning |
| --- | --- |
| `agent.model` | Primary model, or `auto` |
| `agent.acp_priority` | ACP source preference when several enabled adapters offer the primary model |
| `agent.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `agent.session_defaults` | Per-ACP saved session-option defaults for new primary sessions |
| `agent.discrete_review` | Run the end-of-turn discrete review |
| `agent.mcp_discrete_review` | Expose `request_discrete_review` to the primary and instruct it to review changed code before commit, push, PR, merge, tag, publication, or release. Default `false` |
| `agent.bifrost_analysis` | Precompute semantic diff context with Bifrost before review (default `true`). Set to `false` to use the bounded raw Git patch while keeping Bifrost navigation tools available. |
| `agent.review_tier` | Review depth: `quick` (default) sends one general reviewer and validates its findings; `extended` runs the adversarial supervisor with on-demand specialist lanes and spends far more tokens |
| `agent.correction_threshold` | Automatically correct validated findings through `p0`, `p1`, `p2`, or `p3` (default). Findings below the selected threshold remain tracked as deferred, and the Review Board records that policy reason. |
| `agent.max_correction_rounds` | Optional override for review passes over findings-driven corrections, also exposed as **Post-correction verification** in `/mjconfig`'s Reviewer panel; omitted defaults to `1` for both tiers; set to `0` to disable verification |
| `agent.runtime_stall_minutes` | Minutes without an ACP update before an active primary, review, or subagent runtime is shown as stalled; default `5`, `0` disables. Config file only |
| `review.model` | Review supervisor model, or `auto` |
| `review.acp_priority` | ACP source preference for the review supervisor model |
| `review.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `review.permission` | Provider-native permissions for review sessions: `manual`, `auto` (default), or `yolo` |
| `review.session_defaults` | Per-ACP saved session-option defaults for new review sessions, except Mode, which `review.permission` controls |
| `review.bifrost_version` | Optional exact Bifrost npm version. Omit it (the default) to follow the `latest` tag. `/mjconfig` offers `latest` and the five newest stable releases. |
| `subagents.model` | Default subagent model, `auto`, or `disabled` |
| `subagents.acp_priority` | Independent ACP source preference for the default worker model |
| `subagents.reasoning_effort` | Optional per-seat ACP reasoning effort |
| `subagents.permission` | Provider-native permissions for delegated sessions: `manual`, `auto` (default), or `yolo` |
| `subagents.session_defaults` | Per-ACP saved session-option defaults for newly created subagents, except Mode, which `subagents.permission` controls |
| `subagents.max_parallel` | Concurrent subagents, default 6, maximum 16 |
| `subagents.auto_failover` | Move the default pool to the next roster route when the current ACP source's quota runs low; the model may stay the same |
| `subagents.progress_wake_minutes` | Minutes a primary parked on running subagents may go without a report before it is woken with their progress alone; default 20, `0` disables. Config file only |
| `voice_auto_send` | `off` (default), `two_seconds`, `four_seconds`, `six_seconds`, or `eight_seconds`; submit a recognized voice prompt after that much detected silence |

In an active primary session, model and reasoning-effort entries in `/mjconfig`
and the `/model` and `/effort` commands update the current ACP session without
a restart when the connected agent advertises the corresponding selectors;
changes made during a turn apply after it finishes. Team and ACP routing changes
still apply to a new session. A `max_parallel` above 16 is a configuration
error, not a silently clamped value.

Every mj session shares one config file, so a `/mjconfig` save reaches sessions
already running elsewhere. Other terminal sessions notice the save within a few
seconds and adopt the settings a live session can take — session options such as
the permission mode, the review policy, and appearance — while routing changes
still wait for a new session. Only options the save actually changed are pushed,
so a `/mode` chosen inside another session survives. Each session lifecycle
(first session, `/new`, resume, load) re-reads the file, so a session started
after the save honors it whether or not it was running at the time. Saving from
the `mj remote` web panel reaches the sessions that host is already running, so
a permission mode set there applies without waiting for a new session.

Onboarding, the **Team** tab in `/mjconfig`, and **Shift+Tab** during a session
all offer the same four configurations:

| Team | Primary (coder) | Subagents and review (reviewer) |
| --- | --- | --- |
| **Codex** | Codex | Codex |
| **Claude** | Claude | Claude |
| **Codex coder + Claude reviewer** | Codex | Claude |
| **Claude coder + Codex reviewer** | Claude | Codex |

Choosing a team pins the primary seat to the coder, pins the subagent and
review seats to the reviewer, enables discrete review and subagent failover,
and enables the required built-in ACP routes. **Claude coder + Codex reviewer**
defaults review and subagents to `gpt-5-6-luna` at `xhigh` effort and selects
the extended review tier. Every other team keeps model selection on Auto and
preserves the selected review tier (Quick by default). When a team change
replaces the primary agent, **Shift+Tab** offers to switch immediately;
Mjolnir starts the new provider-native session with the complete durable
session transcript as context. See [Teams and adversarial review](/teams/).

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

## Migrating older configs

Versions 3 through 5 migrate in memory. The file on disk keeps its old schema
until settings are next saved, so other installed mj builds can still read it
in the meantime. Version 3's generated `max_correction_rounds = 1` value is
treated as unset so review depth can supply the current tier default; explicit
non-default values such as `0` or `3` are preserved.

Version 2 and earlier are no longer supported and start from fresh defaults.

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
mj --model provider/model-id \
  --review-model provider/review-model-id \
  --subagent-model disabled \
  --print "summarize this repository"
```

Overrides require explicit model IDs; `auto` is not accepted. Each accepts an
optional `+<effort>` suffix (`--model provider/model-id+high`). The saved
configuration remains unchanged.

## Shared project knowledge

Switching between Claude and Codex should not mean teaching the repository
twice. Mjolnir gives both agents one local, inspectable interface for verified
build requirements, architecture constraints, debugging conclusions, and
repository conventions.

Mjolnir keeps these short, durable facts (at most 2,000 bytes each) across
sessions in `memories.json`, next to the config. It synchronizes them into
Claude Code's native `MEMORY.md` and Codex's native `MEMORY.md` and
`memory_summary.md` files before sessions start. Mjolnir never prepends memory
text to a provider's user prompt.

Knowledge is global or project-scoped. The authenticated `mj-memory` MCP
server instructs both agents to save non-obvious, verified implementation
discoveries automatically, including architecture constraints, build
requirements, debugging conclusions, and repository conventions. It tells
agents not to store secrets, speculation, transient task state, or facts
trivially visible in source.

Claude Code's native auto-memory `MEMORY.md` is imported before synchronization
so Claude discoveries become available to Codex as well. Mjolnir honors
`CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_DISABLE_AUTO_MEMORY`, managed policy
settings, user settings, and project/local `autoMemoryEnabled`. A policy- or
user-configured `autoMemoryDirectory` is global; otherwise the standard
per-project path is used. Topic files are not flattened into the prompt.
Imports are source-tracked and updated in place. Project-scoped imports are
removed when disabled or superseded. Global imports are removed when their
source is reconfigured or confirmed absent; while auto-memory is disabled,
Codex's native block omits all Claude imports. Imported entries are a projection of
`MEMORY.md` rather than knowledge Mjolnir owns, so `/memory forget` declines
them and names the file; remove the text there and the next synchronization
drops it. Users can also manage knowledge with `/memory` or `mj memory`.

The feature is optional. A master switch plus two toggles control it, all on
by default:

```toml
[memory]
enabled = true           # master switch; false disables the feature entirely
use_memories = true      # synchronize stored memories into provider-native files
generate_memories = true # expose the memory_save / memory_forget tools
```

Set `enabled = false` (or run `/memory off` in the TUI) to switch memory off
entirely — no native synchronization and no tools, regardless of the other toggles. The
store and the management commands below keep working while disabled, and
`/memory` and `mj memory list` call out the disabled state. Toggle the
sub-switches with `/memory use on|off` and `/memory generate on|off`; all
changes apply to sessions started afterwards. Before the next Claude or Codex
session starts, Mjolnir removes its managed native-memory blocks while leaving
provider-owned memory and `memories.json` intact. `/memory` lists the stored
entries, `/memory forget <id>` deletes one, and `/memory clear confirm` (or
`mj memory clear --yes`) deletes everything.

## Appearance and session controls

Theme, spinner, thought-output, and feature-tip preferences are persistent.
Thought output defaults to **Default**, which summarizes completed thoughts and
shows a bounded tail while a thought is streaming. Choose **Full** under
**Appearance** in the TUI or web `/mjconfig`, or set `thought_output = "full"`
at the top level of the config file, to show all available thought text in both
transcripts. Feature tips are enabled by default and rotate on a dim
line beside the working spinner while a turn is in flight, in both the TUI and
the web viewer; disable them under **Appearance** or set
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
