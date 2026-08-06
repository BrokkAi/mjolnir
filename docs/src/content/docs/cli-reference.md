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
mj server [--hostname HOST | --tailscale]
```

See [Sessions, worktrees, and resume](/sessions-worktrees/) and [Remote
control](/remote/) for behavioral boundaries.

## Useful slash commands

| Command | Purpose |
| --- | --- |
| `/mjconfig` | Configure agents, ACP servers, and appearance |
| `/agents` | Show the active model selections and per-seat usage |
| `/review` | Choose a recent, uncommitted, or HEAD findings-only review |
| `/review recent` | Review the latest change-producing turn |
| `/review uncommitted` | Review all current worktree changes |
| `/review head` | Review `HEAD` |
| `/compact` | Compact the primary agent's session where supported |

The interactive autocomplete is the source of truth for commands in the
installed version.

## Keyboard basics

- Enter sends a prompt or accepts the selected action.
- Up/Down navigate autocomplete and permission choices.
- PageUp/PageDown scroll the transcript.
- Ctrl+Tab switches between the four Codex and Claude coder/reviewer teams.
- Shift+Tab changes the current agent's model and effort.
- F10 toggles help.
- Esc dismisses autocomplete, clears input, or cancels a permission prompt.
- Ctrl-C cancels the active turn together with every running subagent; on an
  idle, empty prompt it quits.
- Ctrl-D quits when input is empty.
- Ctrl-R starts or stops microphone dictation when the voice worker is available.

Long permission commands, descriptions, and options must remain reachable; a
truncated prompt is a UI bug, not an instruction to guess.
