---
title: Delegation and adversarial review
description: Give the coder bounded subagent work and interpret Mjolnir's independent review pass.
---

Delegation works best when the task has a clear seam, concrete inputs, and an
observable finish condition. A subagent runs in a brand-new session with no
memory of the conversation, so the brief has to carry everything. Everything
on this page applies to whichever coder the selected [team](/teams/) put in
charge — Codex or Claude.

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
releasing it. On a mixed [team](/teams/), the review seat runs on the other
provider, so the model challenging the change is not the model that made it.

Discrete review is toggled on the Reviewer tab of `/mjconfig`. The same tab
chooses the **review tier** — how much machinery one review may spend. The
tier is read when a review dispatches, so a change applies to the next
reviewed turn.

The Reviewer tab has an independent **Permissions** setting. Its default,
`Auto`, starts Codex with its native **Approve for me** policy and Claude Code
with its native Auto policy; Mjolnir does not auto-answer approval requests.
Other selected-provider session options remain separately available. The
**Permissions** setting owns the provider's **Mode** option for reviewer
sessions, so any older saved Mode value is ignored. Mjolnir-hosted ACP
filesystem and terminal capabilities stay read-only for review sessions.

### Quick tier (default)

One general reviewer (Vör, visible as `review · vor`) works the
cumulative turn patch directly with Bifrost navigation tools, prioritizing
correctness against the user's stated intent. No intent-extraction model call
is made and no specialist lanes exist in this tier. Every finding it reports
is re-verified by a validation pass before anything reaches the primary, so
few verified findings beat many plausible ones.

### Extended tier

The full adversarial pass, selected on the Reviewer tab or with
`review_tier = "extended"` under `[agent]` in the config file. It is more
thorough than Quick and spends far more tokens:

1. A single self-contained user prompt goes directly to review without another
   model call. For multi-message histories, an intent analyst extracts
   the governing contract and reconciles earlier corrections or requirements.
2. A first-class internal review supervisor on the configured review model receives
   Bifrost core navigation tools and an immutable change packet. It runs in a
   detached session but is not a subagent. Changes under 200 lines
   include the complete captured diff; larger changes include the complete
   semantic file totals and `patch_symbols` from `analyze_diff` for the
   captured base and target trees.
3. The supervisor forms a risk map from the change packet and targeted source
   inspection. It launches a Norse reviewer only for a concrete
   unresolved hypothesis that the lane can investigate: Mímir (complexity),
   Völundr (duplication), Týr (error handling), Hel (dead code), Heimdall
   (tests), and Bragi (comments and contracts). Zero reviewers is a normal
   outcome; patch size does not determine the roster, while several independent
   risks can justify several lanes even in a small patch. Reports arrive as
   later turns in the same supervisor session, where the supervisor verifies
   them and returns one adversarial verdict.

### After the review

On both tiers, surviving findings are injected as a corrective turn on the
primary, framed as strong leads to verify rather than instructions to obey.
Nothing surviving means the turn is released as it stands. If correction
changes the workspace, Quick releases it after the primary's required
validation. Extended runs one bounded, delta-scoped verification pass over the
correction while reusing prior evidence instead of blindly relaunching every
reviewer. Set `max_correction_rounds` under `[agent]` to override either tier's
default.

Reviewers have no model-turn deadline. The extended supervisor is reported as
an internal `review_session`, while dispatched reviewers — Vör on the quick
tier, selected specialists on the extended tier — remain visible as
`review · {name}` subagent rows. The normal Stop action cancels the active
review pass and all of its reviewers and reaps their processes. Reviewers
cannot delegate further or write to the workspace. Model usage is accounted to
the review seat.

## Review surfaces

| Surface | Behavior |
| --- | --- |
| Discrete review | Automatic end-of-turn review whenever the completed turn changed the workspace |
| `/discrete-review recent` | Run the configured discrete-review tier over the latest change-producing turn |
| `/discrete-review uncommitted` | Run the configured discrete-review tier over all current worktree changes |
| `/discrete-review head` | Run the configured discrete-review tier over `HEAD` |
| `/adversarial-review …` | Alias for `/discrete-review …` |

Append `quick` or `extended` to any on-demand command to override the configured
tier for that pass, for example `/discrete-review head extended`. On-demand
passes report their findings without starting a corrective turn.

## Record evaluations

When comparing setups, record the exact primary and subagent models and
adapters, how many subagents ran and whether they overlapped, permission
decisions, elapsed time, token and cost telemetry, validation result, review
findings, and whether the requested delegation actually occurred. The checked
[10-minute evaluation](/evaluate/) provides a small common task.
