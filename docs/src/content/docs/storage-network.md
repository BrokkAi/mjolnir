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
| `config.toml` | Primary/subagent models, ACP policy, review, theme, and spinner preferences |
| Session provenance | Maps resumable session IDs to their original adapter/model route |
| Transcript exports | User-requested Markdown exports |
| DeepSWE cache | Live model-ranking snapshot, refreshed on a time-to-live |
| ACP probe cache | Adapter model/capability results, invalidated by age or binary change |
| ACP registry cache | Public registry metadata used for installable agents |
| Managed agents | Downloaded ACP registry agents |
| Managed runners | Embedded Node.js and uv installations used for `npx` and `uvx` commands |
| Voice cache | Speech-recognition model data downloaded on first dictation use |
| Remote-control state | SQLite session/transcript data, login/cookie material, and certificates |
| `.mjolnir/worktrees/` | Linked Git worktrees created inside a project |

Use `/mjconfig` and normal session/worktree cleanup before deleting files by
hand. Removing provenance does not delete provider-owned ACP sessions; removing
a worktree does not delete remote or provider session records.

## External services

| Service | Why it is contacted |
| --- | --- |
| GitHub | Release installation and update checks |
| DeepSWE/DataCurve | Model ranking refresh |
| ACP registry CDN | Adapter catalog and supported binary downloads |
| npm registry | `npx`-launched ACP bridges and Bifrost discrete-review tooling |
| Node.js / Astral | First-use installation of managed Node.js or uv runners |
| Model providers | Active primary, subagent, and review sessions |
| Voice model hosts | First-use speech model download |
| Tailscale/Let's Encrypt | Optional trusted remote-server certificate issuance |

Network failures normally degrade one route or refresh rather than making every
cached route unavailable. An initial setup with no cached or installed route
can still require network access before any model is launchable.

## Logs

Do not log to stderr while the TUI owns the terminal. Use:

```bash
mj --debug-file /protected/path/mj.log
mj --agent-stderr /protected/path/agent.log
```

Treat both files as sensitive repository context. The environment variables
`BROKK_TUI_LOG` and `BROKK_TUI_AGENT_STDERR` provide the same paths.
