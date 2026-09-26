---
title: Troubleshooting
description: Diagnose Mjolnir setup, authentication, PATH, terminal keys, targets, web access, checkpoints, and recovery.
---

Start with Mjolnir's own diagnosis. Its checks share the same configuration and
target validation code used during setup and launch, so their remediation is
more useful than a generic package checklist.

## The doctor-first loop

Run the machine-readable report:

```console
mj doctor --json
```

Then:

1. Follow every check whose status is `fixable`.
2. Run `mj doctor --json` again.
3. Repeat until no check is `fixable`.
4. If you use containers, finish with an end-to-end disposable target test:

   ```console
   mj doctor --json --smoke
   ```

`--smoke` goes beyond static prerequisite checks for Podman, Docker, and Apple
Container, on this machine or over SSH: it launches, executes in, and removes a
disposable container. It does not smoke-test a bare runtime on any machine. The Docker test also proves the writable OverlayFS attachment path
and cleanup.

For a self-contained handoff to another coding agent, generate the platform
instructions and include the latest JSON report:

```console
mj setup instructions --platform linux
# or
mj setup instructions --platform macos
```

The human-readable `mj doctor` output is convenient at a terminal; JSON is
better for exact check IDs, automation, and issue reports. A warning can still
matter, but only `fixable` means the doctor loop has not converged.

## Configuration is missing, invalid, or from a newer build

If `config.toml` does not exist, run `mj` and open **prefix+s Settings** to add or detect
an agent profile. Local target choices are supplied automatically.
Managed container and EC2 sessions also need a bundle; a bare session selects
an existing Git project directory instead.

For a TOML error, fix the exact path and type named by `mj doctor`. Mjolnir's
schema rejects unknown fields rather than silently ignoring a misspelling. See
the [Configuration reference](/configuration/) for every field.

An older Mjolnir refuses to load a configuration last written by a newer build.
Update Mjolnir, or use the newer build; do not lower the `version` value by
hand.

## A bare clone does not contain my local changes

When **Create isolated checkout** is checked for a new bare session, Mjolnir
creates an independent clone. Source changes do not enter that clone. If you
expected to see uncommitted files from the selected directory, inspect them there:

Inspect the selected checkout on the local or remote target:

```console
git -C <project-directory> status --short --untracked-files=all
```

For a network-backed clone, commit and push the work and select the branch
that contains it; the default is the remote's default branch. To work directly in the selected
directory with its current changes, uncheck **Create isolated checkout** on the
final review. See [Targets](/targets/#bare-runtimes)
and [Workspaces and bundles](/workspaces-bundles/).

## A harness profile is not authenticated

Run the login through Mjolnir so it uses the configured profile home:

```console
mj login --profile <profile-id>
```

If exactly one profile exists, `--profile` is optional, but naming it removes
ambiguity. Then rerun `mj doctor --json`. If doctor still reports no usable
authentication:

- confirm that `[profiles.<id>].home` points to the home used by that harness
  account;
- make sure the harness login completed and wrote its normal authentication
  marker beneath that home;
- check that the profile kind matches the files in the home; and
- resolve any executable or `PATH` failure before repeating login.

For unattended or concurrent Claude Code sessions, prefer a long-lived setup
token:

```console
mj login --profile <claude-profile-id> --setup-token
```

It avoids competing session copies trying to rotate the same single-use OAuth
refresh grant. Credential behavior and its limits are explained in [Security
boundaries](/security/); profile fields are in [Profiles and
harnesses](/profiles/).

### GitHub authentication fails inside a target

Check the controller first:

```console
gh auth status
```

Mjolnir synchronizes the active `gh` token into live non-local sessions. It
does not copy SSH keys, and raw local sessions are excluded from this token
sync. Authenticate `gh` on the controller, wait for the next synchronization,
and use HTTPS Git URLs in isolated targets.

For a repository configured with `local`, isolated sessions resolve its default
network fetch and push remotes. Check those URLs and the fetch remote's
advertised default branch. Local unpublished commits and dirty files are not
copied, so cleaning the host working tree does not fix a remote-authentication
failure. Repositories without network remotes require a raw local session.
Legacy host-bridge checkpoints cannot resume; start a new network-backed
session. See [Workspaces and bundles](/workspaces-bundles/).

## The harness exists in a shell but Mjolnir cannot launch it

Mjolnir-owned workers and ACP bridges use non-login shells. A binary made
visible only by `.bashrc`, `.zshrc`, `/etc/profile`, a shell alias, or a shell
function can therefore work interactively and still be absent from Mjolnir's
runtime.

On raw local, SSH, and EC2 targets, each worker start performs one bounded
login-shell probe and carries the discovered `PATH` into the non-login
runtime. An explicit profile setting wins:

```toml
[profiles.my-profile.environment]
PATH = "/opt/my-tools/bin:/usr/local/bin:/usr/bin:/bin"
```

After changing a profile's environment, restart the worker or stop and resume
the session; an already running worker does not continuously reread shell
startup files. Bare sessions on this machine, SSH, and EC2 do not launch an ambient harness from
this path: they use the exact Mjolnir-managed version, while `PATH` supplies its
installer prerequisites. These targets require Node.js 22 and npm for Codex
and Claude, curl and Bash for Kimi and Grok, or curl and tar for Muse. Install prerequisites
on the host yourself; Mjolnir never invokes sudo for harness setup.

Container images must expose required tools on their ordinary image `PATH`.
Agent-requested `!` shell commands use `bash -lc`, so their environment can
differ from the worker and bridge environment. That distinction explains the
common case where `!which tool` succeeds but session launch says the tool is
missing. See [Custom container images](/custom-images/) and the profile fields
in [Configuration reference](/configuration/).

## The prefix key does nothing

Mjolnir's global commands run from a tmux-style prefix key (`ctrl+b` by
default): press it, release it, then press the command letter. If pressing it
does nothing, something between your keyboard and Mjolnir is intercepting it:

- **tmux or GNU screen**, run with their own default prefix (`ctrl+b` for
  tmux, `ctrl+a` for screen), consumes the key before Mjolnir sees it. Press
  it twice to send the literal key through, the same way you would reach any
  other application's own binding on that key from inside the multiplexer; or
  set a different `prefix` in `[keys]` so the two tools stop colliding.
- A terminal emulator or desktop environment can also bind the same key
  globally; check its keyboard shortcut settings if the multiplexer
  explanation does not apply.

Plain `?` still opens help while focus is outside Prompt, independent of the
prefix. See [Terminal surface](/terminal-surface/#prefix-key) for the full
default table and [Configuration reference](/configuration/#keys-keys) for
rebinding the prefix.

## A `[keys]` edit did not take effect

An invalid `[keys]` entry is a configuration error like any other, and it
names the exact field. At startup this is fatal: Mjolnir refuses to run until
the file is fixed. While Mjolnir is already running, the daemon reloads
`config.toml` continuously but keeps the last good configuration and only
logs a warning when a reload fails, so a broken edit can look like nothing
happened rather than like an error. Run `mj doctor` to see the current
validation error, or restart Mjolnir to have it reported at startup.

## A container target will not start

Run the smoke test before changing the Mjolnir configuration:

```console
mj doctor --json --smoke
```

Then follow the target-specific result:

- **Podman:** it must be rootless and have a valid subordinate UID/GID mapping.
  Run `podman unshare cat /proc/self/uid_map` as the same user that runs
  Mjolnir, then follow [Podman for
  Mjolnir](/podman/).
- **Docker:** the CLI must reach a Linux daemon, and writable attachments need
  working OverlayFS support on the daemon's filesystem host. Run `docker info`
  and follow [Docker for Mjolnir](/docker/).
- **Apple container:** the controller must be Apple silicon on macOS 26 or
  newer. If doctor reports a stopped service, run `container system start`.
  See [Apple container](/apple-container/).
- **SSH Podman:** run the Podman prerequisite and image checks on the remote
  host as the same user selected by SSH. See [SSH and SSH Podman](/ssh/).
- **EC2:** verify the configured AWS profile, region, launch template, SSH
  user, address source, and reachability. See [AWS EC2](/aws/).

If the runtime works but the harness bridge does not, the image likely lacks a
required binary, Node version, writable Mjolnir directories, or ordinary
process `PATH`. Compare it with the contract in [Custom container
images](/custom-images/).

### An attached directory became read-only

This is expected when Mjolnir detects a filesystem that cannot safely host its
copy-on-write overlay, including NFS, SMB, FUSE, FAT-family filesystems, and an
already layered OverlayFS. The launch notice names the detected filesystem.

Move the source to a compatible local Linux filesystem if writes are required,
or keep it read-only and write results into the session workspace. A writable
copy-on-write attachment never writes back to the original source. See
[Container targets](/containers/).

## The web viewer is unavailable

Start with:

```console
mj daemon status
```

The status includes daemon version and PID, viewer state, URL, login code, and
the reason for any loopback fallback. Then check these cases:

- If the viewer is disabled, set `phone.enabled = true` and run
  `mj daemon restart`.
- Without trusted TLS, the viewer intentionally serves only on
  `http://127.0.0.1:3765` by default. It is not reachable from another device.
- Automatic tailnet access requires Tailscale MagicDNS and HTTPS Certificates.
  Enable them, then run `mj daemon restart`. First certificate issuance can
  take about 30 seconds.
- Explicit TLS requires both `phone.tls_cert` and `phone.tls_key`. Mjolnir
  refuses a non-loopback plaintext bind and a half-configured key pair.
- Five incorrect six-digit codes trigger a 30-second lockout; repeated
  lockouts grow to one hour. Wait for the reported lockout rather than retrying
  continuously.

The native `mj app` command opens the same viewer. On Linux its separate
desktop executable needs the system WebKitGTK 4.1 runtime—for Debian and Ubuntu
the package is commonly `libwebkit2gtk-4.1-0`. The headless `mj` controller
does not need that library.

See [Web viewer and desktop app](/web-viewer/) for setup,
[Security boundaries](/security/) for its authentication model, and
[Configuration reference](/configuration/) for `[phone]`.

## Checkpoints or upgrades keep waiting

Automatic checkpoints and worker replacement require a quiet boundary. A
session is not quiet while any of these remain:

- an active user prompt or autonomous harness turn;
- a queued prompt or configuration change;
- a user shell command;
- an agent-owned terminal or background command; or
- a checkpoint barrier already in progress.

Inspect the selected session and its queue. Cancel only work you are willing to
interrupt; a background command can be real agent work even after the last
visible response. Once all work ends, the recovery and upgrade coordinators
observe the idle state automatically. Read
[Durability and recovery](/durability/) for the exact guarantees.

## Suspension failed or a session needs recovery

A normal suspension will not destroy a target unless its recovery archive
passes the checksum and frontier gates. A suspension failure is therefore normally
non-destructive: read the exact checkpoint or target error, repair the storage
or connection problem, and try a manual checkpoint:

```console
mj checkpoint --session <session-id>
```

Then choose **Retry suspension** in the failure dialog, or run **Suspend
session…** again from the command palette. If a fresh checkpoint remains
impossible but an older verified archive exists, the failure dialog also offers
**Discard changes since checkpoint…**. That removes the live target without a new archive and can lose every
change after the older checkpoint, but leaves the session resumable. **Destroy session…** is different: it removes the target, archive, and record
permanently.

If a managed target is still running but has disappeared from controller
state, scan without changing it:

```console
mj recover scan --json
```

Adopt the exact resource you recognize:

```console
mj recover adopt --session <session-id> --target <target-id>
```

Resources created before current ownership markers may also need `--profile`
and `--bundle`. Adoption reconnects to the existing worker and its newer
on-target journal; resuming only from an archive may be older.

Destroy an untracked resource only after confirming it is the right one and
that no newer work needs recovery:

```console
mj recover destroy \
  --session <session-id> \
  --target <target-id> \
  --confirm <session-id>
```

The confirmation value must repeat the full session ID. For stopped, lost, or
retryable sessions already present in the dashboard, use the resume and recover
actions described in [Session lifecycle](/sessions/).

## Collect useful diagnostics

When the built-in remediation is not enough, collect:

- `mj --version`, operating system, architecture, harness profile ID, and
  target ID;
- the latest `mj doctor --json` output;
- the exact launch, checkpoint, resume, or viewer error; and
- the newest `mj-daemon-*.log` under Mjolnir's platform data directory
  `logs/` subdirectory, which is the long-running daemon's own log;
  `mj-tui-*.log` and `mj-cli-*.log` files there are shorter-lived dashboard
  and one-shot command runs, retained separately.

Set `MJ_DATA_DIR` only when you intentionally override that platform data
directory. Worker connection failures include the worker exit record and the
tail of `worker.log` when Mjolnir can still reach the target, so preserve the
complete error chain.

Do not attach raw harness authentication files, GitHub tokens, TLS private
keys, recovery archives, or a full profile home to an issue. Paths and
non-secret credential fingerprints can still be identifying; review diagnostic
output before sharing it.
