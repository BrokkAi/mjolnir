# SSH machines: bare and container runtimes

An SSH machine is a remote host under `[machines.<id>]` with `kind = "ssh"`.
Four runtimes can run on it:

- `bare` — uses an existing Git project directory on the remote machine.
  **Create isolated checkout** chooses a session-specific clone under
  `.mj/clones/<session-id>` instead of using the selected directory directly.
- `podman` — starts a rootless container on the remote machine
  (the same model as on this machine, just reached over SSH) and runs the
  session inside it.
- `docker` — uses the remote host's Linux Docker daemon; see
  [Docker for Mjolnir](DOCKER.md).
- `apple-container` — runs Apple's Linux container runtime on an Apple-silicon
  Mac; see [Apple container](/apple-container/).

All four shell out to the local `ssh` CLI rather than using an SSH library.

## Sharing connections per host

Provisioning a session runs 15 to 25 `ssh` and `scp` commands against the same
host, and every attached session keeps one more `ssh` process open for its
relay. On unix, Mjolnir runs these as sessions on a small number of shared,
already authenticated connections ("masters") instead of letting each command
pay for its own handshake.

An `sshd` limits the sessions on one connection with `MaxSessions` (stock
value 10; some hosts raise it to 64 or 200). Mjolnir does not guess that
number. It learns it from the server: a new session goes on the
lowest-numbered master for the host that is not known to be full, and a host
starts with one master. When `sshd` refuses a session on a master, Mjolnir
records that master's capacity as the number of sessions it was carrying at
that moment, and runs the refused command again on the next master that is not
known to be full, opening one if needed. The refused command never started, so
running it again is safe, and the recorded capacity ends the retry without a
delay. Mjolnir logs each learned capacity once, at info level, with the host,
the master number, and the capacity. A master keeps its learned capacity only
while it runs: a new master, opened after the old one exited, starts with an
unknown capacity, so a changed `MaxSessions` is learned again.
The old `MJ_SSH_SESSIONS_PER_CONNECTION` setting is ignored.

Mjolnir opens each master explicitly with `ssh -f -N -o ControlMaster=yes
-o BatchMode=yes -o ControlPersist=60` and confirms it with `ssh -O check`.
Every other command then runs with `ControlMaster=no`, the master's
`ControlPath`, and `ProxyCommand=false`. That combination can only use the
master: if the master is gone, the command fails at once with exit status 255
instead of quietly opening its own connection. Mjolnir then checks the master
again, reopens it, and retries the command. If a master cannot be opened at
all, the command fails with an error that names the host and quotes `ssh`'s
own message. A master exits 60 seconds after its last session closes, so
`ssh` processes can outlive the daemon by that long.

The control sockets live in `$XDG_RUNTIME_DIR/mjolnir/<instance>/` when that
variable is set, and in `<data dir>/ssh/` otherwise. `<instance>` is
`default` for the default instance, the name for `--instance`, and a
16-character hash of the data directory when only `MJ_DATA_DIR` is set.
Each instance has its own directory, so two daemons never share a master. A socket is named `<hash>-<shard>`, where the
hash covers the destination and your `extra_args`, and the shard number counts
the masters for that host from 0. A `<hash>-<shard>.lock` file beside each
socket makes processes of the same instance (for example the old and new
daemon during a restart) take turns checking and opening that master, so
they never open two. The socket path is the value of an `ssh` option, which
`ssh` splits on whitespace, so a path containing a space or `%` is quoted and
escaped for `ssh`'s own parser: macOS keeps its data in
`~/Library/Application Support/mjolnir`, and that space would otherwise end
the option early and make `ssh` refuse the whole command line. Mjolnir creates
the directory with mode
`0700`. If it cannot, or if the socket path would be too long for a unix socket
address, every command opens its own connection instead. Commands that share
a master also share its fate: if the underlying connection drops, every
session on it fails at once.

Two kinds of command only ever *reuse* a master and never create one: the
target validation probes (including the `mj doctor` connectivity probe) and
remote Tab completion. They set a short `ConnectTimeout` and a one-miss
keepalive so they fail fast instead of hanging the interface, and a master
holding those settings would drop every later session on it after a stall of
a couple of seconds. They carry `ControlMaster=no` and the path of a master,
so they join it when it is up and otherwise open their own direct
connection. In the daemon, a validation probe counts as a session on the
master it joins, so the capacity Mjolnir learns includes the probes. `mj doctor`
and Tab completion run in other processes and are not counted, so the server
can refuse them when a master is full.

Every refusal happens before the remote command starts, so Mjolnir runs the
command again. The rules depend on what the refusal says:

- A refusal that teaches Mjolnir a new capacity is retried at once on another
  master. It is logged only at debug level.
- A connection closed before authentication (`MaxStartups`) means `sshd` is up
  and asking the client to come back later. Mjolnir waits and tries again,
  with a growing, randomized delay of up to a few seconds, until the command
  gets in or the operation is cancelled or reaches its own deadline. Each
  retry is logged as a warning, and once a command has been refused for over a
  minute, Mjolnir logs one more warning saying so. A few internal paths cannot
  be cancelled (for example some cleanup after a session is destroyed); they
  stop after three attempts.
- A session refused by a master whose capacity is already known has nothing
  new to learn. Mjolnir tries up to three times with a growing delay, then
  logs a warning that names "refused another session on a shared connection
  (MaxSessions)".

The dashboard's own Git reads for the conversation header, the capacity probe,
and the relay connection to each session's worker retry the same way. A command
that streams input from the caller (an upload through standard input) cannot
be run again because the input has been consumed, so it fails with `ssh`'s
message; the refusal is still recorded, and the next command goes to another
master.

If the target's `extra_args` already set `ControlMaster`, `ControlPath`, or
`-S`, Mjolnir adds no sharing options at all for that target and leaves
sharing to you. A `-J` jump host in `extra_args` is used when a master is
opened. To turn sharing off entirely, set `MJ_SSH_CONTROL_MASTER=0` (`off`,
`false`, and `no` also work) in the daemon's environment; every command then
opens its own connection. Sharing is unix-only; Windows OpenSSH does not
implement it.

## Connection concurrency

Mjolnir does not impose a client-side cap on concurrent SSH commands or
connections. On Unix, commands normally use channels over a shared OpenSSH
connection; only opening a connection authenticates and counts against the
remote `sshd`'s `MaxStartups`. When a server refuses a connection before
authentication, Mjolnir recognizes the refusal and retries the invocation until
it gets in or the operation is cancelled, because the remote command never ran. Commands on platforms without connection
sharing open their own connection and use the same refusal retry.

## Prerequisites you set up by hand

- **Key-based SSH that works non-interactively.** Mjolnir runs `ssh` without a
  pseudo-terminal and does not prompt for a password or passphrase, so the
  target user must already accept your key without interaction (an unlocked
  key, `ssh-agent`, or a passphrase-free key).
- **A host-key policy you have chosen.** By default Mjolnir passes
  `-o StrictHostKeyChecking=accept-new`: `ssh` trusts a host key it has never
  seen and records it in `known_hosts`, and refuses a key that has changed.
  Mjolnir does not otherwise manage `known_hosts`. To require that the key
  already be in `known_hosts`, add the option to the machine's `extra_args`:

  ```toml
  [machines.builder]
  kind = "ssh"
  host = "builder"
  extra_args = ["-o", "StrictHostKeyChecking=yes"]
  ```

  OpenSSH uses the first value it sees for an option, and Mjolnir puts
  `extra_args` before its own defaults, so this setting wins. The same holds
  for `UserKnownHostsFile` and any other option. Setting
  `StrictHostKeyChecking yes` in `~/.ssh/config` is not enough: options on
  the command line override the config file. Add the host key first, for
  example with `ssh-keyscan <host> >> ~/.ssh/known_hosts` after you have
  checked the key's fingerprint.
- For a bare runtime: **an existing remote Git project with a valid `HEAD`.** The
  SSH user must be able to create `.mj/clones/` below the
  repository when **Create isolated checkout** is selected. Uncommitted changes
  in the source are not copied into that clone.
- For a Podman runtime: **rootless Podman on the remote host**, meeting the same
  postconditions Mjolnir expects locally. See [Podman for Mjolnir](PODMAN.md) — the
  remote host needs Podman 4.3 or newer and the same rootless
  user-namespace setup as a local Podman host.
- For an Apple container runtime: **Apple's signed `container` package, macOS 26
  or newer, Apple silicon, and a running service**. Follow the [official
  installation instructions](https://github.com/apple/container#initial-install)
  on the Mac, run `container system start`, and check `container system status`.
  Mjolnir runs `container` with the Mac's system `PATH` from `/etc/paths`,
  which includes `/usr/local/bin` where the package installs it, so the SSH
  account needs no shell configuration. Mjolnir checks these same commands
  before offering or launching the runtime.

## Machine and runtime configuration

The SSH host is one `[machines.<id>]` entry, and every runtime on it names that
machine. Sessions run on runtimes, so a machine with no runtime is not offered
as a session target.

You do not have to write the runtimes yourself. Each time `mj` starts and each
time you open the new-session or resume picker, Mjolnir asks every machine what
it can run, with one SSH call per machine that runs every check and
disconnects, and adds each runtime it newly runs: a `bare` runtime named after
the machine when the host is Linux or macOS on x86_64 or arm64,
`<machine>-podman` and `<machine>-docker` for each engine that passes the same
check a launch runs, and `<machine>-apple-container` when the machine is
Apple-silicon macOS 26 or newer with a running Apple container service. This
covers machines that already have runtimes, so Docker installed later on a
host you already use appears without any configuration. `mj setup` does the
same, and Settings › Runtimes › Detect runtimes does it on demand. A runtime
whose machine did not answer, or whose engine stopped working, is left out of
the pickers and named under the list with the reason. A machine that answers
with nothing usable, or does not answer, keeps no runtime; Settings ›
Machines, the new-session picker and `mj doctor` say why, with the fix. To keep
a runtime you do not want, set `enabled = false` on it (or clear Enabled in
Settings › Runtimes): detection never adds a kind the machine already has
configured. To stop using a host altogether, remove the machine.

The machine's keys are:

| Key | Required | Notes |
| --- | --- | --- |
| `kind` | yes | `ssh`. |
| `host` | yes | SSH destination: hostname, IP, or an alias from your SSH config. |
| `user` | no | SSH login user; omit to use your SSH config / default. |
| `identity_file` | no | Path to the private key. |
| `extra_args` | no | Extra arguments for every `ssh` invocation for this machine. They come before Mjolnir's own options, so they override Mjolnir's defaults. |
| `workspace_prefix` | no | Per-session lifecycle path recorded for cleanup as `<prefix>/<session-id>`. It does not select or relocate the Git project. Defaults to `.local/share/hel/workspaces` relative to the login home. |

A `bare` runtime on the machine also takes:

| Key | Required | Notes |
| --- | --- | --- |
| `permissions` | no | `guardian` (the default) preserves configured harness approvals; `yolo` runs unconstrained. |

Guardian approvals are supported by Codex, Claude Code, Grok Build, OpenCode,
and Muse Code. Kimi does not provide them.

```toml
[machines.builder]
kind = "ssh"
host = "builder"
workspace_prefix = ".local/share/hel/workspaces"

[targets.builder]
kind = "bare"
machine = "builder"
permissions = "guardian"
```

### macOS hosts, including EC2 Macs

A bare SSH runtime can run directly on an Intel or Apple silicon Mac. The
controller detects the remote operating system and architecture and uploads
the worker from its own release. Linux controllers can manage Mac workers;
release archives include both per-architecture macOS workers, and each macOS
npm package includes its own architecture's worker.

Enable Remote Login on the Mac and configure noninteractive SSH key access.
Install working Git (including the Xcode Command Line Tools if using Apple's
Git), curl, and the tools your project needs. Node-based harnesses reuse
Node.js 22 or newer with npm and npx when the login environment provides them;
otherwise the worker downloads and verifies its pinned Node.js with curl.
Mjolnir checks the runtime through the account's login shell, so Homebrew's
PATH setup belongs in that shell's login configuration, such as `.zprofile`
for zsh. The worker uses the target account's login environment and then
applies any configured profile environment overrides. Apple container commands
run through noninteractive SSH with the Mac's system `PATH` from `/etc/paths`,
so the official package's `/usr/local/bin/container` is found without any
shell configuration.

```toml
[machines.mac]
kind = "ssh"
host = "my-mac"

[targets.mac]
kind = "bare"
machine = "mac"
permissions = "guardian"
```

Select an existing Git project on the Mac when starting a session. Workspaces
and checkpoints follow the same SSH-bare lifecycle described below. Closing
a session does not shut down the Mac or stop other sessions.

For EC2, provision the Mac instance and Dedicated Host yourself, then use its
SSH address in this configuration. Use the SSH machine kind for this persistent
host: the `aws-ec2` machine kind owns disposable instances and terminates them
on session teardown. This SSH configuration does not allocate or release AWS
hosts, install Xcode, or manage signing identities.

When developing from source, provide a worker built from the same source
revision as the controller. `MJ_WORKER_DIR` accepts
`mj-worker-aarch64-apple-darwin` or `mj-worker-x86_64-apple-darwin`; the older
combined `mj-worker-universal-apple-darwin` is still accepted as a fallback for
bundles that predate the per-architecture split. Build an architecture-specific
worker on a Mac with `cargo build -p brokk-mj-worker --bin mj-worker`; copy it
under the matching name in that directory. A release controller and a worker
from another revision cannot be mixed.

### How a remote bare project is prepared

The new-session wizard asks for an existing absolute Git directory on the SSH
host. Mjolnir validates that path remotely; it does not clone a configured
bundle into it.

**Create isolated checkout** defaults on for a primary checkout and off for
an existing linked worktree; you can change either choice in the final review.
When checked, Mjolnir creates an independent clone at
`<repository>/.mj/clones/<session-id>` on the default branch. Changes in the
source checkout stay there. When unchecked, the session uses the selected
checkout directly.

Because the session works in a checkout that belongs to the host, it stays
there: `mj move` and `mj resume` refuse a target on another machine
(`this session's working tree lives on <host>; resume it there`). Changing
the profile in place works. To continue elsewhere, push the session's branch
and start a new session from it.

`workspace_prefix` is separate from this project workflow. It derives a
Mjolnir-owned lifecycle path that is recorded with the target and removed
during teardown. It does not control the selected project, the linked-worktree
location, or the fixed worker and staged-profile roots. A leading `~/` is
treated as relative to the SSH login home; leave the default unless you need a
different cleanup namespace.

A Podman runtime always runs unconstrained and does not accept `permissions`.
It takes the same container keys as a local Podman runtime (`image`, and optionally
`platform`, `cpus`, `memory`, `environment`, `pull_policy`,
`workspace_storage`):

```toml
[targets.builder-podman]
kind = "podman"
machine = "builder"
```

## Verifying a target

`mj doctor` includes a dedicated check per runtime on an SSH machine, and a
`machine.<id>` check for a machine that has no runtime, saying what probing it
found. It
first probes connectivity with `ssh -o BatchMode=yes <host> true`, and its
failure messages include the exact command to fix the common causes
(`ssh-copy-id` for key auth, `ssh-keyscan` for an untrusted host key). For
a Podman runtime it then runs the same Podman probes as on this machine, over
SSH. With `--smoke`, Podman over SSH also gets a disposable run/exec/remove
test; a bare runtime does not have a smoke test.

You can also always sanity-check
reachability by hand before relying on a target:

```console
ssh <host> true
```

If that succeeds non-interactively, Mjolnir's own SSH invocations should too.
