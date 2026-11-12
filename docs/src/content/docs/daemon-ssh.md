---
title: Run the daemon over SSH
description: Use a native Windows dashboard or a browser with a Mjolnir daemon on a Linux computer.
---

SSH placement lets you run the Mjolnir daemon on a separate Linux computer
and use the terminal dashboard on your desktop, including native `mj.exe` on
Windows. The Linux computer owns your sessions, workers, settings and harness
credentials. Your desktop connects to its control connection through SSH. The
browser connects directly to the Linux computer's web viewer.

## Connect your desktop

Install the same Mjolnir release on both computers using the
[installation instructions](/install/). Development builds must come from the
same commit; see the development command below.

In your desktop's Mjolnir configuration, set:

```toml
[daemon]
runtime = "ssh"
host = "linux-box"             # an SSH destination or ~/.ssh/config alias
instance = "remote-dev"        # optional; defaults to the client's instance
ssh_args = []                  # for example ["-i", "C:/Users/me/.ssh/id_ed25519"]
remote_command = "mj"           # or the installed executable's absolute Linux path
```

Only this placement table belongs on the desktop. Every other setting lives
on the daemon's machine; use the dashboard to edit daemon settings. To keep a
separate configuration for development, select the same named instance on
every invocation:

```console
mj --instance remote-dev daemon status
mj --instance remote-dev
```

Status names the configured daemon host and remote instance, the client configuration
file, and both builds. It reports `same_source: true`
when they match. A mismatch requires installing the same release or development
commit on both computers.

SSH must work with key authentication and a trusted host key before Mjolnir
connects. Connect with `ssh linux-box` first to verify access and host trust.
Use your SSH configuration or `ssh_args` for connection options. Mjolnir runs
`remote_command` through the remote user's login shell, so the shell's profile
must put the installed `mj` on PATH. If it does not, use an absolute path such
as `/home/me/.cargo/bin/mj`. The remote configuration must use host or container
placement, rather than forwarding to another SSH daemon.

## Projects and settings

Project paths refer to the daemon's machine. Give an explicit Linux path when
starting a session from the command line:

```console
mj --instance remote-dev go --path /home/me/Projects/my-project
```

Your Windows current directory is not a Linux project path. The terminal
dashboard and browser are the remote client surfaces; CLI commands that use
the HTTP API also require the viewer to be reachable.

## Reach the web viewer

SSH carries the control connection, not the web viewer's HTTP traffic. Open
the daemon's advertised viewer URL in your browser. The desktop must be able
to reach that address directly.

On the Linux computer, use working Tailscale detection:

```toml
[phone]
tailscale_detect = true
```

Or configure a reachable address and TLS, with certificate files on Linux:

```toml
[phone]
bind = "192.168.1.50:8080"
tls_cert = "/home/me/certs/viewer.crt"
tls_key = "/home/me/certs/viewer.key"
```

See [Web viewer](/web-viewer/) for setup. An advertised loopback address such
as `127.0.0.1`, or a wildcard such as `0.0.0.0`, produces an error telling you
to configure the daemon's viewer address. Control connections can work while
the viewer remains unreachable. Remote container daemons do not publish their
viewer for this mode yet.

## Logins, import and voice

The dashboard login pane, history upload and remote voice transcription are
still being implemented. Their behavior is settled as follows:

- **Settings → Login** runs the harness login on the daemon and shows its
  terminal in a dashboard pane. Browser OAuth callbacks to `localhost` on the
  Linux computer may not complete from your desktop. Paste-code and API-key
  methods work without that callback; available methods depend on the harness.
- **Import** reads your own harness history on the client and uploads it to the
  daemon. If its recorded project path differs, choose a project path on the
  daemon's machine. The web viewer has no import.
- **Voice** records audio on the client and sends it to the daemon for
  transcription using the daemon's credentials.

## One command for development builds

Run this from a Windows checkout with native Git, OpenSSH and Rust's MSVC
build tools installed. The Linux checkout needs the normal prerequisites for
`scripts/install.sh`, plus Git and `flock`. Configure the selected instance's
SSH placement first. Both checkouts must be able to fetch the selected commit.

```powershell
pwsh -File scripts/dev-remote.ps1 -SshHost linux-box -Instance remote-dev
```

The default source is `~/Projects/mjolnir` on Linux. Override either source
checkout or select an exact commit:

```powershell
pwsh -File scripts/dev-remote.ps1 -SshHost linux-box `
  -RemoteCheckout '~/Projects/mjolnir' -WindowsCheckout 'C:\src\mjolnir' `
  -Commit <full-commit-sha> -Instance remote-dev
```

The command resolves the Linux checkout's HEAD once unless you supply
`-Commit`. It reuses a dedicated `<checkout>-build` worktree on each computer,
leaving your source checkout's branch alone. Keep those build worktrees free
of edits. A lock prevents two copies of this command from building in the same
worktree at once. Do not edit, commit or run another installer there during a
build: changing source between binary builds can give them different build
identities.

Linux uses that commit's `scripts/install.sh` to install `mj`, the native and
portable workers and the voice helper. Windows builds and installs native
`mj.exe`. The Windows destination is `MJOLNIR_INSTALL_DIR` when set, otherwise
`%LOCALAPPDATA%\Programs\Mjolnir\bin`, as with the release installer. Set
`MJOLNIR_NO_MODIFY_PATH` to leave the user PATH alone.

The command checks that daemon status names the same host as `-SshHost` and
the same remote instance as `-Instance` before starting or upgrading the daemon.
If they differ, it names both values and the client configuration file to change.
It finishes only after the installed Windows client reports `same_source: true`
from daemon status. If either installation or that check
fails, rerun with the printed `-Commit` SHA to finish the same build. A failed
run may already have installed one computer. If an unpublished commit cannot
be fetched from the Linux checkout over SSH, the error gives the exact Git
push command to make it available through origin.
