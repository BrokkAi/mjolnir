---
title: Remote control
description: Control the Codex session on your machine from a browser with explicit network and session boundaries.
---

`mj server` starts Mjolnir's self-hosted remote-control server with the same
resolved Codex model and subagent configuration as the terminal client. The
repository and agent processes stay on the host running Mjolnir; the browser is
a control surface for that session.

## Default local server

```bash
mj server
```

The default listens on loopback HTTPS (`127.0.0.1` and `::1`) on port 11921
with a locally generated certificate. It is reachable only from the same
machine and does not print a device-login QR code.

The viewer uses a bearer login token or short viewer code, then stores a signed
session cookie. Treat QR codes, login URLs, tokens, cookies, certificate keys,
and downloaded transcripts as secrets.

## Subagents in the viewer

The browser keeps an actor-level subagent list: one row per background
subagent, running rows first in spawn order, each showing its id, label, latest
activity, and elapsed time, and finished rows marked `✔`, `✘`, or `⊘`. This is
more granular than the terminal's aggregate workflow progress rows.
Permission requests raised by a subagent carry its id, so a remote user answers
the right nested request while several run at once. MCP approvals for
`mcp__mj_subagents__create_subagent` and `mcp__mj_subagents__subagent_cancel`
are recognized as Mjolnir's own tools rather than third-party ones.

## Side conversations in the viewer

When the connected agent supports ephemeral side sessions, the web command
palette exposes `/side` with an optional question. The same transcript stays
open in a visible side mode, but side prompts, replies, and tools are labelled
with the `Side` actor so they cannot be mistaken for main-session history. Use
the **Exit side** action (or send `exit`) to delete the temporary session and
return the composer to the main conversation.

## Status line in the viewer

Under the chat header the browser mirrors the terminal status line: primary
model and its ACP adapter, reasoning effort, per-seat token totals
(primary/review/subagents), context occupancy when the agent reports it,
provider subscription quota windows (Codex/Claude), and a link to the current
branch's open pull request. The pull-request badge comes from a `git`/`gh`
probe on the session host and refreshes about twice a minute.

Cost is deliberately not shown here. Only some ACP adapters report one, and
seats bind adapters independently, so a single session-wide figure would
silently cover just the seats that happen to report. `/agents` in the terminal
breaks cost down per seat and per model, where the unpriced seats are visible.

## Ragnarok in the viewer

While a local `/ragnarok` arena is active, the browser shows its task and
phase, each fighter's model source, status, and vigor, plus the final verdict
and adoption guidance when available. This view is read-only: summoning the
arena, approving `UNLEASH`, choosing between finalists, and publishing a draft
pull request remain local TUI actions.

## Tailscale

```bash
mj server --tailscale
```

This requires Tailscale, MagicDNS, and HTTPS Certificates enabled on the
tailnet. Mjolnir binds to network interfaces, asks `tailscale cert` for the
machine's `ts.net` certificate, and renews it. Tailnet reachability and ACLs are
part of the security boundary.

## Public hostname

```bash
mj server --hostname mj.example.com
```

This binds to network interfaces and generates a self-signed certificate for
the supplied hostname. It does not provision DNS, a trusted public certificate,
a reverse proxy, firewall rules, or internet authentication. Do not expose this
mode directly to an untrusted network without designing those layers.

## Retention and sign-out

```bash
mj server \
  --history-days 7 \
  --session-ttl-days 2
```

- `--history-days 0` keeps disconnected session history indefinitely.
- `--session-ttl-days 0` makes viewer sessions ephemeral.
- `--logout-all` rotates the cookie signing key and signs every viewer out. The
  underlying QR/bearer login token remains available for reauthentication.

Remote state includes local SQLite session/transcript data, queued prompts,
permission decisions, authentication material, and certificates under
Mjolnir's platform state/config directories.

## Before leaving loopback

1. Decide who can reach port 11921 and enforce that with host or tailnet policy.
2. Protect the login token, cookie key, certificates, and transcript storage.
3. Set finite history and session lifetimes.
4. Confirm remote users understand the active workspace and permission mode.
5. Test `--logout-all` and recovery before relying on remote access.
6. Keep the host patched; the server can drive provider agents and answer nested permissions.

See [Data and trust boundaries](/data-boundaries/) and [Storage and network
activity](/storage-network/) for the complete surface.
