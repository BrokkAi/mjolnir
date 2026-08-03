---
title: Delegation and review
description: Shape standalone subagent briefs and interpret Mjolnir's discrete review.
---

Delegation works best when the task has a clear seam, concrete inputs, and an
observable finish condition. A subagent runs in a brand-new session with no
memory of the conversation, so the brief has to carry everything.

## A useful brief

Ask the primary agent to give each subagent:

1. one bounded objective;
2. the context and decisions it needs to start immediately, quoted rather than
   paraphrased;
3. exact validation to run;
4. files or behaviors that must not change; and
5. the report you expect back.

Example:

```text
Launch a subagent for this: fix the parser's empty-input panic without changing
the public AST. Add the smallest regression test, run that test and the parser
module tests, and report the root cause plus what you verified.
```

Small edits that need the same context the primary is already holding are
usually faster done directly — delegation pays off when the work is clearly
larger than writing the brief and reviewing the result. Read-only investigation
is a normal subagent task too; there is no separate explore tool and no
read-only variant.

## Parallel work

Several subagents run at once and all of them can write, so the split matters
more than the count. Give each one files or modules the others will not touch.
When two share a workspace, neither report can show an isolated diff and you are
told to inspect `git diff` yourself — treat that note as a sign the split was
too coarse.

Subagents use the model and ACP routing selected in Mjolnir's `[subagents]`
configuration. Use `resume` for a follow-up on work a subagent already did, so
its context is not rebuilt from scratch.

## Cancellation and permissions

Ctrl-C during a turn cancels the primary turn and every running subagent
together. `subagent_cancel` stops one by id. Neither reverts edits already made.

Permission requests raised by a subagent are prefixed with its id
(`subagent #3 · …`), so concurrent prompts stay attributable in the terminal and
in the remote viewer. Permission approval does not make the model correct;
review the requested command, path, workspace root, and side effects first.

## Discrete review

When automatic review is enabled, any completed turn that changed the workspace
is reviewable once write-capable implementation subagents have drained. This is
independent from delegation: a turn implemented entirely by the primary follows
the same review gate. Mjolnir holds the completion and reviews the work before
releasing it:

1. A single self-contained user prompt goes directly to review without another
   model call. For multi-message histories, a read-only intent analyst extracts
   the governing contract and reconciles earlier corrections or requirements.
2. A first-class internal review supervisor on the configured review model receives
   Bifrost core navigation tools and an immutable change packet. It runs in a
   detached read-only session but is not a subagent. Changes under 200 lines
   include the complete captured diff; larger changes include the complete
   diffstat plus `analyze_diff` results for the captured base and target trees.
3. The supervisor forms a risk map from the change packet and targeted source
   inspection. It launches a read-only Norse reviewer only for a concrete
   unresolved hypothesis that the lane can investigate: Mímir (complexity),
   Völundr (duplication), Týr (error handling), Hel (dead code), Heimdall
   (tests), and Bragi (comments and contracts). Zero reviewers is a normal
   outcome; patch size does not determine the roster, while several independent
   risks can justify several lanes even in a small patch. Reports arrive as
   later turns in the same supervisor session, where the supervisor verifies
   them and returns one adversarial verdict.
4. Surviving findings are injected as a corrective turn on the primary, framed
   as strong leads to verify rather than instructions to obey. Nothing survives
   vetting means the turn is released as it stands.
5. If correction changes the workspace, one bounded, delta-scoped verification
   pass checks the corrections while reusing prior evidence instead of blindly
   relaunching every specialist.

The supervisor and reviewers have no model-turn deadline. The supervisor is
reported as an internal `review_session`, while selected specialists remain
visible as `review · {name}` subagent rows. The normal Stop action cancels the
supervisor and all of its reviewers and reaps their processes.
Reviewers cannot delegate further or write to the workspace. Model usage is
accounted to the review seat. Discrete review is toggled on the Agents tab of
`/mjconfig`.

## Review surfaces

| Surface | Behavior |
| --- | --- |
| Discrete review | Automatic end-of-turn review whenever the completed turn changed the workspace |
| `/review recent` | Findings-only review of the latest change-producing turn |
| `/review uncommitted` | Findings-only review of all current worktree changes |
| `/review head` | Findings-only review of `HEAD` |

A review can legitimately report no findings. Findings are evidence to consider,
not an automatic rollback or proof that the change is safe.

## Record evaluations

When comparing setups, record the exact primary and subagent models and
adapters, how many subagents ran and whether they overlapped, permission
decisions, elapsed time, token and cost telemetry, validation result, review
findings, and whether the requested delegation actually occurred. The checked
[10-minute evaluation](/evaluate/) provides a small common task.
