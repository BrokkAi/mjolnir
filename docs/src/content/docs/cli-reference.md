---
title: CLI and keyboard reference
description: Common commands, options, slash commands, and terminal controls.
---

## Common CLI options

| Option | Purpose |
| --- | --- |
| `--cwd PATH` | Primary workspace; defaults to the current directory |
| `--additional-directory PATH` | Add an absolute workspace root; repeatable; alias `--add-dir` |
| `-p, --print [PROMPT]` | Run one headless prompt; omit the value or pass `-` for stdin |
| `--output-format text\|json\|stream-json` | Select headless output |
| `--permission-mode manual\|auto\|yolo` | Set headless permission behavior |
| `--model MODEL[+EFFORT]` | Override the primary agent's model for one headless invocation |
| `--review-model MODEL[+EFFORT]` | Override the review supervisor model for one headless invocation |
| `--subagent-model MODEL[+EFFORT]\|disabled` | Override or disable the default subagent model for one headless invocation |
| `-w, --worktree [NAME]` | Create or reuse a linked worktree |
| `--fullscreen-tui` | Use the alternate-screen UI instead of inline mode |
| `--debug-file PATH` | Write Mjolnir diagnostics without corrupting the TUI |
| `--agent-stderr PATH` | Capture ACP adapter stderr |
| `--no-update-check` | Skip the startup release check |

`--model`, `--review-model`, and `--subagent-model` require `--print` and
explicit model IDs. The optional `+EFFORT` suffix (`off`, `none`, `minimal`,
`low`, `medium`, `high`, `xhigh`) sets that seat's ACP reasoning effort.

## Subcommands

```bash
mj resume [SESSION_ID]
mj resume --list --format json --cwd /work/project
mj models refresh
mj memory list
mj memory add [--global] "one short fact"
mj memory forget m7
mj memory clear --yes
mj app [--history-days DAYS]
mj server [--hostname HOST] [--no-tailscale-detect] [--port PORT]
```

See [Sessions, worktrees, and resume](/sessions-worktrees/) and [Mjolnir
Web](/remote/) for behavioral boundaries.

## Useful slash commands

| Command | Purpose |
| --- | --- |
| `/mjconfig` | Configure agents, ACP servers, and appearance |
| `/diff` | Show workspace changes against `HEAD` |
| `/agents` | Show the active model selections and per-seat usage |
| `/discrete-review` | Choose a target for the configured discrete-review tier |
| `/discrete-review <recent\|uncommitted\|head> [quick\|extended]` | Run an on-demand discrete-review pass, optionally overriding the configured tier |
| `/adversarial-review …` | Alias for `/discrete-review …` |
| `/memory` | List stored memories and the use/generate toggles |
| `/memory add [--global] <text>` | Save a memory for this project (or globally) |
| `/compact` | Compact the primary agent's session where supported |
| `/subagents` | Open the session-wide actor roster and its retained transcripts |
| `/exit` | Quit Mjolnir; in a side conversation, return to the primary conversation |

The interactive autocomplete is the source of truth for commands in the
installed version.

## Keyboard basics

- Enter sends a prompt or accepts the selected action. During a primary turn,
  it queues the prompt FIFO behind that turn.
- Up/Down navigate autocomplete and permission choices.
- PageUp/PageDown scroll the transcript.
- Shift+Tab (or Ctrl+Tab in terminals that can send it) switches between the
  four Codex and Claude coder/reviewer teams. Models and reasoning effort are
  configured per seat in `/mjconfig`.
- Typing `@` opens workspace file autocomplete; the chosen file is attached to
  the prompt as an ACP resource link.
- Alt+Up (or Shift+Left) pulls the newest queued prompt back into the composer
  for editing.
- F10 toggles help.
- Esc dismisses autocomplete, clears input, or cancels a permission prompt.
- Ctrl-C steers the oldest queued prompt into the active turn when the agent
  supports steering; otherwise it cancels the active turn together with every
  running subagent. On an idle, empty prompt it quits.
- Ctrl-D quits when input is empty.
- Ctrl-R starts or stops microphone dictation when the voice worker is available.

## Mid-turn steering

Use either composer to correct or redirect an agent without losing your place
in its work. While its primary turn is streaming, press Enter (or **Send** in
Mjolnir Web) to place the instruction in the FIFO queue. Then press Ctrl-C
(or **Stop** in Mjolnir Web). When the agent advertises steering, Mjolnir
removes the oldest queued instruction and injects it into the current turn;
the message appears in the transcript immediately and the turn keeps running.

Without steering support, Ctrl-C or Stop performs a normal cancellation and
the queued instruction runs after the turn finishes. The terminal and Mjolnir
Web use this same queue-first behavior.

Long permission commands, descriptions, and options must remain reachable; a
truncated prompt is a UI bug, not an instruction to guess.
