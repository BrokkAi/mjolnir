---
title: Targets
description: Compare local, container, SSH, and AWS execution targets and choose the right isolation, project, resource, and lifecycle model.
---

A target is a named execution template. A session combines one
[profile](/profiles/), one target, and either a configured
[bundle](/workspaces-bundles/#bundles-define-managed-projects) or an existing
Git project directory. The target decides where work runs, what isolation
contains unrestricted agent actions, how the workspace is created, and what is
removed after a verified stop.

`mj setup` creates a `localhost` raw target and one target for each usable local
container runtime it discovers. Add SSH and AWS targets during setup or edit
`config.toml` directly. Press `F5` in the dashboard to refresh target capacity,
or run `mj doctor --smoke` for end-to-end checks of local Podman, local Docker,
Apple Container, and SSH Podman targets. Bare and AWS targets do not have smoke
tests.

## Capability matrix

| Kind | Runs on | Isolation boundary | New-session project | Supplemental directories | Resource choice | Target lifecycle |
| --- | --- | --- | --- | --- | --- | --- |
| `local-bare` | Linux controller machine | none | Existing local Git directory | no | host-owned | Machine persists; managed session worktree is archived and retired on stop. |
| `local-podman` | Local Linux/WSL2 | rootless container | Bundle | copy-on-write or read-only mounts | CPU and memory | Container and workspace storage are removed after verified stop. |
| `local-docker` | Local Linux/WSL2 | Docker container | Bundle | copy-on-write or read-only OverlayFS views | CPU and memory | Container and managed workspace volume are removed after verified stop. |
| `apple-container` | Apple-silicon macOS 26+ | Apple container VM | Bundle | read-only mounts | CPU and memory | Container is removed after verified stop. |
| `ssh-bare` | Named remote Linux host | none beyond host/account | Existing remote Git directory | no | host-owned | Host persists; per-session worktree/workspace is archived and retired. |
| `ssh-podman` | Named remote Linux host | rootless container | Bundle | remote-host copy-on-write or read-only mounts | CPU and memory | Remote container and workspace storage are removed after verified stop. |
| `aws-ec2` | Your AWS account | disposable EC2 instance | Bundle | controller-side directory snapshot | EC2 instance type | Instance is terminated after verified stop. |

“Verified stop” means Mjolnir has created a recovery archive and checked its
SHA-256 on the controller before tearing the resource down. See
[Durability and recovery](/durability/).

## Execution policy

Mjolnir selects approval behavior from the target, then translates it into the
chosen harness's controls:

| Target | Effective policy |
| --- | --- |
| `local-bare` | Preserve the profile and harness's configured approvals. |
| `ssh-bare`, `permissions = "guardian"` | Preserve configured approvals. |
| `ssh-bare`, `permissions = "yolo"` | Unconstrained. |
| Every container and EC2 target | Unconstrained inside the isolation boundary. |

The unconstrained translation is Codex `agent-full-access`, Claude Code
`bypassPermissions` with its sandbox disabled, Kimi Code `auto`, Grok Build
always-approve with its sandbox disabled, and DeepSeek Harness
`danger-full-access`. These all approve every action; Kimi's mode happens to be
named `auto` but is not a risk-selective guardian.

Codex, Claude Code, and Grok Build can preserve guardian approvals on raw
targets. Kimi Code and DeepSeek Harness cannot, so Mjolnir displays a prominent
warning when either is selected without an isolation boundary. Read
[Security boundaries](/security/) before choosing a raw or `yolo` target.

## Bare targets

Bare targets select an existing absolute Git project directory rather than a
bundle. They do not accept supplemental directory attachments or container
resource sizing.

### Local bare

```toml
[targets.localhost]
kind = "local-bare"
```

The directory must exist locally and have a valid Git `HEAD`. When it is the
repository's primary checkout, Mjolnir creates a session-specific linked
worktree under `.mj/worktrees/<session-id>` on branch `mj/<session-id>`. This
keeps concurrent sessions off the primary checkout while retaining ordinary
local Git object sharing. If the selected path is already a linked worktree,
Mjolnir uses it as selected. Creating a managed worktree requires the primary
checkout to be completely clean, including staged, unstaged, and untracked
files.

Although the controller and viewer support macOS, current `local-bare` worker
launch requires Linux. Use Apple Container or a remote target for sessions from
a macOS controller.

There is no process, filesystem, or network isolation between the harness and
your controller account. The harness also uses the configured profile home
directly. Use this target only when you trust both the agent and its approval
configuration.

### SSH bare

```toml
[targets.builder]
kind = "ssh-bare"
host = "builder.example.com"
user = "ubuntu"
permissions = "guardian"
workspace_prefix = ".local/share/hel/workspaces"
```

The wizard validates an existing Git directory on the remote host. Primary
checkouts are isolated with the same linked-worktree model on that host and
must first be completely clean, including untracked files. The remote machine
persists across sessions. Mjolnir-created worktrees and worker/profile staging
areas are lifecycle-managed; a linked worktree you selected yourself remains
yours. `workspace_prefix` controls a separate per-session lifecycle/cleanup
path, not the selected project or its linked-worktree location.

The host does not need a preinstalled harness bridge. Its worker installs and
reuses the exact harness version pinned by Mjolnir in the remote user's cache.
It does require Node.js 22 and npm for Codex, Claude, and DeepSeek, or curl and
Bash for Kimi and Grok. Mjolnir never uses sudo to add these prerequisites and
does not fall back to another harness executable from the remote `PATH`.

`permissions` is required and accepts `guardian` or `yolo`. SSH connection
fields include `identity_file` and `extra_args`. See
[SSH and SSH Podman](/ssh/) for host prerequisites, connection checks, and
workspace cleanup.

## Container targets

All container targets require an image and accept optional `pull_policy`,
`platform`, `cpus`, `memory`, and target `environment`. The published default is
multi-architecture:

```toml
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
pull_policy = "auto"
```

It carries the supported bridges and common development tools. A plain image
can work, but every new session may need to install Git, GitHub CLI, Node, or a
harness bridge. See [Container targets](/containers/) and
[Custom images](/custom-images/).

### Podman

```toml
[targets.podman]
kind = "local-podman"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

Rootless Podman 4.0 or newer is the reference Linux/WSL2 runtime. It is the only
runtime with configurable workspace backing: a named volume by default, the
container layer, or a host path managed through an operator-supplied helper.
See [Podman](/podman/).

### Docker

```toml
[targets.docker]
kind = "local-docker"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

Docker requires a reachable Linux daemon. Mjolnir owns a managed volume for the
session workspace and uses managed OverlayFS volumes for writable supplemental
directories. Podman's `workspace_storage` override is not accepted. See
[Docker](/docker/).

### Apple container

```toml
[targets.apple]
kind = "apple-container"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
platform = "linux/arm64"
```

Apple's `container` CLI requires Apple silicon and macOS 26 or newer.
Supplemental directories are mounted read-only; writable OverlayFS views and
Podman workspace-storage overrides are unavailable. See
[Apple container](/apple-container/).

### Podman over SSH

```toml
[targets.remote-podman]
kind = "ssh-podman"
host = "builder.example.com"
user = "ubuntu"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

This combines the SSH connection fields with every Podman container field. The
container and any workspace volume live on the remote host. Supplemental
directory sources are therefore paths on that remote host, not paths on the
controller. See [SSH and SSH Podman](/ssh/).

## AWS EC2

```toml
[targets.aws]
kind = "aws-ec2"
aws_profile = "default"
region = "eu-west-1"
launch_template = "lt-0123456789abcdef0"
ssh_user = "ubuntu"
address_source = "public-dns"
```

Mjolnir launches one instance per session from an existing launch template,
discovers its configured public or private DNS/IP address, connects over SSH,
and terminates it only after the recovery archive verifies. The launch template
owns the AMI, networking, IAM, security groups, storage, and default instance
type. The new-session wizard may select another allowed instance type for that
session.

Supplemental controller-side directories are transferred as point-in-time
snapshots. They are not live mounts and changes on the instance do not write
back to the source directory. See [AWS EC2](/aws/) for the required AWS CLI,
permissions, launch-template contract, and connectivity.

## Bundles, clones, and workspace roots

Managed targets—Podman, Docker, Apple Container, SSH Podman, and EC2—clone a
configured bundle into a new session workspace. `primary_repo` is the agent's
working directory and every other repository is an additional ACP workspace
root. GitHub repositories clone normally; controller-side `local` repositories
use Mjolnir's confined Git bridge.

Local and SSH bare sessions instead choose an existing project directory.
Moving or resuming a stopped single-repository local session can convert between
the bundle and bare representations when the destination supports it; a
multi-repository bundle cannot become one checkout.

DeepSeek Harness supports one ACP workspace root, so it requires a
single-repository bundle or one existing bare project directory, with no
supplemental directories. See [Workspaces and bundles](/workspaces-bundles/)
for repository validation, dirty state, Git caching, and project memory.

## Supplemental directories

The terminal new-session and resume flows can attach directories that are not
part of the bundle. Each attachment has an absolute source, a unique safe
absolute destination, and a read-only choice. The default destination is
`/mnt/<source-name>`.

| Target | Source location | Writable request |
| --- | --- | --- |
| Local Podman | Controller host | Podman's isolated overlay view. |
| Local Docker | Controller host | Managed OverlayFS volume. |
| Apple Container | Controller host | Downgraded to read-only. |
| SSH Podman | Remote container host | Podman's isolated overlay view. |
| AWS EC2 | Controller host | Snapshot copy; no write-back. |
| Local/SSH bare | — | Unsupported. |

For Podman and Docker, NFS, SMB, FUSE, FAT-family, and existing overlay
filesystems cannot safely host the requested overlay. Mjolnir detects these and
forces the attachment read-only, reporting the reason. Sources must be absolute
directory paths, and destinations must be unique safe absolute paths without
parent traversal.

Attachments are per-session state in `mj.sqlite3`, not target fields in
`config.toml`. They can be changed for the next recreation through
**F2 → Container settings** where supported. See [Session lifecycle](/sessions/).

## CPU, memory, and instance sizing

Container target `cpus` and `memory` values are backend fallbacks for launch
requests that do not supply a per-session allocation. The terminal wizard does
not seed from them: it starts with the remembered size for that physical host
or a baseline of 8 CPUs and 32 GiB, then clamps choices to known host limits.
Its per-session choice overrides the fallback. Later **Container settings**
overrides win when that session's container is next created.

EC2 uses an instance type rather than independent CPU and memory strings. Bare
targets use the host directly and have no Mjolnir resource limit.

These choices are capacity controls, not scheduling constraints. Mjolnir shows
host and fleet usage but does not prevent overcommit.

## Image pull policy

Container `pull_policy` accepts:

| Value | Launch behavior |
| --- | --- |
| `auto` | Use the existing image; pull when missing. Eligible moving tags are refreshed in the background for Podman, Docker, and SSH Podman. Apple resolves it during provisioning. |
| `always` | Refresh during launch. |
| `newer` | Refresh when the runtime supports a newer-only check; Docker treats it as `always`. |
| `missing` | Pull only if absent. |
| `never` | Never pull; fail if absent. |

Running containers are never replaced in place. A refreshed image is used by
the next new or recreated session. Digest references remain pinned, versioned
tags remain cached under `auto`, and local image names are not background
refreshed.

## Target environment versus profile environment

For container targets, `[targets.<id>.environment]` becomes environment inside
the target container. `[profiles.<id>.environment]` configures the harness and
ACP bridge. AWS and bare target variants do not accept a target environment
table, but profile environment still applies to their workers.

Do not put secrets in either table casually: `config.toml` is plain text. Use
the harness login flow for provider credentials and the controller's GitHub
token flow for GitHub access. See [Profiles and harnesses](/profiles/).

## Full field reference

The examples above intentionally omit optional connection and storage fields.
Use the [Configuration reference](/configuration/) for every accepted field,
exact defaults, and validation constraints. Use
[Troubleshooting](/troubleshooting/) when a target appears configured but fails
its doctor check or launch.
