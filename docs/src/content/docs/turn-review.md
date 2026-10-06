---
title: Turn review
description: Configure one independent reviewer and resolve findings between agent turns.
---

Turn review starts one independent reviewer after a turn finishes. It reads the user's request and the captured repository changes, then reports material, actionable findings. The daemon owns review execution, so a review started in the terminal continues if that terminal detaches. The same review is visible in the web viewer.

## Configure a reviewer

**Settings → Code Review** configures both turn review and the plan approval dialog's **Second opinion**. Choose **Auto** or a named [harness profile](/profiles/). A named profile may also be the primary session's profile; the reviewer still has its own conversation and private harness home.

Auto is the default. In `config.toml`, omit `profile`, `model`, and `effort`:

```toml
[review]
enabled = true
```

Auto first tries profiles from a different provider, then other profiles from the primary provider, then the primary profile itself. Within each group it ranks healthy quota before reserve quota before unknown quota, then uses provider order **Codex → Claude → DeepSeek → Kimi**. Remaining quota and profile ID break ties. A known 0% quota window excludes a profile, 1–10% is reserve, above 10% is healthy, API billing is healthy, and missing or failed quota readings remain eligible as unknown.

| Provider | Auto reviewer |
| --- | --- |
| OpenAI Codex | Astra / medium |
| Claude | Fable / medium |
| DeepSeek | Flash / max |
| Kimi | Newest K-series / max |
| Other review-capable providers | Manual selection only |

Model families resolve to the newest advertised matching model. DeepSeek through a Codex harness counts as DeepSeek, not OpenAI. Effort values must be supported exactly. Auto skips unusable candidates and explains why if none can run. A manually selected profile fails visibly instead of changing profiles.

To choose a profile, model, or effort manually:

```toml
[review]
profile = "reviewer"
# model = "provider-model-id"
# effort = "high"
```

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Automatically review completed changed turns after the queue drains. |
| `profile` | Auto | Omit for Auto or name an enabled review-capable profile. |
| `model` | harness default for a named profile | Optional reviewer model override; unavailable in Auto. |
| `effort` | harness default for a named profile | Optional reviewer effort override; unavailable in Auto. |

The old `tier` configuration field is deprecated and ignored. Existing
`quick` and `extended` values remain readable for compatibility.

With `enabled = false`, `/review` and plan second opinion remain available. Each new review reads current settings; an already-open review keeps its selection. Reviewers do not appear in the main session navigation or Resume list.

### Per-session settings

`mj new` can override these settings for the session it creates, and `mj import <harness>` for the session it adopts:

- `--review-model <model>` and `--review-effort <effort>` turn on automatic review for that session, even when `enabled = false`. They replace `model` and `effort` for that session's turn reviews.
- `--review-tier quick|extended` is deprecated. It still accepts either value
  and turns on automatic review for that session, but the value has no effect.
- `--no-review` turns off automatic review for that session, even when `enabled = true`. `/review` still reviews a turn on request.

The choice is stored with the session and kept when it resumes. A session created without these flags follows `[review]`. Plan second opinion always uses `[review]`.

## Plan second opinion

Choose **Second opinion** before approving a proposed plan. Mj starts the reviewer from the shared settings, without a separate profile/model/effort picker. It asks the planning agent for context and sends that context plus the captured plan to the reviewer. You can transfer feedback for a revised plan, implement the original plan, or cancel. Preparation supports cancellation and retry; failures leave the plan unapproved.

## What the reviewer reads

The reviewer follows the Codex `/review` rubric and returns Codex's JSON review format. Mj renders that output as readable findings text before displaying or storing it. An empty findings list is a clean review; findings include their title, location, and explanation.

The initial prompt includes the user's messages and a per-file diffstat, not the full diff. For each repository it gives the reviewer the root, the baseline tree, the captured tree, and the command:

```text
git -C <root> diff --no-ext-diff <baseline_tree> <capture_tree>
```

The reviewer runs that command itself with its harness's shell or read tools. The tree IDs keep the review scoped to the captured changes, even if files change later. Repository contents and tool output are treated as untrusted data, not instructions.

The reviewer receives the user's messages as the source of intent; the primary agent's account of its own work is not included. On a corrective pass, it also receives the previous findings so it can check the changes made in response.

## Automatic review

With `enabled = true`, every completed prompt-driven turn arms review. Review runs between turns. If prompts are already queued, Mjolnir lets the queue drain and reviews the resulting batch. Preparation resolves the reviewer settings and captures the repository delta; when nothing changed, review resolves without sending a review prompt.

An open review holds new prompts for that session from preparation through a clean or findings verdict. This prevents more edits from racing ahead of work being inspected. A failed review releases the hold immediately, and other sessions remain independent throughout.

## Review on demand

After a turn completes, enter:

```text
/review
```

Use the status form to see how review is configured and whether one is open:

```text
/review status
```

These are the supported `/review` forms. `/review quick` and
`/review extended` are not review modes. Use configuration to change automatic
review settings.

From a script, the same two forms are CLI commands:

```text
mj review start --session <id>
mj review status --session <id>
```

`mj review start` answers once the review has opened, or says why it cannot start. `mj review status` shows the review status, its reviewer role, and its verdict once it has one, or that no review is open.

## Read and resolve a verdict

The review view provides **Overview**, the reviewer's transcript, and **Verdict**. Use Tab to move among them. The web viewer shows the same single reviewer role and status.

Resolution depends on the verdict:

- A **clean** verdict resolves automatically and advances the reviewed boundary.
- A **findings** verdict is forwarded to the primary harness as a corrective prompt. If the primary rejects that prompt, the review stays open with **Forward findings** to retry, **Dismiss** to advance the reviewed boundary without requesting changes, and **Cancel** to close the review without advancing it.
- A **failed** review offers **Dismiss** and **Cancel**. Its prompt hold has already been released, and neither choice advances the reviewed boundary; fix the profile, model, credential, or connectivity problem before trying again.

Cancel is also available while review work is still running. It releases the prompt hold and leaves the unreviewed changes for a later pass.

## Lifecycle behavior

New eligible primary sessions capture a review baseline even when automatic review is off. A session started without a baseline must be resumed or restarted after configuring an eligible reviewer. Child sessions do not capture independent review baselines.

Suspending a session while a reviewer conversation is open preserves its result for reference, but that reviewer's native conversation cannot continue after the target is destroyed. A later review starts a new reviewer conversation.

While a review is preparing or running, Mjolnir starts no recovery copy or worker upgrade for that session; a copy already running when the review begins is finished first. Deferred copies and upgrades start as soon as the review closes.

If Mjolnir restarts during a review, it clears the interrupted in-flight marker, releases the prompt hold, and leaves the reviewed boundary unchanged. The next review therefore covers the same changes instead of silently skipping them.

Review traffic is charged through the selected reviewer profile. A different profile ID may still share account-level limits with the primary profile. See [configuration](/configuration/#automatic-review-review) for schema details.

When [automatic continuation](/sessions/#automatic-continuation) is enabled, automatic review waits for its check and any continuation turns to settle. The reviewer then considers the completed chain against your original request; generated continuation prompts do not become new user requirements.
