---
title: Turn review
description: Configure an independent quick or extended review and resolve findings between agent turns.
---

Turn review starts a separate reviewer conversation to inspect the work completed since the previous review boundary. It is an opt-in second opinion: the reviewer reads the user's messages, the repository, and the changed code, then reports only material, actionable findings.

The daemon owns review execution. A review started in the terminal continues if that terminal detaches, and the same review state is visible from the web viewer.

## Configure a reviewer

**Settings → Code Review** configures both turn review and the plan approval dialog's **Second opinion**. Choose **Auto** or a named [harness profile](/profiles/). A named profile may also be the primary session's profile: the reviewer always has its own conversation and private harness home.

Auto is the default. In `config.toml`, omit `profile`, `model`, and `effort`:

```toml
[review]
enabled = true
tier = "quick"
```

Auto first tries profiles from a different provider, then other profiles from the primary provider, then the primary profile itself. Within each group it ranks healthy quota before reserve quota before unknown quota, then uses provider order **Codex → Claude → DeepSeek → Kimi**. Remaining quota and profile ID break ties. It uses the same quota rules as utility inference: a known 0% window excludes a profile, 1–10% is reserve, above 10% is healthy, API billing is healthy, and missing or failed quota readings remain eligible as unknown.

| Provider | Auto main reviewer | Specialist lanes, including manual review |
| --- | --- | --- |
| OpenAI Codex | Astra / medium | Luna / xhigh; fast mode when available |
| Claude | Fable / medium | Sonnet / xhigh |
| DeepSeek | Flash / max | Flash / high |
| Kimi | Newest K-series / max | Main reviewer's model and effort |
| Other review-capable providers | Manual selection only | Main reviewer's model and effort |

Model families resolve to the newest advertised matching model. DeepSeek through a Codex harness counts as DeepSeek, not OpenAI. Effort values must be supported exactly. Auto skips unusable candidates and explains why if none can run. A manually selected profile fails visibly instead of changing profiles. Fast mode is optional; its rejection does not prevent review.

The main-reviewer choice is shared by quick review, supervision, and plan second opinion. Specialist lanes use the table's overrides even when you select the main reviewer manually.

To choose manually:

```toml
[review]
profile = "reviewer"
# model = "provider-model-id"
# effort = "high"
```

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Automatically review completed changed turns after the queue drains. |
| `tier` | `"quick"` | Turn review's `quick` or `extended` tier. |
| `profile` | Auto | Omit for Auto or name an enabled review-capable profile. |
| `model` | harness default for a named profile | Optional main-reviewer model override; unavailable in Auto. |
| `effort` | harness default for a named profile | Optional main-reviewer effort override; unavailable in Auto. |

With `enabled = false`, both `/review` and plan second opinion remain available. Each new review reads current settings; an already-open review keeps its selection. Reviewers do not appear in the main session navigation or Resume list. Ordinary sessions using the same profile remain visible.

### Per-session settings

`mj new` can override these settings for the session it creates, and `mj import <harness>` for the session it adopts:

- `--review-model <model>` and `--review-effort <effort>` turn on automatic review for that session, even when `enabled = false`. They replace `model` and `effort` for that session's turn reviews. When `[review]` names a profile, the model must be one that profile offers. In Auto, Mjolnir uses the first enabled profile, in Auto's usual order, that offers the model, including profiles Auto has no policy for. A named effort replaces the policy's effort.
- `--review-tier quick|extended` turns on automatic review for that session at that tier, even when `enabled = false`, and replaces `tier` for its automatic reviews and its `/review`.
- `--no-review` turns off automatic review for that session, even when `enabled = true`. `/review` still reviews a turn on request.

The choice is stored with the session and kept when it resumes. A session created without these flags follows `[review]`. Plan second opinion always uses `[review]`.

## Plan second opinion

Choose **Second opinion** before approving a proposed plan. Mj starts the reviewer from the shared settings, without a separate profile/model/effort picker. It asks the planning agent for context and sends that context plus the captured plan to the reviewer. You can transfer feedback for a revised plan, implement the original plan, or cancel. Preparation supports cancellation and retry; failures leave the plan unapproved. Previously remembered workspace reviewer choices are no longer used.

## Quick and extended tiers

| Tier | How it works | Use it for |
| --- | --- | --- |
| `quick` | One general reviewer checks the turn. Its findings go straight to the primary harness, marked as one reviewer's unverified findings; the harness checks each against source as it fixes them. | Routine turns and the lowest review cost. |
| `extended` | A supervisor examines the change and can dispatch focused specialists for control flow, duplication, error handling, dead code, tests, and contracts. The supervisor starts as soon as the change is captured. | Larger or riskier changes where wider coverage is worth more time and tokens. |

Every reviewing agent reads the change from the capture: the diff, and Git's added and removed line counts for every changed file, which list the whole change even when a large diff is cut short in the prompt. A small change's supervisor reads the whole diff; a large one starts from the per-file counts.

Reviewers take the intent from the user's messages in the session's current context (since its last compaction), in order, with the most recent governing message winning a conflict. They are not given the primary agent's own messages or its report of the work, so they judge the change against what was asked rather than against the author's account of it.

Reviewers navigate the code with Bifrost's tools. For the callers, references, or tests of a declaration they use `scan_usages_by_location`, which answers in well under a second once the server's index is built; they are told not to use `usage_graph`, which builds a reference graph for whole files and can take minutes on a large file.

Both tiers apply the same qualification bar. A concern must have meaningful correctness, security, performance, or maintainability impact; it must be introduced by the reviewed turn, demonstrable from inspected evidence, and concrete enough to act on. Tests changed in the same turn are evidence to inspect, not an oracle for the intended behavior.

## Automatic review

With `enabled = true`, every completed prompt-driven turn arms review. Review runs between turns. If prompts are already queued, Mjolnir lets the queue drain and reviews the resulting batch rather than interleaving a reviewer with active work. Preparation resolves the reviewer settings and captures the repository delta; when nothing changed, review resolves without sending a review prompt.

An open review holds new prompts for that session from preparation through a clean or findings verdict. This prevents more edits from racing ahead of work being inspected. A failed review releases the hold immediately, and other sessions remain independent throughout.

## Review on demand

With an Auto-eligible or manually selected reviewer available, enter this in Prompt after a turn completes:

```text
/review
```

Use the status form to see how review is configured and whether one is open:

```text
/review status
```

Tier and automatic behavior belong in `config.toml`; `/review quick`, `/review on`, and similar command variants are not accepted. A one-off review also must run between turns, after queued prompts have drained.

From a script, the same two forms are CLI commands:

```text
mj review start --session <id>
mj review status --session <id>
```

`mj review start` answers once the review has opened, or says why it cannot start. `mj review status` shows what the open review is doing, each reviewing role, and its verdict once it has one, or that no review is open.

## Read and resolve a verdict

While review is running, the review view shows the active role and its status. In a multi-role review, `Tab` switches among the reviewer conversations so you can inspect how the verdict was reached.

Resolution depends on the verdict:

- A **clean** verdict resolves automatically and advances the reviewed boundary.
- A **findings** verdict is forwarded automatically, for automatic and one-off reviews alike: Mjolnir sends the findings to the primary harness as its next corrective prompt, saying whether they come from one quick reviewer or from an extended review's supervisor, and a later review can verify those corrections. If the primary rejects that prompt, the review stays open with **Forward findings** to retry, **Dismiss** to advance the reviewed boundary without requesting changes, and **Cancel** to close the review without advancing it, so the same delta remains reviewable.
- A **failed** review offers **Dismiss** and **Cancel**. Its prompt hold has already been released, and neither choice advances the reviewed boundary; fix the profile, model, credential, or connectivity problem before trying again.

Cancel is also available while review work is still running. It releases the prompt hold and leaves the unreviewed changes for a later pass.

## Lifecycle behavior

New eligible primary sessions capture a review baseline even when automatic review is off. A session started without a baseline must be resumed or restarted after configuring an eligible reviewer. Child sessions do not capture independent review baselines.

Suspending a session while a reviewer conversation is open preserves its result for reference, but that reviewer's native conversation cannot continue after the target is destroyed. A later review starts a new reviewer conversation.

While a review is preparing or running, Mjolnir starts no recovery copy or worker upgrade for that session; a copy already running when the review begins is finished first. Deferred copies and upgrades start as soon as the review closes.

If Mjolnir restarts during a review, it clears the interrupted in-flight marker, releases the prompt hold, and leaves the reviewed boundary unchanged. The next review therefore covers the same changes instead of silently skipping them.

Review traffic is charged through the selected reviewer profile. A different profile ID may still share account-level limits with the primary profile, so check the Profiles pane before selecting an extended review for a large turn. See [configuration](/configuration/#automatic-review-review) for schema details.

When [automatic continuation](/sessions/#automatic-continuation) is enabled, automatic review waits for its check and any continuation turns to settle. The reviewer then considers the completed chain against your original request; generated continuation prompts do not become new user requirements.
