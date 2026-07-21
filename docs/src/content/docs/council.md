---
title: Thor, Eitri, and Loki
description: The responsibilities, guarantees, and limits of Mjolnir's coding Council.
---

The Council is three distinct roles, not necessarily three distinct providers
or models. Auto selection prefers useful separation but can reuse a model when
the launchable catalog has no qualifying alternative.

## Thor coordinates

Thor owns every user turn. It can use tools directly for small, tightly coupled
work and delegate a bounded implementation when a fresh context is more useful.
Mjolnir adds the coordinator policy to the first user message of a session and
re-arms it after detected context compaction.

Thor is never disabled. In Auto mode it selects the strongest launchable
eligible DeepSWE row for the configured reasoning tier; this is a ranking input,
not a claim of general model superiority.

## Eitri implements and explores

Eitri backs four Council tools exposed to Thor through an authenticated local
MCP server:

- `code_agent` starts one bounded implementation run.
- `code_agent_wait` waits for a longer run without blocking every poll.
- `explore_agent` performs one read-only investigation.
- `explore_agents` runs independent read-only investigations concurrently.

During an implementation handoff, Thor's foreground lane pauses and Eitri's
fresh ACP session streams through the normal Mjolnir transcript. Ctrl-C cancels
the active Eitri request. Eitri's final response and change summary return to
Thor before it continues.

Read-only explorations can overlap. Loki does not review exploration-only runs;
its Eitri path is reserved for change-producing `code_agent` work.

## Loki reviews without blocking

Loki is a long-lived, best-effort, read-only reviewer. Transcript checkpoints
flow into its queue, and ready advice is delivered at natural boundaries. Work
never waits for Loki.

Advice can arrive:

- with an Eitri tool result;
- before Thor concludes a user turn;
- as one Council-initiated follow-up after the turn; or
- at a later boundary when the user has already started another prompt.

Every delivered review is labelled with the turn and span it observed because
later work may have superseded it. Automatic discrete Thor review is separate
from Loki and can be toggled in `/mjconfig`.

## Disable optional roles

Set `model = "disabled"` under `[loki]` or `[eitri]`. Thor remains available.
For one headless invocation, `--loki disabled` and `--eitri disabled` override
the saved choices without modifying the config file.

Continue with [Delegation and review](/delegation-review/) for task-shaping
guidance or [ACP adapters and models](/adapters/) for route selection.
