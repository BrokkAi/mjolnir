---
title: Teams and adversarial review
description: Pair Codex and Claude as coder and reviewer so the model that challenges a change is not the model that wrote it.
---

Mjolnir runs Codex and Claude through one terminal, permission, session, and
remote workflow. A **team** decides which of them fills each seat:

- The **coder** backs the primary session — the agent that plans, edits, runs
  tools, and answers every user turn.
- The **reviewer** backs the discrete-review pass that challenges changed turns
  and the default subagent pool.

The reason to split the seats is adversarial review. When discrete review is
enabled — every team preset enables it — a completed turn that changed the
workspace is held once its subagents have drained, and an independent session
reviews the work before it is released. On a mixed team that session runs on
the other provider, so the model challenging the diff did not write it and
does not share its blind spots. Findings survive a validation pass before they reach the
coder, and a clean review is a normal outcome.

## The four teams

Onboarding, the **Team** tab in `/mjconfig`, and **Ctrl+Tab** during a session
offer the same four configurations:

| Team | What it does |
| --- | --- |
| **Codex** | Codex handles primary, subagents, and review |
| **Claude** | Claude handles primary, subagents, and review |
| **Codex coder + Claude reviewer** | Codex is primary; Claude handles subagents and review |
| **Claude coder + Codex reviewer** | Claude is primary; Codex handles subagents and review |

Choosing a team keeps every model selection on Auto, pins the primary seat to
the coder's adapter and the review and subagent seats to the reviewer's,
enables discrete review and subagent failover, and enables the built-in ACP
routes its seats use — both routes for a mixed team, only that provider's
route for a single-provider team. Team changes apply to a new session; after saving from **Ctrl+Tab**,
start the offered session to use the new team immediately.

A single-provider team still gets discrete review — the reviewer is an
independent session with its own context, just not an independent provider.

## The default team

Until you choose a team, Mjolnir picks the one your machine can run:

| Signed in | Default team |
| --- | --- |
| Codex and Claude | **Claude coder + Codex reviewer** |
| Claude only | **Claude** |
| Codex only | **Codex** |

Setup opens on that team, and runs that never see setup — headless
`mj --print`, `mj server` — use it too. An ACP server switched off in
`/mjconfig` counts as unavailable, so a disabled Codex leaves a
both-providers machine on **Claude**. Choose a team yourself and Mjolnir keeps
that choice; the default only fills in what you left unset.

## What a team does not change

Mjolnir's terminal, permissions, worktrees, session storage, remote control,
and voice input are identical across teams. Switching teams changes which
provider fills each seat, never the workflow around them. Model requests go to
the selected providers under their own terms; see
[Data and trust boundaries](/data-boundaries/).

## How the review runs

Review depth is controlled by the review tier on the **Reviewer** tab of
`/mjconfig`:

- **Quick** (default) sends one general reviewer over the change, then
  validates its findings before anything reaches the coder.
- **Extended** runs an adversarial supervisor that forms a risk map and
  launches read-only specialist lanes on demand. It is more thorough and
  spends far more tokens.

Surviving findings return to the coder as a corrective turn, framed as strong
leads to verify rather than instructions to obey. See
[Delegation and adversarial review](/delegation-review/) for the full
mechanics, and `/review` for explicit findings-only reviews of recent,
uncommitted, or `HEAD` changes.

## Requirements

Each provider in the team needs its own working credentials: the `codex` CLI
signed in for Codex, the `claude` CLI signed in for Claude. Roster resolution
probes every enabled adapter, and a route that cannot launch is unavailable for
its seats. Use `/agents` to record the model and adapter that actually bound to
each seat.

Continue with [Start with Codex](/codex/) or [Start with Claude](/claude/) to
set up a provider, then run the [10-minute evaluation](/evaluate/) to see a
delegated, reviewed change end to end.
