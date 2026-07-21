---
title: Permissions and workspace scope
description: Understand interactive and headless permission modes, workspace roots, and adapter-owned tools.
---

Permissions answer whether a requested action may run. They do not establish
that the action is correct, necessary, or safe.

## Two defaults

| Context | Default | Behavior |
| --- | --- | --- |
| Interactive Council | `auto` | Applies Mjolnir's interactive Council permission policy and still surfaces requests that need a decision |
| Headless `--print` | `manual` | Rejects permission requests so an unattended process cannot hang |

Headless modes:

- `manual` rejects all prompts.
- `auto` accepts supported file edit/delete/move prompts but rejects shell execution.
- `yolo` accepts every permission prompt and should only be used inside a
  disposable, tightly scoped environment.

Legacy names `default`, `acceptEdits`, and `bypassPermissions` remain aliases.

## Workspace roots

`--cwd` is the primary workspace. Repeat `--additional-directory` (or
`--add-dir`) to expose more absolute directories:

```bash
mj --cwd /work/app --additional-directory /work/shared
```

Additional roots widen the filesystem and terminal scope available to
Mjolnir-hosted ACP tools. They do not mark content as trusted and do not grant a
model blanket permission to change it.

Mjolnir canonicalizes roots and constrains its hosted filesystem and terminal
requests to them. Agent-owned tools can have provider- or adapter-owned policy
that Mjolnir does not replace. Custom ACP servers inherit the environment and
run from the workspace directory.

## Nested Council requests

Thor calls Eitri through a local authenticated MCP server. Nested permission IDs
are namespaced so the active Eitri request can be answered without confusing it
with Thor's foreground session. The same identity is preserved through the
remote viewer.

## Safe automation checklist

1. Use a disposable repository or `mj --worktree` for evaluation.
2. Keep `manual` for read-only headless work.
3. Use `auto` only when file changes are expected and shell execution should stay blocked.
4. Avoid `yolo` on personal or production directories.
5. Limit additional roots to the minimum required scope.
6. Capture `--debug-file` and `--agent-stderr` only to protected paths; logs can contain repository context even though Mjolnir avoids logging API-key values.

Continue with [Data and trust boundaries](/data-boundaries/) before connecting
private source or exposing remote control.
