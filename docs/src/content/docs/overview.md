---
title: Overview
description: What Mjolnir is and how the Council fits together.
---

Mjolnir (`mj`) is a native Rust [Agent Client Protocol](https://agentclientprotocol.com/) client built around a model-first coding Council. It keeps transcripts, permissions, tools, terminals, and session handling consistent while locally available ACP adapters remain an implementation detail.

## Council architecture

```text
user
  │
  ▼
Thor ───── delegates ─────▶ Eitri
  │                           │
  ├──── coordinates           └──── returns implementation + diff
  │
  └──── checkpoints ───────▶ Loki ───── returns review advice
```

## Sessions and worktrees

Mjolnir can create an isolated linked Git worktree for a session and records enough provenance to resume through the original adapter and model.

```bash
mj --worktree
mj resume <session-id> --worktree quiet-forge
```

These pages are an initial documentation structure. The [project README](https://github.com/BrokkAi/mjolnir#readme) remains the complete current reference.
