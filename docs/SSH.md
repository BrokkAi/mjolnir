# SSH targets: `ssh-bare` and `ssh-podman`

Mjolnir has two target kinds that run a session on a remote machine over SSH:

- `ssh-bare` — clones the project directly into a workspace directory on the
  remote machine and runs the harness there.
- `ssh-podman` — starts a rootless Podman container on the remote machine
  (same model as `local-podman`, just reached over SSH) and runs the session
  inside it.

Both shell out to the local `ssh` CLI (`src/hel_targets/ssh.rs`, `ssh_command`/
`ssh_command_owned`); Mjolnir does not use an SSH library or persistent
connection multiplexing of its own.

## Prerequisites you set up by hand

- **Key-based SSH that works non-interactively.** Mjolnir runs `ssh` without a
  pseudo-terminal and does not prompt for a password or passphrase, so the
  target user must already accept your key without interaction (an unlocked
  key, `ssh-agent`, or a passphrase-free key).
- **The host key already trusted.** Add the remote host to `known_hosts` (or
  otherwise satisfy your SSH host-key policy) before pointing a target at it;
  Mjolnir does not manage `known_hosts` for you.
- For `ssh-podman`: **rootless Podman on the remote host**, meeting the same
  postconditions Mjolnir expects locally. See [Podman for Mjolnir](PODMAN.md) — the
  remote host needs Podman 4.0 or newer and the same rootless
  user-namespace setup as a local `local-podman` host.

## Target configuration

Verified against `SshConnection` and the `SshBare`/`SshPodman` variants of
`TargetTemplate` in `src/hel_config.rs`.

Shared SSH connection keys (flattened into both target kinds):

| Key | Required | Notes |
| --- | --- | --- |
| `host` | yes | SSH destination: hostname, IP, or an alias from your SSH config. |
| `user` | no | SSH login user; omit to use your SSH config / default. |
| `identity_file` | no | Path to the private key. |
| `extra_args` | no | Extra arguments appended to every `ssh` invocation for this target. |

`ssh-bare` also takes:

| Key | Required | Notes |
| --- | --- | --- |
| `permissions` | yes | `guardian` preserves configured harness approvals; `yolo` runs unconstrained. |
| `workspace_prefix` | no | Remote directory session workspaces are created under. Defaults to `.local/share/hel/workspaces` (relative to the login home). |

```toml
[targets.builder]
kind = "ssh-bare"
host = "builder"
permissions = "guardian"
workspace_prefix = ".local/share/hel/workspaces"
```

`ssh-podman` always runs unconstrained and does not accept `permissions`. It
also takes the same container keys as `local-podman` (`image`, and optionally
`platform`, `cpus`, `memory`, `environment`, `pull_policy`,
`workspace_storage`):

```toml
[targets.builder-podman]
kind = "ssh-podman"
host = "builder"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

## Verifying a target

`mj doctor` includes a dedicated check per SSH target of either kind. It
first probes connectivity with `ssh -o BatchMode=yes <host> true`, and its
failure messages include the exact command to fix the common causes
(`ssh-copy-id` for key auth, `ssh-keyscan` for an untrusted host key). For
`ssh-podman` it then runs the same Podman probes as a local target, over SSH.

You can also always sanity-check
reachability by hand before relying on a target:

```console
ssh <host> true
```

If that succeeds non-interactively, Mjolnir's own SSH invocations should too.
