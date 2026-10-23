---
title: Turn review
description: Configure one independent reviewer and resolve findings between agent turns.
---

Mjolnir can review work at the end of each turn or before an agent pushes commits. Choose the trigger in **Settings → Code Review** or in `config.toml`. A reviewer runs as a hidden child session of the session it reviews. It uses the profile, model, and effort selected in review settings. It does not appear in the agent's `list_agents` results or count against the child-session limit. Its transcript appears in the review view.

## Configure a reviewer

**Settings → Code Review** selects the trigger and reviewer settings. The trigger choices are **Off**, **End of turn**, and **Before push**. Choose **Auto** or a named [harness profile](/profiles/). A named profile may also be the primary session's profile; the reviewer still has its own conversation and private harness home.

Auto is the default. In `config.toml`, omit `profile`, `model`, and `effort`:

```toml
[review]
trigger = "turn"
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
| `trigger` | `"off"` | `"off"` disables automatic review, `"turn"` reviews completed turns, and `"push"` reviews commits before push. |
| `profile` | Auto | Omit for Auto or name an enabled review-capable profile. |
| `model` | harness default for a named profile | Optional reviewer model override; unavailable in Auto. |
| `effort` | harness default for a named profile | Optional reviewer effort override; unavailable in Auto. |

The old `tier` configuration field is deprecated and ignored. Existing
`quick` and `extended` values remain readable for compatibility.

An older configuration with `enabled = true` still loads as `trigger = "turn"`; `enabled = false` loads as `"off"`. Saving settings writes `trigger`, not `enabled`. `/review` remains available when the trigger is off. Each new review reads current settings; an already-open review keeps its selection. Reviewer children do not appear in the main session navigation or Resume list.

### Per-session settings

`mj new` and `mj import <harness>` can override these settings for one session:

- `--review off|turn|push` selects that session's trigger, overriding `[review]`.
- `--review-model <model>` and `--review-effort <effort>` replace the reviewer model and effort for that session. If you omit `--review`, either option selects `turn`.
- `--review-tier quick|extended` is deprecated. It still accepts either value
  and selects `turn` when you omit `--review`, but the value has no effect.

The choice is stored with the session and kept when it resumes. A session created without these flags follows `[review]`.

## What the reviewer reads

The reviewer follows the Codex `/review` rubric and returns Codex's JSON review format. Mj renders that output as readable findings text before displaying or storing it. An empty findings list is a clean review; findings include their title, location, and explanation.

The initial prompt includes the user's messages and a per-file diffstat, not the full diff. For each repository it gives the reviewer the root, the baseline tree, the captured tree, and the command:

```text
git -C <root> diff --no-ext-diff <baseline_tree> <capture_tree>
```

The reviewer runs that command itself with its harness's shell or read tools. The tree IDs keep the review scoped to the captured changes, even if files change later. Repository contents and tool output are treated as untrusted data, not instructions.

The reviewer receives the user's messages as the source of intent; the primary agent's account of its own work is not included. On a corrective pass, it also receives the previous findings so it can check the changes made in response.

## Automatic review

With `trigger = "turn"`, every completed prompt-driven turn arms review. Review runs between turns. If prompts are already queued, Mjolnir lets the queue drain and reviews the resulting batch. Preparation resolves the reviewer settings and captures the repository delta; when nothing changed, review resolves without sending a review prompt.

An open review holds new prompts for that session from preparation through a clean or findings verdict. This prevents more edits from racing ahead of work being inspected. A failed review releases the hold immediately, and other sessions remain independent throughout.

## Review before push

With `trigger = "push"`, Claude Code and Codex sessions get the `request_review` tool. The agent is instructed to commit its work and call this tool before `git push` or `gh pr create`. The tool answers when a review starts. The reviewer checks committed changes; tracked working-tree changes must be committed or stashed first. Untracked files are ignored.

The review starts from the last cleanly reviewed commit when that commit is still in the current history and is not older than the merge base with the default branch. Otherwise, it starts from that merge base. Mjolnir uses `origin/HEAD` to find the default branch, then tries `main`, then `master`. If the session is on the default branch, or no default branch is available, it uses the commit that was checked out when the session started.

The verdict arrives later as a message in the agent's conversation. A clean verdict records the reviewed tree. A findings verdict tells the agent to fix the findings, commit the changes, and call `request_review` again. Push review does not hold prompts or send findings as a corrective prompt.

Mjolnir installs a pre-push hook in the session checkout. For pushes made by the agent, the hook refuses any commit tree that has not passed review and prints `mj: <ref> (tree <short>) has not been reviewed.` The hook is enabled only in the harness environment, so pushes from your own terminal are not affected. If the repository already has a pre-push hook, Mjolnir keeps it as `pre-push.before-mj` and runs it after its own check. If `core.hooksPath` points to a directory inside the work tree that Git does not ignore, Mjolnir skips installing the gate and reports that in the transcript. The gate can be skipped with `git push --no-verify`; the agent must follow the request to call `request_review`.

Other harnesses do not receive `request_review` or the push gate. Push mode continues without review, and Mjolnir adds a notice to the transcript.

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

`mj review start` answers once the review has opened, or says why it cannot start. `mj review status` shows the session's trigger, review status, reviewer role, and verdict once it has one, or that no review is open.

## Read and resolve a verdict

The review view provides **Overview**, the reviewer's transcript, and **Verdict**. Use Tab to move among them. The web viewer shows the same reviewer role and status.

Resolution depends on the verdict:

- A **clean** verdict resolves automatically and advances the reviewed boundary.
- A **findings** verdict is forwarded to the primary harness as a corrective prompt. If the primary rejects that prompt, the review stays open with **Forward findings** to retry, **Dismiss** to advance the reviewed boundary without requesting changes, and **Cancel** to close the review without advancing it.
- A **failed** review offers **Dismiss** and **Cancel**. Its prompt hold has already been released, and neither choice advances the reviewed boundary; fix the profile, model, credential, or connectivity problem before trying again.

Cancel is also available while review work is still running. It releases the prompt hold and leaves the unreviewed changes for a later pass.

## Lifecycle behavior

New eligible primary sessions capture a review baseline even when automatic review is off. A session started without a baseline must be resumed or restarted after configuring an eligible reviewer. Child sessions do not capture independent review baselines.

Suspending a session while a review is open preserves its result for reference. The reviewer's worker cannot continue after the target is destroyed. A later review starts a new reviewer child session.

While a review is preparing or running, Mjolnir starts no recovery copy or worker upgrade for that session; a copy already running when the review begins is finished first. Deferred copies and upgrades start as soon as the review closes.

If Mjolnir restarts during a review, it clears the interrupted in-flight marker, releases the prompt hold, and leaves the reviewed boundary unchanged. The next review therefore covers the same changes instead of silently skipping them.

Review traffic is charged through the selected reviewer profile. A different profile ID may still share account-level limits with the primary profile. See [configuration](/configuration/#automatic-review-review) for schema details.

When [automatic continuation](/sessions/#automatic-continuation) is enabled, automatic review waits for its check and any continuation turns to settle. The reviewer then considers the completed chain against your original request; generated continuation prompts do not become new user requirements.
