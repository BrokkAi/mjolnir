# SSH machines: bare and container runtimes

An SSH machine is a remote host under `[machines.<id>]` with `kind = "ssh"`.
Two runtimes can run on it:

- `bare` — uses an existing Git project directory on the remote machine.
  When that path is the repository's primary checkout, Mjolnir creates a
  session-specific linked worktree beside it and runs the harness there.
- `podman` (or `docker`) — starts a rootless container on the remote machine
  (the same model as on this machine, just reached over SSH) and runs the
  session inside it.

Both shell out to the local `ssh` CLI rather than using an SSH library.

## Sharing connections per host

Provisioning a session runs 15 to 25 `ssh` and `scp` commands against the same
host, and every attached session keeps one more `ssh` process open for its
relay. On unix, Mjolnir runs these as sessions on a small number of shared,
already authenticated connections ("masters") instead of letting each command
pay for its own handshake.

A stock `sshd` allows at most 10 sessions on one connection (`MaxSessions
10`). Mjolnir therefore places at most **8 sessions on each master** and opens
another master for the same host when all of its masters are full. Set
`MJ_SSH_SESSIONS_PER_CONNECTION` in the daemon's environment to change the
number; an unset or invalid value falls back to 8. The two spare sessions are
for `mj doctor` and Tab completion, which can join a master from another
process without being counted.

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
variable is set (`default` for the default instance), and in
`<data dir>/ssh/` otherwise. Each instance has its own directory, so two
daemons never share a master. A socket is named `<hash>-<shard>`, where the
hash covers the destination and your `extra_args`, and the shard number counts
the masters for that host from 0. Mjolnir creates the directory with mode
`0700`. If it cannot, or if the socket path would be too long for a unix socket
address, every command opens its own connection instead. Commands that share
a master also share its fate: if the underlying connection drops, every
session on it fails at once.

Two kinds of command only ever *reuse* a master and never create one: the
target validation probes (including the `mj doctor` connectivity probe) and
remote Tab completion. They set a short `ConnectTimeout` and a one-miss
keepalive so they fail fast instead of hanging the interface, and a master
holding those settings would drop every later session on it after a stall of
a couple of seconds. They carry `ControlMaster=no` and the path of the host's
first master, so they join it when it is up and otherwise open their own
direct connection.

If the target's `extra_args` already set `ControlMaster`, `ControlPath`, or
`-S`, Mjolnir adds no sharing options at all for that target and leaves
sharing to you. A `-J` jump host in `extra_args` is used when a master is
opened. To turn sharing off entirely, set `MJ_SSH_CONTROL_MASTER=0` (`off`,
`false`, and `no` also work) in the daemon's environment; every command then
opens its own connection. Sharing is unix-only; Windows OpenSSH does not
implement it.

## Limiting concurrent connections

Mjolnir starts one `ssh` or `scp` process per remote operation, so a daemon
restart with many sessions opens a burst of connections to the same host at
once. To keep that burst from being refused, Mjolnir admits at most **6
concurrent connections per destination** and makes the rest wait. Set
`MJ_SSH_MAX_CONCURRENT` in the daemon's environment to change the limit; an
unset or invalid value falls back to 6.

The number to compare it to is the remote `sshd`'s `MaxStartups`, which counts
*unauthenticated* connections. Its stock value, `10:30:100`, starts randomly
dropping connections at the eleventh concurrent pre-auth connection and drops
all of them past a hundred. Keep `MJ_SSH_MAX_CONCURRENT` comfortably below the
first number, or raise `MaxStartups` on the host. A dropped connection exits
255 with `Connection closed by <host> port 22` or
`kex_exchange_identification: read: Connection reset by peer`; Mjolnir
recognizes those and retries the invocation rather than reporting a failure,
because the remote command never ran.

Connection sharing removes most of this pressure, because a master
authenticates once for up to 8 sessions. The concurrency limit still applies
to opening masters, and to every command when sharing is off.

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
  SSH user must be able to create a branch and `.mj/worktrees/` below the
  repository. If you select its primary checkout, that checkout must be fully
  clean, including staged, unstaged, and untracked files.
- For a Podman runtime: **rootless Podman on the remote host**, meeting the same
  postconditions Mjolnir expects locally. See [Podman for Mjolnir](PODMAN.md) — the
  remote host needs Podman 4.3 or newer and the same rootless
  user-namespace setup as a local Podman host.

## Machine and runtime configuration

The SSH host is one `[machines.<id>]` entry, and every runtime on it names that
machine. The machine's keys are:

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

### How a remote bare project is prepared

The new-session wizard asks for an existing absolute Git directory on the SSH
host. Mjolnir validates that path remotely; it does not clone a configured
bundle into it.

If the path belongs to the repository's primary checkout, Mjolnir requires the
whole checkout to be clean and creates branch `mj/<session-id>` in a linked
worktree at `<repository>/.mj/worktrees/<session-id>`. “Clean” includes
untracked files; `git stash` without `--include-untracked` is not enough. If you
select an existing linked worktree, Mjolnir uses that checkout directly instead
of creating another one.

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
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

## Verifying a target

`mj doctor` includes a dedicated check per runtime on an SSH machine. It
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
