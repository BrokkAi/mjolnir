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
| `--thor MODEL` | Override Thor for one headless invocation |
| `--loki MODEL\|disabled` | Override or disable Loki for one headless invocation |
| `--eitri MODEL\|disabled` | Override or disable Eitri for one headless invocation |
| `-w, --worktree [NAME]` | Create or reuse a linked worktree |
| `--fullscreen-tui` | Use the alternate-screen UI instead of inline mode |
| `--debug-file PATH` | Write Mjolnir diagnostics without corrupting the TUI |
| `--agent-stderr PATH` | Capture ACP adapter stderr |
| `--no-update-check` | Skip the startup release check |
| `--anvil-path PATH` | Use a development Anvil binary |

## Subcommands

```bash
mj resume [SESSION_ID]
mj resume --list --format json --cwd /work/project
mj server [--hostname HOST | --tailscale]
```

See [Sessions, worktrees, and resume](/sessions-worktrees/) and [Remote
control](/remote/) for behavioral boundaries.

## Useful slash commands

| Command | Purpose |
| --- | --- |
| `/mjconfig` | Edit Council, accounts, ACP servers, review, and appearance |
| `/models` | Open configuration on the Council model tab |
| `/council` | Show the active role, model, adapter, permission, and usage state |
| `/review` | Choose a recent, uncommitted, or HEAD findings-only review |
| `/review recent` | Review the latest change-producing turn |
| `/review uncommitted` | Review all current worktree changes |
| `/review head` | Review `HEAD` |

The interactive autocomplete is the source of truth for commands in the
installed version.

## Keyboard basics

- Enter sends a prompt or accepts the selected action.
- Up/Down navigate autocomplete and permission choices.
- PageUp/PageDown scroll the transcript.
- F1–F9 edit visible Thor session controls.
- F10 toggles help.
- Esc dismisses autocomplete, clears input, or cancels a permission prompt.
- Ctrl-C cancels the active Thor or Eitri request; idle Ctrl-C quits.
- Ctrl-D quits when input is empty.
- Ctrl-R starts or stops microphone dictation when the voice worker is available.

Long permission commands, descriptions, and options must remain reachable; a
truncated prompt is a UI bug, not an instruction to guess.
