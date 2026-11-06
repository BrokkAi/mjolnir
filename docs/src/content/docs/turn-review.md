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

An empty reviewer list means no reviewers are configured. A session-level `--reviewer` choice enables review unless that session uses `--review off`. Old `trigger` and `enabled` keys are accepted but dropped when settings are saved.

### Per-session settings

`mj new` and `mj import <harness>` accept `--review off|push` and repeatable `--reviewer MODEL[:EFFORT]` (up to three). `--review off` disables review for that session. `--review push` enables it. `--review turn` is refused. A session-level reviewer choice replaces the global reviewer list. API `review.mode` accepts `off` or `push`; `turn` is refused.

Review requires a committed Git checkout. Tracked changes must be committed before review; untracked files are ignored.

## Request a review

Claude Code, Codex, and DSH sessions receive the `request_review` tool from the separate `mj-review` MCP server. The agent commits its work and calls the tool before pushing. `/review` in Prompt, `mj review start`, and `POST /review` also start a push review of committed work. The verdict arrives as a message to the agent. If a review cannot start, the requester sees the reason. `mj review status` shows the open review and verdict.

Reviewers read the whole change from its review base. They do not receive prior findings for a corrective pass. A findings verdict tells the agent to address findings and request review again.

The review starts from the last cleanly reviewed commit when that commit is in the current history and is not older than the merge base with the default branch. Otherwise, it starts from that merge base. If the agent merges newer default-branch commits into reviewed work, the review covers only conflict resolutions and later edits.

A clean review advances the review base. It records the reviewed tree in `refs/mj/reviewed/<tree>`. The ref follows the repository through Mjolnir checkpoints.

## Overrides and review base

The `override_review` tool takes pairs of findings and reasons for findings the agent will not fix. The agent should record these before committing other fixes. The override allows pushing the exact reviewed commits, but does not move the review base. Mjolnir records overridden trees in `refs/mj/overridden/<tree>` and tells later reviewers not to report declined findings again. The declined list clears when the review base moves.

`set_review_baseline(commit, path?)` moves the review base through a commit whose tree was reviewed or overridden. Later reviews start after that commit. Moving the base clears the declined list. The tools `request_review`, `override_review`, and `set_review_baseline` are provided by `mj-review`.

The pre-push hook accepts trees recorded under either `refs/mj/reviewed/` or `refs/mj/overridden/`. A merge of the default branch does not require reviewing changes already covered by the prior review base.
