---
title: Web viewer and desktop app
description: Control the same durable Mjolnir sessions from a browser, phone, or native desktop window.
---

Mjolnir's daemon starts a personal authenticated web viewer by default. It is a
control surface over the same workspaces and sessions as the terminal, not a
copy of the repository or a separate session server.

## Open the viewer

Ask the daemon for the current address and six-digit login code:

```console
mj daemon status
```

Open the reported URL, enter the code, and Mjolnir exchanges it for a signed
HTTP-only session cookie. The viewer exposes the workspaces attached to this
daemon, live and resumable sessions, the conversation and prompt composer,
target capacity, profile quota, and the new-session flow.

The viewer can:

- create a session by choosing its profile, target, project or bundle, and
  reviewing the resolved launch;
- open a live conversation, send prompts, and run the slash commands the
  session actually supports;
- queue prompts while an agent is busy and cancel agent or shell work;
- stop a session, resume it from its checkpoint, and browse hidden or archived
  resume candidates;
- refresh target capacity and profile quota; and
- keep a per-browser draft for the active conversation.

The terminal owns the richer launch workflow. Use it when you need per-session
CPU or memory sizing, attached-directory setup, or quick bundle creation. The
viewer also omits native-session import, force destruction, and configuration or
secret editing.

The browser's Back button returns from a conversation to its workspace. A
temporary network loss does not move session ownership into the browser: the
daemon and target keep working, and the viewer reconnects to their current
state.

## Open the native desktop shell

```console
mj app
```

`mj app` launches the separate `mj-desktop` executable and opens the same
viewer in a native window. The main `mj` binary stays headless. On Linux, the
desktop executable uses the system WebKitGTK runtime; install the distribution
package that provides WebKitGTK 4.1 when the app cannot start. Headless
installations do not need the desktop package or its native libraries.

## Local access by default

Without an explicit TLS configuration or a usable Tailscale node, the viewer
serves plain HTTP only on `127.0.0.1:3765`. This makes it reachable solely from
the controller machine.

The historical configuration section is named `[phone]`:

```toml
[phone]
enabled = true
bind = "127.0.0.1:3765"
tailscale_detect = true
```

Set `enabled = false` to turn the viewer off. A non-loopback `bind` is rejected
unless both `tls_cert` and `tls_key` are configured.

## Reach it through Tailscale

When the local Tailscale node has MagicDNS and HTTPS Certificates enabled,
Mjolnir requests the node's trusted `ts.net` certificate in the background and
serves HTTPS on all interfaces at the same port. The first certificate can take
about 30 seconds. Mjolnir renews it daily without restarting the daemon.

If detection cannot obtain a trusted certificate, `mj daemon status` keeps the
viewer on loopback and explains why. After changing the tailnet setting, run:

```console
mj daemon restart
```

To manage certificates yourself, explicit paths take precedence over automatic
detection:

```toml
[phone]
enabled = true
bind = "0.0.0.0:3765"
tailscale_detect = false
tls_cert = "/path/to/fullchain.pem"
tls_key = "/path/to/private-key.pem"
```

## Security boundary

The viewer is designed for one operator, not as a multi-user team service.
Treat its login code, cookies, TLS private key, and any copied
transcript as secrets. The repository and harness processes remain on their
configured targets, but an authenticated viewer can send prompts and lifecycle
commands with your authority.

See [Security boundaries](/security/) for the full trust model,
[Configuration reference](/configuration/#web-viewer-phone) for every field,
and [Troubleshooting](/troubleshooting/) when the viewer remains loopback-only
or the desktop shell cannot open.
