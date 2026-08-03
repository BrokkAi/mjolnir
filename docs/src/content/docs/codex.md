---
title: Start with Codex
description: What Mjolnir adds around Codex and how to launch the recommended setup.
---

Mjolnir is a self-hosted power frontend for Codex. Codex remains the coding
agent that plans, edits, runs tools, and answers; Mjolnir supplies the interface
and operating environment around that session.

## What Mjolnir adds

| Capability | What it changes |
| --- | --- |
| Native terminal | Inline streaming, readable permissions, tool activity, session controls, and keyboard-driven configuration |
| Remote control | Drive the session from another browser or device while the repository and Mjolnir server remain on your machine |
| Voice dictation | Add locally transcribed prompts with Ctrl-R on supported desktop platforms |
| Parallel subagents | Let Codex launch focused background sessions and receive their reports without polling |
| Adversarial review | Hold a delegated, workspace-changing turn while a separate supervisor challenges the diff and vets targeted specialist findings |
| Durable work | Resume the original Codex route and isolate changes in linked Git worktrees |
| Automation | Run the same setup headlessly with text, JSON, or newline-delimited stream output |

Mjolnir, its transcript storage, remote server, and Mjolnir-hosted tools run on
infrastructure you control. Codex model requests still go to OpenAI under the
terms and data boundaries of your Codex account. Mjolnir does not make the
model service itself self-hosted.

## Recommended setup

Install and authenticate the official Codex CLI first:

```bash
npm install -g @openai/codex
codex login
```

Then [install Mjolnir](/install/), open a repository, and run:

```bash
mj
```

First launch opens Mjolnir's configuration screen. Confirm:

1. **OpenAI / ChatGPT** reports that you are signed in.
2. The **Codex** ACP server is detected and enabled.
3. The primary model resolves to a Codex model. Keeping the model on **Auto**
   lets Mjolnir choose among the currently launchable ranked models.
4. **Discrete review** is on if you want delegated workspace changes reviewed
   before the turn is released.

Return to the same settings later with `/mjconfig`.

Use `/agents` after launch to record the model and adapter that actually bound
to each seat. The current session keeps those choices until `/new` or `/clear`.

## A safe first session

For change-producing work, start in an isolated linked worktree:

```bash
mj --worktree
```

Try a small request with an observable validation step. Mjolnir will show
Codex's messages, thoughts, tools, permissions, and any subagent or review
activity in one transcript. Run the checked [10-minute Codex evaluation](/evaluate/)
for an end-to-end exercise in a disposable repository.

## Go beyond the terminal

- Press **Ctrl-R** to dictate a prompt on a supported desktop installation.
- Run `mj server` to open the same session through the self-hosted remote
  viewer.
- Ask Codex to launch a subagent for bounded parallel work.
- Run `/review recent` for a findings-only review of the latest
  change-producing turn.
- Run `mj --print` for scripts and machine-readable output.

Continue with [Remote control](/remote/), [Voice dictation](/voice/), or
[Delegation and adversarial review](/delegation-review/).

## Add other agents when useful

Codex is the recommended primary, not a lock-in boundary. Claude, Kimi, Anvil,
and custom ACP servers can be added as alternative primaries or specialist
subagent and review routes. See [Other agents and models](/adapters/) after the
Codex path is working.
