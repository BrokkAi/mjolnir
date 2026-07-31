---
title: Subagents
description: How the primary agent launches background subagents, and how their reports come back.
---

Mjolnir runs one **primary agent** — the ACP session that owns every user turn —
plus any number of **subagents** it launches in the background. A subagent is
not a second configured role: it is a fresh ACP process and session that exists
for one task and then reports back.

## The two tools

Mjolnir advertises a local MCP server named `mj-subagents` to the primary
session over ACP Streamable HTTP (loopback URL, bearer token). It exposes
exactly two tools:

| Tool | Arguments | Purpose |
| --- | --- | --- |
| `create_subagent` | `prompt`, optional `label`, `cwd`, `resume` | Launch one background subagent |
| `subagent_cancel` | `subagent_id` | Interrupt a running subagent or release a finished one |

`create_subagent` returns as soon as the subagent has started. The tool result
carries `subagentId`, `status: "started"`, the resolved `agent` and `model`, and
the display `label` — never the subagent's work. The tool description says so
explicitly, because an agent that waits for a result it will never receive is
the main failure mode of a push-delivered design.

## Lifecycle

1. The primary calls `create_subagent` with a complete standalone brief.
2. Mjolnir admits the run against the pool, spawns a fresh ACP session, and
   returns the id immediately. The stable `Subagents` workflow row reflects the
   run, and `/subagents` opens its retained actor transcript.
3. The primary keeps working or ends its turn. The turn is not held open.
4. When the subagent finishes, Mjolnir **injects its report into the primary
   session as a new user message**, which starts a normal turn.

Nothing polls. There is no wait tool, and the primary is told not to invent one.

### The injected report

```text
<subagent_result id="3" label="fix-tests" agent="codex-acp" model="gpt-5.6-sol" outcome="completed" elapsed="4m12s">
<report>
…the subagent's final message…
</report>
<activity_summary>
…condensed log of the tool calls it made…
</activity_summary>
<workspace_diff>
…the diff that run produced…
</workspace_diff>
</subagent_result>

Review this report critically against the repository before relying on it.
```

`outcome` is `completed`, `cancelled`, or `failed`. Reports that finish while
the primary is mid-turn are queued and injected together as one message after
that turn completes, so a burst of subagents produces one follow-up turn rather
than several.

The report is the subagent's own account of its work. Mjolnir does not verify
it, and the injected message says so.

### Shared workspaces suppress the diff

Each run is snapshotted independently, so its `workspace_diff` is that
subagent's own changes. When another subagent was working in the same workspace
during the run the two sets of edits cannot be separated, and the section is
replaced with a note:

```text
omitted: 2 subagents shared this workspace during the run — inspect git diff yourself
```

### Cancelling

`subagent_cancel` interrupts a running subagent's turn and returns everything it
did up to that point — its activity log and workspace diff — in the tool result
itself. A cancelled subagent injects no report: the tool result is the whole
story. On a finished subagent, cancel releases the retained session instead.

Cancel never reverts edits. Whatever the subagent already wrote stays in the
workspace exactly as it left it.

Ctrl-C during a turn cancels the primary turn and every running subagent at
once; cancelling only the subagents would leave the primary free to launch the
same work again. Ctrl-C on an idle, empty prompt still quits, and quitting tears
down running subagents with the process.

### Resuming

A finished subagent's session is retained warm, so `resume: <id>` continues it
with a new prompt and the context it already built. Mjolnir retains up to
`subagents.max_parallel` finished sessions and reaps the oldest beyond that; a
reaped, unknown, or still-running id fails with an explicit error rather than
silently starting something else.

## Parallelism and write access

All subagents are equal. Up to `subagents.max_parallel` run concurrently
(default 6, maximum 16), every one of them has full write access to the
workspace, and none is confined to a read-only role.

When the pool is full, `create_subagent` fails immediately, naming the active
ids and the capacity. Nothing is queued — the primary decides what to do next.

Two subagents editing the same files will conflict. Mjolnir does not arbitrate
that: the primary is instructed to hand out non-overlapping work, and the
suppressed diff above is the signal that it did not.

An explicit `cwd` must be an absolute directory inside the workspace roots
Mjolnir already authorized (`--cwd` plus any `--additional-directory`). A
subagent cannot use those roots to reach an arbitrary sibling directory.

## Choosing the subagent model

Subagents run on the model selected by Mjolnir's `[subagents]` configuration.
When `subagents.auto_failover` is on, Mjolnir can move the configured pool to
another launchable route as provider quota changes. The primary agent cannot
override the model or ACP adapter on an individual `create_subagent` call.

## Workflow progress

Delegation and review workflows get a dedicated area between the header and the
input box, in both inline and fullscreen modes:

```text
 ⠹ Subagents [/subagents] · 1m04s · delegating · 3 running · 2 done
 ⠹ Review [/subagents] · 42s · specialist review · waiting for 1 automatic result · reviewers 2/3
```

Each row represents one authoritative workflow, not one actor. It shows the
current phase, aggregate actor counts, waits, coverage, and elapsed time, so
workers starting or finishing cannot move the input area. A terminal outcome
freezes in place until the next user turn instead of disappearing on a timer.
If the area overflows, active and newer workflows keep the visible slots.

Run `/subagents` to open the session-wide actor roster and inspect labels,
models, activity, tool calls, and retained transcripts. The roster allocates a
row to every retained actor, with running actors first and the newest actor
selected when it opens. Use PageUp/PageDown to move through long output; on a
MacBook, Fn+Up/Fn+Down send those keys. Every start and finish also lands in the
primary transcript as a permanent
`subagent #N · label · …` line. Live activity stays in the nested transcript,
so primary scrollback and terminal geometry are not rewritten.

## Discrete review

When a turn that used at least one subagent completes with the pool drained and
the workspace changed, Mjolnir reviews the finished work before releasing the
turn. A visible supervisor on the configured review model investigates the immutable
change packet and asynchronously launches only the useful read-only Norse
reviewers. Their reports return to the same supervisor session for vetting, and
surviving findings come back as a corrective turn. The supervisor and reviewers
use the same workflow progress and nested transcript machinery as ordinary
subagents but do not receive implementation write access or recursive
delegation tools. See
[Delegation and review](/delegation-review/).

## Turning subagents off

Set `model = "disabled"` under `[subagents]`. The primary keeps working; the
`mj-subagents` server is not advertised and neither tool exists. For one
headless invocation, `--subagent-model disabled` overrides the saved choice
without changing the config file.

Continue with [Delegation and review](/delegation-review/) for task shaping, or
[ACP adapters and models](/adapters/) for how routes are selected.
