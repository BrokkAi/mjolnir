---
title: Start with Codex
description: What Mjolnir adds around Codex and how to launch the recommended setup.
---

Mjolnir is a self-hosted power frontend for Codex and Claude. Codex remains
the coding agent that plans, edits, runs tools, and answers; Mjolnir supplies
the interface and operating environment around that session. Setting up Claude
instead? See [Start with Claude](/claude/).

## What Mjolnir adds

| Capability | What it changes |
| --- | --- |
| First-class coding teams | Run Codex, Claude, Codex coder + Claude reviewer, or Claude coder + Codex reviewer without changing tools or workflows |
| Integrated adversarial review | Hold a workspace-changing turn while an independent reviewer challenges the diff before the turn is released |
| Self-hosted remote control | Drive the session from another browser or device while the repository and Mjolnir server remain on your machine |
| Worktree-first workflow | Start Codex in a linked Git worktree so agent changes stay separate from your current checkout and easy to inspect |
| Cross-platform voice | Add locally transcribed prompts with Ctrl-R on supported desktop platforms |

Mjolnir, its transcript storage, remote server, and Mjolnir-hosted tools run on
infrastructure you control. Codex model requests still go to OpenAI under the
terms and data boundaries of your Codex account. Mjolnir does not make the
model service itself self-hosted.

## Recommended setup

Authenticate with the Codex CLI supplied by Mjolnir's ACP route:

```bash
npx --yes --package=@agentclientprotocol/codex-acp codex login
```

You do not need a separate global `@openai/codex` installation. Mjolnir runs
the compatible Codex version installed transitively with `codex-acp`.

Then [install Mjolnir](/install/), open a repository, and run:

```bash
mj
```

First launch opens Mjolnir's onboarding on the Team tab. Choose **Codex** to
keep primary, subagent, and review model selection on Auto while constraining
all three seats to the Codex adapter. Then confirm:

1. **OpenAI / ChatGPT** reports that you are signed in.
2. The **Codex** ACP server is detected and enabled.
3. The primary model resolves to a Codex model. Keeping the model on **Auto**
   lets Mjolnir choose among currently launchable ranked Codex models.
4. **Discrete review** is on if you want workspace-changing turns reviewed
   before they are released.

Press **Shift+Tab** during a session to switch among all four teams, or return to
the same choice on the **Team** tab in `/mjconfig`. Start a new session after
switching so the new coder and reviewer routes apply together.

Use `/agents` after launch to record the model and adapter that actually bound
to each seat. The current session keeps those choices until `/new` or `/clear`.

## A safe first session

For change-producing work, start in an isolated linked worktree:

```bash
mj --worktree
```

Try a small request with an observable validation step. Mjolnir will show
Codex's messages, thoughts, tools, permissions, and any subagent or review
activity in one transcript. Run the checked [10-minute evaluation](/evaluate/)
for an end-to-end exercise in a disposable repository.

## Go beyond the terminal

- Press **Ctrl-R** to dictate a prompt on a supported desktop installation.
- Run `mj server` to open the same session through the self-hosted remote
  viewer.
- Ask Codex to launch a subagent for bounded parallel work.
- Run `/review recent` for a findings-only review of the latest
  change-producing turn.
- Run `mj --print` for scripts and machine-readable output.

Continue with [Mjolnir Web](/remote/), [Voice dictation](/voice/), or
[Delegation and adversarial review](/delegation-review/).

## Pair Codex with Claude

The mixed teams are where Mjolnir earns its keep: **Codex coder + Claude
reviewer** keeps Codex in charge of the turn while Claude challenges every
changed turn from an independent session, and the reverse team swaps the
roles. See [Teams and adversarial review](/teams/), or
[Other agents and models](/adapters/) for model resolution details.
