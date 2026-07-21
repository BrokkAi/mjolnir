---
title: Storage and network activity
description: Persistent state, caches, managed binaries, worktrees, and external endpoints.
---

Mjolnir uses platform config, data, state, and cache directories through the
operating system. Linux examples commonly appear below `~/.config`,
`~/.local/share`, and `~/.cache`; macOS and Windows use their platform
equivalents.

## Persistent categories

| Category | Purpose |
| --- | --- |
| `config.toml` | Council, ACP policy, permissions, review, theme, and spinner preferences |
| Session provenance | Maps resumable session IDs to their original adapter/model route |
| Transcript exports | User-requested Markdown exports |
| DeepSWE cache | Live model-ranking snapshot, refreshed on a time-to-live |
| ACP probe cache | Adapter model/capability results, invalidated by age or binary change |
| ACP registry cache | Public registry metadata used for installable agents |
| Managed agents | Downloaded Kimi/registry agents and the managed Anvil runtime |
| Voice cache | Speech-recognition model data downloaded on first dictation use |
| Remote-control state | SQLite session/transcript data, login/cookie material, and certificates |
| `.mjolnir/worktrees/` | Linked Git worktrees created inside a project |

Use `/mjconfig` and normal session/worktree cleanup before deleting files by
hand. Removing provenance does not delete provider-owned ACP sessions; removing
a worktree does not delete remote or provider session records.

## External services

| Service | Why it is contacted |
| --- | --- |
| GitHub | Release installation, update checks, managed Anvil assets |
| DeepSWE/DataCurve | Model ranking refresh |
| ACP registry CDN | Adapter catalog and supported binary downloads |
| npm registry | `npx`-launched Codex and Claude ACP bridges |
| Model providers | Active Thor, Eitri, and Loki sessions |
| Voice model hosts | First-use speech model download |
| Tailscale/Let's Encrypt | Optional trusted remote-server certificate issuance |

Network failures normally degrade one route or refresh rather than making every
cached route unavailable. An initial setup with no cached or installed route
can still require network access before a Council is launchable.

## Logs

Do not log to stderr while the TUI owns the terminal. Use:

```bash
mj --debug-file /protected/path/mj.log
mj --agent-stderr /protected/path/agent.log
```

Treat both files as sensitive repository context. The environment variables
`BROKK_TUI_LOG` and `BROKK_TUI_AGENT_STDERR` provide the same paths.
