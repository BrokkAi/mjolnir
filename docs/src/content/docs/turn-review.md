---
title: Turn review
description: Configure independent reviewers and resolve findings between agent turns.
---

Mjolnir can review work at the end of each turn or before an agent pushes commits. Choose the trigger in **Settings → Code Review** or in `config.toml`. A reviewer runs as a hidden child session of the session it reviews. It uses the model and effort selected for its reviewer slot. It does not appear in the agent's `list_agents` results or count against the child-session limit. Open **Reviewers** on the prompt’s bottom border to read its conversation in a virtual workspace.

## Configure reviewers

**Settings → Code Review** selects the trigger and up to three reviewer rows. Each row has a model and effort. **Autoconfigure** fills the rows from models offered by enabled profiles. At review time, Mjolnir chooses an enabled profile offering each row's model and effort through the same load balancer used for subagents.

```toml
[review]
trigger = "turn"

[[review.reviewers]]
model = "gpt-6.10-terra"
effort = "xhigh"

[[review.reviewers]]
model = "gpt-6.1-sol"
effort = "medium"
```

An empty list means no reviewers are configured. Press **Autoconfigure** in Settings → Code Review to choose reviewers. It chooses the first available combination: Terra, Sol, and Haiku; Terra and Sol; two Haiku; two DeepSeek Flash; Grok; or Kimi K3.1 or newer. It uses the newest advertised model within each family. If no combination is available, choose reviewers in **Settings → Code Review**.

Old `count`, `profile`, `model`, `effort`, `tier`, and `enabled` config keys remain readable. An existing config without `[review]`, or a `[review]` table without `reviewers` and `model`, retains the old Auto selection. The daemon saves reviewer rows after capability discovery succeeds; if discovery fails, it retries. Other settings saves preserve this pending selection. A fresh config starts with no reviewers. An explicit `reviewers = []` stays empty.

### Per-session settings

`mj new` and `mj import <harness>` accept `--review off|turn|push` and repeatable `--reviewer MODEL[:EFFORT]` (up to three). The reviewer list replaces `[review]` for that session. Without these flags, the session follows `[review]`.

Review needs a committed Git checkout to capture changes. A local bare session
whose directory is not a committed checkout is created with review off for that
session. The New Session review page and `mj new` say so, `/review status`
gives the reason, and `/review` reports that it cannot capture the directory's
changes.

## What the reviewer reads

Each reviewer follows the Codex `/review` rubric and answers in plain text. The exact reply `No findings.` means that reviewer found no issues. Any other non-empty reply is findings, and an empty reply is a failed review.

Reviewer sessions inspect the same captured change in parallel using their independently resolved models and profiles. The primary receives every reviewer's findings, labeled with its session. Reports may contain redundant findings; Mjolnir keeps them separate. Multiple clean results become one clean verdict only after every reviewer finishes cleanly. If a reviewer fails, completed findings still reach the primary, but the review remains incomplete and coverage does not advance.

The initial prompt includes the user's messages and a per-file diffstat, not the full diff. For each repository it gives the reviewer the root, the baseline tree, the captured tree, and the command:

```text
git -C <root> diff --no-ext-diff --ignore-submodules=none <baseline_tree> <capture_tree>
```

The reviewer runs that command itself with its harness's shell or read tools. The tree IDs keep the review scoped to the captured changes, even if files change later. Repository contents and tool output are treated as untrusted data, not instructions.

The reviewer receives the user's messages as the source of intent; the primary agent's account of its own work is not included. On a corrective pass, it also receives the previous findings so it can check the changes made in response.

## Automatic review

With `trigger = "turn"`, every completed prompt-driven turn arms review. Review starts after a turn and runs in the background. New prompts remain available while reviewers inspect the captured change. Preparation resolves the reviewer settings and captures the repository delta; when nothing changed, review resolves without sending a review prompt.

The reviewer examines a captured change while the primary conversation stays available. Later edits are covered by a subsequent review.

## Review before push

With `trigger = "push"`, Claude Code, Codex, and DSH sessions get the `request_review` tool. The agent is instructed to commit its work and call this tool before `git push` or `gh pr create`. The tool answers when a review starts. The reviewer checks committed changes; tracked working-tree changes must be committed or stashed first. Untracked files are ignored.

The review starts from the last cleanly reviewed commit when that commit is still in the current history and is not older than the merge base with the default branch. Otherwise, it starts from that merge base. Mjolnir looks for the default branch in this order: `origin/HEAD`, `origin/main`, `origin/master`, local `main`, then local `master`. If the session is on the default branch, or no default branch is available, it uses the commit that was checked out when the session started. If the agent merges newer default-branch commits into reviewed work, the review starts from Git's merge of the last reviewed commit with those commits. The reviewer then sees only conflict resolutions and later edits. Where a merge conflicted, the base contains conflict markers. A binary, modify/delete or submodule conflict cannot hold markers; if it was resolved to the version Git's merge chose, the file is left out of the base so the reviewer sees all of it. If the committed tree is the same Git tree as its base, Mjolnir records a clean review without starting a reviewer.

Reviewing merged work requires Git 2.39.5 or newer. With older Git, Mjolnir records that push without review and shows a warning in the session conversation. Upgrade Git to review merged work.

The verdict arrives later as a message in the agent's conversation. The group is clean only when every reviewer finishes cleanly; a failed or missing reviewer keeps the gate blocked. A clean verdict records the reviewed tree in the Git ref `refs/mj/reviewed/<tree>`. A move keeps every ref under `refs/mj/`: these records, the session's start commit and the turn-review baseline. A managed checkout's checkpoint carries them with their objects. A raw session's checkpoint records their names and object IDs, and restoring it recreates each ref whose object the repository still has. A findings verdict tells the agent to fix the findings, commit the changes, and call `request_review` again. Push review does not hold prompts or send findings as a corrective prompt.

If the agent judges the findings incorrect or unreasonable, it can call `override_review` with a reason. The override applies to the whole findings verdict and only to the same reviewed trees captured for that verdict. Before the reviewed refs are unlocked, Mjolnir records an “override requested” notice in the session transcript with the reason and findings, so that record remains visible even if a later operation fails. An override is refused while a review is running or when a reviewer failed and the group is incomplete.

Mjolnir installs a pre-push hook in the session checkout. For pushes made by the agent, the hook refuses any commit tree that has not passed review or been accepted through a recorded override, and prints `mj: <ref> (tree <short>) has not been reviewed.` The hook is enabled only in the harness environment, so pushes from your own terminal are not affected. If the repository already has a pre-push hook, Mjolnir keeps it as `pre-push.before-mj` and runs it after its own check. If `core.hooksPath` points to a directory inside the work tree that Git does not ignore, Mjolnir skips installing the gate and reports that in the transcript. The gate can be skipped with `git push --no-verify`; the agent must follow the request to call `request_review`.

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

## Follow a review

The parent conversation and its prompt remain available while reviewers work. A short status appears above the prompt. Open **Reviewers** to inspect reviewer sessions in the virtual workspace; close the workspace or press Escape in the Sessions list to return. The web viewer works the same way: **Reviewers · n**, with the number of reviewers still open, sits beside **Subagents · n** once a session has any reviewer, and opens the same list filtered to reviewers. **Subagents** lists only the other children.

A clean review advances the reviewed boundary automatically. Findings are sent to the primary agent as a corrective prompt, and a failed review reports its reason. `mj review status` shows the current state and verdict.

## Lifecycle behavior

New eligible primary sessions capture a review baseline even when automatic review is off. A session is eligible when an enabled profile can serve review choices. A session started without a baseline must be resumed or restarted after configuring an eligible reviewer. Child sessions do not capture independent review baselines.

Suspending a session while a review is open preserves its result for reference. The reviewer's worker cannot continue after the target is destroyed. A later review starts a new reviewer child session.

While a review is preparing or running, Mjolnir starts no recovery copy or worker upgrade for that session; a copy already running when the review begins is finished first. Deferred copies and upgrades start as soon as the review closes.

If Mjolnir restarts during a review, the reviewer workers keep running. The replacement daemon restores the captured target, reviewer sessions, and completed results, then continues the group. A repeated result does not duplicate findings or approve the tree twice. Reviews interrupted before their group could be saved remain unreviewed rather than advancing coverage.

Review traffic is charged through the selected reviewer profile. A different profile ID may still share account-level limits with the primary profile. See [configuration](/configuration/#automatic-review-review) for schema details.

When [automatic continuation](/sessions/#automatic-continuation) is enabled, automatic review waits for its check and any continuation turns to settle. The reviewer then considers the completed chain against your original request; generated continuation prompts do not become new user requirements.
