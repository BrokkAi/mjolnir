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

On a workstation, the dashboard arranges sessions in columns and conversations
use a wider reading area with the composer kept in view. On phones, sessions
remain in a single column with touch controls.

**Create** and **Resume** stay above the Sessions list, including when a workspace
has no live sessions or the list is filtered. Opening a conversation does not
move those controls away from the list.

The viewer can:

- create a session by choosing its profile, target, project or bundle, and
  reviewing the resolved launch;
- open a live conversation, send prompts, and run the slash commands the
  session actually supports;
- queue prompts while an agent is busy and cancel agent or shell work;
- stop a session, resume it from its checkpoint, and browse all stopped resume
  candidates, including records previously archived by a provider;
- prepare and confirm a move to another compatible target or profile while
  keeping the same logical session; and
- refresh target capacity and profile quota; and
- keep a per-browser draft for the active conversation.

The terminal owns the richer launch workflow. Use it when you need per-session
CPU or memory sizing, attached-directory setup, or quick bundle creation. The
viewer also omits native-session import, force destruction, and configuration or
secret editing.

### Move confirmation

Move is a two-step, authenticated flow. The viewer first asks the daemon for a
read-only preparation that resolves the profile, target, compatibility, active
state, and queued commands. Only the confirmation submits the fingerprinted
preparation and interruption acknowledgement. The browser cannot implement a
move by composing Stop and Resume, and a stale destination is rejected before
the source is interrupted.

The confirmation explains that a fresh environment is rebuilt. It warns before
interrupting an active turn, lists queued prompts and configuration changes,
and defaults to discarding that queue. Selecting **Run queued work** admits the
original commands only after the destination is ready. The daemon keeps moving
after a browser tab closes; the session row reports its current phase and
recovery guidance on reconnect.

Resource sizing is inherited and attached directories remain fixed to the
workspace. The confirmation has an explicit **Clear inherited resource sizing**
checkbox for returning to destination defaults. If a move fails or is
cancelled after retaining its checkpoint, the dashboard offers **Retry move**
with the recorded destination and **Resume with previous settings** using the
source profile and target. Partial queue admission is pinned to its original
destination and queue choice so a retry cannot replay accepted commands on a
new target.

The browser's Back button returns from a conversation to its workspace. A
temporary network loss does not move session ownership into the browser: the
daemon and target keep working, and the viewer reconnects to their current
state.

## Recover a port conflict

Press **F4** in the terminal to open **Web viewer**. If its port is occupied,
the dialog shows the address and offers **Use another port**, **Retry**, and
**Inspect port**. Startup status updates automatically while the dialog is open.

**Use another port** reserves an available port and shows the new URL and login
code. It keeps the same HTTPS hostname and certificate when HTTPS is configured.
The port applies until the daemon restarts; saved configuration is unchanged.

**Inspect port** shows the listening process and its PID. On Linux, an identified
Mjolnir daemon owned by your account can be stopped with **Stop server…** and a
separate **Stop and retry** confirmation. Other clients using that daemon will
disconnect. Mjolnir verifies the process identity again before requesting a
graceful stop and does not escalate to a force kill. The current daemon and
unrelated applications cannot be stopped through this dialog. On other platforms,
stop the identified application yourself or use another port.

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
