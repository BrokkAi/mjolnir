---
title: Data and trust boundaries
description: What Mjolnir reads, stores, downloads, and can send to providers.
---

Mjolnir is local software, but a normal Council session is not necessarily
offline. Provider agents can receive prompts, source, diffs, tool results, and
transcript context. Their retention, training, residency, and account policies
are separate from Mjolnir's license and local controls.

## Boundary map

| Component | Reads or receives | Writes or sends |
| --- | --- | --- |
| Mjolnir UI/runtime | User input, ACP events, workspace paths, config, session metadata | Terminal rendering, local state, provider-bound ACP prompts |
| Provider ACP adapter | Credentials/environment, prompts, source/tool context | Provider API requests, ACP messages and tool requests |
| Mjolnir filesystem/terminal tools | Files and commands under active workspace roots | Approved file and process effects under those roots |
| Eitri MCP server | Thor's authenticated local tool calls | Nested implementation/exploration sessions and results |
| Custom ACP server | Inherited environment, workspace cwd, ACP messages | Server-defined network, file, and provider behavior |
| Remote server | Viewer login, prompts, permissions, transcript activity | Local SQLite/session state, cookies, TLS material, downloads |
| Voice worker | Microphone audio after Ctrl-R activation | Local model cache and transcribed prompt text |

## Workspace scope is not trust

The primary `--cwd` and any `--additional-directory` values define active
workspace roots for Mjolnir-hosted file and terminal operations. Adding a root
widens access; it does not assert that files or instructions inside it are safe.

Agent-owned tools can follow adapter-specific permission policy. A custom ACP
server runs in the workspace and inherits Mjolnir's environment unless you
launch it through a wrapper that constrains that environment.

## Credentials

Discovery can inspect supported provider credential files and token environment
variables. Mjolnir does not intentionally log API-key values. Spawned adapters
still need access to the credentials required for their provider, and custom
servers inherit the environment.

Use `--debug-file` and `--agent-stderr` only with protected paths. Diagnostic
output can contain repository paths, provider errors, commands, or other
sensitive context even when secrets are redacted.

## Network and downloads

A normal lifecycle can contact:

- GitHub release APIs and assets for install and update checks;
- the DeepSWE ranking endpoint;
- the public ACP registry and agent archives;
- npm for Codex and Claude ACP bridges;
- provider model APIs through the selected adapter;
- Anvil release assets for a managed runtime; and
- model hosts for the optional voice model on first use.

First launch may therefore take longer and use more bandwidth than subsequent
cached launches. Ctrl-R dictation downloads roughly 0.7 GB on first use.

## Remote exposure

`mj server` is loopback-only by default. `--hostname` and `--tailscale` change
the network boundary. Remote state can include transcripts, queued prompts,
permission decisions, authentication tokens/cookies, certificates, and local
session metadata. Read [Remote control](/remote/) before leaving loopback.

## Private-repository checklist

1. Choose providers whose data terms match the repository.
2. Confirm the actual Council with `/council`; optional roles can create additional provider calls and cost.
3. Limit workspace roots and use a worktree for change-producing evaluation.
4. Review every permission request and avoid unattended `yolo` mode.
5. Inspect custom ACP commands and inherited environment values.
6. Decide whether remote control and voice downloads are acceptable.
7. Protect or periodically clear local transcripts, provenance, caches, managed agents, and worktrees according to your policy.

For exact local categories, continue with [Storage and network
activity](/storage-network/).
