---
title: Sessions, worktrees, and resume
description: Isolate a coding session and return through its original ACP route.
---

## Create an isolated worktree

From a Git repository:

```bash
mj --worktree
```

Mjolnir creates a linked worktree under
`<project>/.mjolnir/worktrees/<adjective-noun>`. Pass a name to reuse an existing
Mjolnir worktree:

```bash
mj --worktree quiet-forge
```

Freshly created worktrees offer cleanup when the TUI exits. Reused worktrees do
not, because the explicit name indicates continued ownership. Mjolnir prints the
path before cleanup decisions so it is not lost when terminal state changes.

## Resume a session

`mj resume` opens a searchable picker. A session ID resumes directly:

```bash
mj resume <session-id>
```

Saved provenance pins the session back to its original ACP adapter and model
when that route is still launchable. It does not automatically infer a prior
worktree directory. Reuse it explicitly:

```bash
mj resume <session-id> --worktree quiet-forge
```

Mjolnir prints a complete resume hint after an interactive session.

## List sessions

```bash
mj resume --list
mj resume --list --cwd /work/project --format json
```

Legacy session IDs can be ambiguous across adapters. If direct resume cannot
identify one route, use the interactive picker first.

Session provenance is persisted separately from provider-owned session data.
Read [Storage and network activity](/storage-network/) before deleting state or
moving a worktree.
