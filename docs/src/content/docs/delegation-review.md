---
title: Delegation and review
description: Shape bounded Eitri tasks and interpret asynchronous Council review.
---

Delegation works best when the task has a clear seam, concrete inputs, and an
observable finish condition.

## A useful handoff

Ask Thor to give Eitri:

1. one bounded objective;
2. the relevant repository and constraints;
3. exact validation to run;
4. files or behaviors that must not change; and
5. the expected return evidence.

Example:

```text
Delegate this to Eitri: fix the parser's empty-input panic without changing the
public AST. Add the smallest regression test, run that test and the parser module
tests, and return the root cause plus the diff summary.
```

Small edits that require the same context as Thor's current reasoning are often
better done directly. Open-ended, read-only questions are better suited to
`explore_agent` or parallel `explore_agents`.

## Cancellation and permissions

The role currently streaming in the foreground owns Ctrl-C. During a handoff,
that means Eitri is cancelled while Thor remains paused. Permission requests are
namespaced so the user or remote client answers the active nested request.

Permission approval does not make the model correct. Review the requested
command, path, workspace root, and side effects before accepting it.

## Review surfaces

| Surface | Behavior |
| --- | --- |
| Loki | Best-effort asynchronous advice over transcript checkpoints |
| Automatic Thor review | Optional end-of-turn discrete workspace review |
| `/review recent` | Findings-only review of the latest change-producing turn |
| `/review uncommitted` | Findings-only review of all current worktree changes |
| `/review head` | Findings-only review of `HEAD` |

A review can legitimately report no findings. Advice is evidence to consider,
not an automatic rollback or proof that the change is safe.

## Record evaluations

When comparing Councils, record the exact Thor/Eitri/Loki models and adapters,
permission decisions, elapsed time, token and cost telemetry, validation result,
review findings, and whether the requested handoff actually occurred. The
checked [10-minute evaluation](/evaluate/) provides a small common task.
