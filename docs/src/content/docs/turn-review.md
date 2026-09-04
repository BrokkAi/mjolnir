---
title: Turn review
description: Configure an independent quick or extended review and resolve findings between agent turns.
---

Turn review asks a separate harness profile to inspect the work completed since the previous review boundary. It is an opt-in second opinion: the reviewer reads the governing user intent, transcript, repository state, and changed code, then reports only material, actionable findings.

The daemon owns review execution. A review started in the terminal continues if that terminal detaches, and the same review state is visible from the web viewer.

## Configure a reviewer

Add a `[review]` table to `config.toml`:

```toml
[review]
enabled = true
tier = "quick"
profile = "reviewer"
# model = "provider-model-id"
# effort = "high"
```

`profile` must name a configured [harness profile](/profiles/) that is different from the primary profile of the session being reviewed. This separation is enforced at runtime; Mjolnir will not let the same profile write and independently review a turn.

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Automatically review completed changed turns after the queue drains. |
| `tier` | `"quick"` | `quick` or `extended`; see the comparison below. |
| `profile` | none | Profile used for every reviewing role. Required for automatic and one-off review. |
| `model` | profile default | Optional reviewer model override. |
| `effort` | profile default | Optional reviewer reasoning-effort override. |

You may leave `enabled = false` while retaining `profile`, `model`, and `effort`; this disables automatic review but keeps one-off `/review` available. Configuration is read when a review starts, so a later review uses your latest settings.

## Quick and extended tiers

| Tier | How it works | Use it for |
| --- | --- | --- |
| `quick` | One general reviewer checks the turn. If it reports findings, a validator rechecks those claims against source before they reach you. | Routine turns and the lowest review cost. |
| `extended` | A supervisor examines the change and can dispatch focused specialists for control flow, duplication, error handling, dead code, tests, and contracts. When the message history makes intent ambiguous, a separate analyst first reconciles the governing intent. | Larger or riskier changes where wider coverage is worth more time and tokens. |

Both tiers apply the same qualification bar. A concern must have meaningful correctness, security, performance, or maintainability impact; it must be introduced by the reviewed turn, demonstrable from inspected evidence, and concrete enough to act on. Tests changed in the same turn are evidence to inspect, not an oracle for the intended behavior.

## Automatic review

With `enabled = true`, every completed prompt-driven turn arms review. Review runs between turns. If prompts are already queued, Mjolnir lets the queue drain and reviews the resulting batch rather than interleaving a reviewer with active work. The first step captures the repository delta; when nothing changed, the review resolves without launching a reviewer.

An open review holds new prompts for that session from preparation through a clean or findings verdict. This prevents more edits from racing ahead of work being inspected. A failed review releases the hold immediately, and other sessions remain independent throughout.

## Review on demand

With a reviewer profile configured, enter this in Prompt after a turn completes:

```text
/review
```

Use the status form to see how review is configured and whether one is open:

```text
/review status
```

Tier and automatic behavior belong in `config.toml`; `/review quick`, `/review on`, and similar command variants are not accepted. A one-off review also must run between turns, after queued prompts have drained.

## Read and resolve a verdict

While review is running, the review view shows the active role and its status. In a multi-role review, `Tab` switches among the reviewer conversations so you can inspect how the verdict was reached.

Resolution depends on the verdict:

- A **clean** verdict resolves automatically and advances the reviewed boundary.
- A **findings** verdict offers **Forward findings**, **Dismiss**, and **Cancel**. Forward sends the validated findings to the primary harness as its next corrective prompt; a later review can verify those corrections. Dismiss advances the reviewed boundary without requesting changes. Cancel closes the review without advancing it, so the same delta remains reviewable.
- A **failed** review offers **Dismiss** and **Cancel**. Its prompt hold has already been released, and neither choice advances the reviewed boundary; fix the profile, model, credential, or connectivity problem before trying again.

Cancel is also available while review work is still running. It releases the prompt hold and leaves the unreviewed changes for a later pass.

## Lifecycle behavior

Stopping a session while a reviewer conversation is open preserves its result for reference, but that reviewer's native conversation cannot continue after the target is destroyed. A later review starts a new reviewer conversation.

If Mjolnir restarts during a review, it clears the interrupted in-flight marker, releases the prompt hold, and leaves the reviewed boundary unchanged. The next review therefore covers the same changes instead of silently skipping them.

Turn-review traffic is charged through the configured reviewer profile. A different profile ID may still share account-level limits with the primary profile, so check the Quota pane before selecting an extended review for a large turn. See [configuration](/configuration/#automatic-review-review) for schema details.
