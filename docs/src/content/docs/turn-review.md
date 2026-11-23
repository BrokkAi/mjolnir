---
title: Code review
description: Configure reviewers and review committed work before push.
---

Mjolnir reviews committed work before push whenever reviewers are configured. A reviewer runs as a hidden child session and uses the model and effort selected for its reviewer slot. Open **Reviewers** on the prompt’s bottom border to read its conversation.

## Configure reviewers

**Settings → Code Review** selects up to three reviewer rows. Each row has a model and effort. **Autoconfigure** fills the rows from models offered by enabled profiles. At review time, Mjolnir chooses an enabled profile offering each row's model and effort.

```toml
[review]
[[review.reviewers]]
model = "gpt-6.10-terra"
effort = "xhigh"
```

An empty list means no reviewers are configured. Press **Autoconfigure** in Settings → Code Review to choose reviewers. It chooses the first available combination: Terra, Sol, and Haiku; Terra and Sol; three Haiku; two DeepSeek Flash; Grok; or Kimi K3.1 or newer. It uses the newest advertised model within each family. If no combination is available, choose reviewers in **Settings → Code Review**. A session-level `--reviewer` choice enables review unless that session uses `--review off`.

Old `count`, `profile`, `model`, `effort`, `tier`, `trigger`, and `enabled` config keys remain readable; `trigger` and `enabled` are dropped when settings are saved. An existing config without `[review]`, or a `[review]` table without `reviewers` and `model`, retains the old Auto selection. The daemon saves reviewer rows after capability discovery succeeds; if discovery fails, it retries. Other settings saves preserve this pending selection. A fresh config starts with no reviewers. An explicit `reviewers = []` stays empty.

### Relation brief

A brief writer is an optional extra step before the reviewers start. It is off unless you enable it in `config.toml`:

```toml
[review]
brief_writer = { enabled = true, model = "gpt-6.1-sol", effort = "high" }
```

With the writer enabled, a hidden writer session reads the change and the rest of the repository, then hands back a short list of relations between the changed code and its surroundings: who calls it, what else reads or writes the same state, and which other paths reach it. Each item carries `file:line` anchors. Every reviewer's prompt then includes the list under "Relations computed from the repository", labelled as relations and not findings. `model` is required while the writer is enabled, and it is resolved to an enabled profile like a reviewer model.

The writer never blocks a review. If it cannot start, ends without handing back, or is cancelled with the review, the reviewers start without a brief and the requester sees a notice saying so. The writer's usage counts toward the requesting session like a reviewer's, and the session is closed when the writer is done. A session's own `--reviewer` choice does not change the writer.

The writer is off by default because its effect depends on the codebase. On Mjolnir's own bugs whose cause lies outside the changed code, a GPT-6.1 Sol/high writer raised GPT-6.1 Terra/high reviewers' catch rate from about 20% to about 56%. On another large Rust codebase it lowered that rate, from 17.6% to 7.8%, because its briefs usually missed the relation that mattered. On ordinary changes there, it did not reduce the findings reviewers confirmed. A Terra-written brief carried much less of the lift than a Sol-written one, so use a Sol-class model for the writer. A Sol/high brief costs about $0.20 at API prices and adds the writer's run time before the reviewers start. The measurements are in the repository's review evaluation notes.

### Per-session settings

`mj new` and `mj import <harness>` accept `--review off|push` and repeatable `--reviewer MODEL[:EFFORT]` (up to three). `--review off` disables review for that session. `--review push` enables it. `--review turn` is refused. A session-level reviewer choice replaces the global reviewer list. API `review.mode` accepts `off` or `push`; `turn` is refused.

Review requires a committed Git checkout. Tracked changes must be committed before review; untracked files are ignored.

## Request a review

Claude Code, Codex, and DSH sessions receive the `request_review` tool from the separate `mj-review` MCP server. The agent commits its work and calls the tool before pushing. With no arguments it reviews the repository that contains the session's working directory. `request_review(paths)` reviews the named worktrees together in one review, for example a change that spans two repositories. A session can have several reviews open at once when they cover different worktrees: asking for review of worktree `c` while worktree `a` is under review starts a second review, and each delivers its own verdict. A request that names a worktree an open review already covers is refused, and the refusal says to request the free worktrees now or to wait for that verdict. A session whose worker predates concurrent review keeps one open review at a time. `/review` in Prompt, `mj review start`, and `POST /review` also start a push review of committed work. The verdict arrives as a message to the agent. If a review cannot start, the requester sees the reason. `mj review status` shows the open review and verdict.

Reviewers read the whole change from its review base. They do not receive prior findings for a corrective pass. A findings verdict tells the agent to address findings and request review again.

The review starts from the last cleanly reviewed commit when that commit is in the current history and is not older than the merge base with the default branch. Otherwise, it starts from that merge base. If the agent merges newer default-branch commits into reviewed work, the review covers only conflict resolutions and later edits.

A clean review advances the review base. It records the reviewed tree in `refs/mj/reviewed/<tree>`. The ref follows the repository through Mjolnir checkpoints.

## Overrides and review base

The `override_review` tool takes pairs of findings and reasons for findings the agent will not fix. With more than one verdict outstanding, `path` names a worktree the verdict covers; with only one outstanding, it can be omitted. The agent should record these before committing other fixes. The override allows pushing the exact reviewed commits, but does not move the review base. Mjolnir records overridden trees in `refs/mj/overridden/<tree>` and tells later reviewers not to report declined findings again. The declined list clears when the review base moves.

`set_baseline(commit, path?)` moves the review base through a commit whose tree was reviewed or overridden. Later reviews start after that commit. Moving the base clears the declined list. The tools `request_review`, `override_review`, and `set_baseline` are provided by `mj-review`.

The pre-push hook accepts trees recorded under either `refs/mj/reviewed/` or `refs/mj/overridden/`. It also permits plain documentation changes without review: `.md`, `.markdown`, `.rst`, `.adoc`, and `.asciidoc` files under `docs/`, `doc/`, or `.agents/plans/`, including nested documentation directories. Files in those formats with conventional `README`, `LICENSE`, `COPYING`, `AUTHORS`, `CONTRIBUTORS`, `CHANGELOG`, `NEWS`, `CONTRIBUTING`, `CODE_OF_CONDUCT`, or `SECURITY` name prefixes also qualify, as do root-level extensionless `README`, `LICENSE`, `COPYING`, `AUTHORS`, `CONTRIBUTORS`, `CHANGELOG`, and `NEWS` files. Markdown elsewhere remains gated because applications can embed it as runtime prompts or policies. Executables, symbolic links, binary files, scripts, configuration, lockfiles, and MDX still require review. Known agent instruction files such as `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, and `SKILL.md`, prompt and instruction Markdown files, Markdown directly under `.agents` (including `PLANS.md`), and files under `.agents/skills`, `.github`, `.claude`, `.codex`, `.gemini`, `.cursor`, and `.opencode` remain subject to review.

The documentation exemption compares the entire pushed tree with the destination's previous commit or a reviewed ancestor that includes the destination's history. For a new branch, a locally recorded published ancestor on the same remote can also provide the comparison point when the fetch URL matches the push destination. The hook uses that local history without another network request. A documentation commit on top of unreviewed code does not qualify. A new repository containing only documentation also qualifies. Existing user pre-push hooks still run for exempt pushes, and exemptions do not mark trees as reviewed. A merge of the default branch does not require reviewing changes already covered by the prior review base.

## Follow a review

The parent conversation and its prompt remain available while reviewers work. A short status appears above the prompt. Open **Reviewers** to inspect reviewer sessions in the virtual workspace; close the workspace or press Escape in the Sessions list to return. The web viewer works the same way: **Reviewers · n**, with the number of reviewers still open, sits beside **Subagents · n** once a session has any reviewer, and opens the same list filtered to reviewers. **Subagents** lists only the other children.

A clean review advances the reviewed boundary automatically. Findings arrive as a message to the primary agent, and a failed review reports its reason. `mj review status` shows the current state and verdict. `mj review show` reads a finished review after it closes, including each reviewer's model, effort, profile, full handback, verdict, and usage. Reviewer sessions are destroyed when their role ends; the review's output does not depend on them and remains readable with `mj review show --session <id> [--review <id>]` and `GET /sessions/{id}/reviews`.

## Lifecycle behavior

New eligible primary sessions capture a review baseline even when automatic review is off. A session is eligible when an enabled profile can serve review choices. A session started without a baseline must be resumed or restarted after configuring an eligible reviewer. Child sessions do not capture independent review baselines.

Suspending a session while a review is open preserves its result for reference. The reviewer's worker cannot continue after the target is destroyed. A later review starts a new reviewer child session.

While a review is preparing or running, Mjolnir starts no recovery copy or worker upgrade for that session; a copy already running when the review begins is finished first. Deferred copies and upgrades start as soon as the review closes.

If Mjolnir restarts during a review, the reviewer workers keep running. The replacement daemon restores the captured target, reviewer sessions, and completed results, then continues the group. A repeated result does not duplicate findings or approve the tree twice. Reviews interrupted before their group could be saved remain unreviewed rather than advancing coverage.

Review traffic is charged through the selected reviewer profile. A different profile ID may still share account-level limits with the primary profile. See [configuration](/configuration/#automatic-review-review) for schema details.
