---
title: Start with Claude
description: What Mjolnir adds around Claude and how to launch the recommended setup.
---

Mjolnir is a self-hosted power frontend for Claude and Codex. Claude remains
the coding agent that plans, edits, runs tools, and answers; Mjolnir supplies
the interface and operating environment around that session. Setting up Codex
instead? See [Start with Codex](/codex/).

## What Mjolnir adds

| Capability | What it changes |
| --- | --- |
| First-class coding teams | Run Claude, Codex, Claude coder + Codex reviewer, or Codex coder + Claude reviewer without changing tools or workflows |
| Integrated adversarial review | Hold a workspace-changing turn while an independent reviewer challenges the diff before the turn is released |
| Self-hosted remote control | Drive the session from another browser or device while the repository and Mjolnir server remain on your machine |
| Worktree-first workflow | Start Claude in a linked Git worktree so agent changes stay separate from your current checkout and easy to inspect |
| Cross-platform voice | Add locally transcribed prompts with Ctrl-R on supported desktop platforms |

Mjolnir, its transcript storage, remote server, and Mjolnir-hosted tools run on
infrastructure you control. Claude model requests still go to Anthropic under
the terms and data boundaries of your Claude account. Mjolnir does not make the
model service itself self-hosted.

## Recommended setup

After [installing Mjolnir](/install/), open a repository and run:

```bash
mj
```

Open the onboarding **ACP Servers** tab, select **Anthropic / Claude**, and
choose **Claude subscription**. Mjolnir launches the Claude Code executable
supplied by the ACP route and returns to onboarding after sign-in.

The equivalent manual command is:

```bash
npx -y @agentclientprotocol/claude-agent-acp --cli auth login --claudeai
```

You do not need a separate global `@anthropic-ai/claude-code` installation.
Mjolnir detects the credentials written by this flow and runs the compatible
Claude Code version installed transitively with `claude-agent-acp`.

First launch opens Mjolnir's onboarding on the Team tab. Choose **Claude** to
keep primary, subagent, and review model selection on Auto while constraining
all three seats to the Claude adapter. Then confirm:

1. **Anthropic / Claude** reports that you are signed in.
2. The **Claude** ACP server is detected and enabled.
3. The primary model resolves to a Claude model. Keeping the model on **Auto**
   lets Mjolnir choose among currently launchable ranked Claude models.
4. **Discrete review** is on if you want workspace-changing turns reviewed
   before they are released.

Press **Ctrl+Tab** during a session to switch among all four teams, or return to
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
Claude's messages, thoughts, tools, permissions, and any subagent or review
activity in one transcript. Run the checked [10-minute evaluation](/evaluate/)
for an end-to-end exercise in a disposable repository.

## Go beyond the terminal

- Press **Ctrl-R** to dictate a prompt on a supported desktop installation.
- Run `mj server` to open the same session through the self-hosted remote
  viewer.
- Ask Claude to launch a subagent for bounded parallel work.
- Run `/review recent` for a findings-only review of the latest
  change-producing turn.
- Run `mj --print` for scripts and machine-readable output.

Continue with [Remote control](/remote/), [Voice dictation](/voice/), or
[Delegation and adversarial review](/delegation-review/).

## Pair Claude with Codex

The mixed teams are where Mjolnir earns its keep: **Claude coder + Codex
reviewer** keeps Claude in charge of the turn while Codex challenges every
changed turn from an independent session, and the reverse team swaps the
roles. See [Teams and adversarial review](/teams/), or
[Other agents and models](/adapters/) for model resolution details.
