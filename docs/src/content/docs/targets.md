---
title: Targets
description: Compare local, container, SSH, and AWS execution targets and choose the right isolation, project, resource, and lifecycle model.
---

A runtime is a named execution template under `[targets.<id>]`, and it names
the machine it runs on under `[machines.<id>]`. A session combines one
[profile](/profiles/), one target, and either a configured
[bundle](/workspaces-bundles/#bundles-define-managed-projects) or an existing
Git project directory. The target decides where work runs, what isolation
contains unrestricted agent actions, how the workspace is created, and what is
removed after a verified stop.

Local target choices are supplied automatically and checked live when you open
the target picker. Add SSH and AWS connections or customize runtime defaults in
**prefix+s Settings → Machines** and **prefix+s Settings → Runtimes**. No setup
command is required. A target that is checking or unavailable cannot advance;
**prefix+shift+r** in the picker rechecks all targets. Dashboard
**prefix+shift+r** refreshes host capacity. So does **Refresh** in the menu
under the dashboard's Targets title (click the title, or press `.` while the
pane has focus). The same menu's **Runtimes…** opens Settings → Runtimes, and
its **Machines…** opens Settings → Machines. The optional `mj doctor --smoke`
command exercises container creation and removal.

## Capability matrix

| Runtime | Machine | Isolation boundary | New-session project | Supplemental directories | Resource choice | Target lifecycle |
| --- | --- | --- | --- | --- | --- | --- |
| `bare` | `local` (Linux or macOS controller machine) | none | Existing local directory, optionally an isolated Git clone | no | host-owned | Machine and user-selected directory persist; a managed clone is archived and retired on suspend. |
| `bare` | an `ssh` machine (named remote Linux host) | none beyond host/account | Existing remote Git directory, optionally an isolated clone | no | host-owned | Host and user-selected directory persist; the managed clone and staging areas are retired on suspend. |
| `bare` | an `aws-ec2` machine (your AWS account) | disposable EC2 instance | Bundle | controller-side directory snapshot | EC2 instance type | Instance is terminated after verified stop. |
| `podman` | `local` (Linux/WSL2) | rootless container | Bundle | copy-on-write or read-only mounts | CPU and memory | Container and workspace storage are removed after verified stop. |
| `podman` | an `ssh` machine | rootless container | Bundle | remote-host copy-on-write or read-only mounts | CPU and memory | Remote container and workspace storage are removed after verified stop. |
| `docker` | `local` with a Linux Docker daemon (including a VM on macOS), or an `ssh` machine | Docker container | Bundle | copy-on-write or read-only OverlayFS views | CPU and memory | Container and managed workspace volume are removed after verified stop. |
| `apple-container` | `local` (Apple-silicon macOS 26+) | Apple container VM | Bundle | read-only mounts | CPU and memory | Container is removed after verified stop. |

“Verified stop” means Mjolnir has created a recovery archive and checked its
SHA-256 on the controller before tearing the resource down. See
[Durability and recovery](/durability/).

## Harness versions

Mjolnir installs and caches its pinned harness runtimes for every bare session,
on this machine, on an SSH machine, and on an EC2 instance. Every session uses
a staged copy of the selected profile's allowlisted files and credentials,
while its runtime is managed independently
of commands such as `codex` installed for native terminal use. Containers use
the runtimes supplied by their image.

New workers use the versions shipped with your Mjolnir build. Upgrade an
existing session's worker to adopt those versions; running workers retain their
current runtime until upgraded.

## Execution policy

Mjolnir selects approval behavior from the runtime, then translates it into the
chosen harness's controls:

| Runtime | Effective policy |
| --- | --- |
| `bare` on `local` | Preserve the profile and harness's configured approvals. |
| `bare` on an SSH machine, `permissions = "guardian"` | Preserve configured approvals. |
| `bare` on an SSH machine, `permissions = "yolo"` | Unconstrained. |
| Every container runtime, and a bare runtime on an EC2 machine | Unconstrained inside the isolation boundary. |

The unconstrained translation is Codex `agent-full-access`, Claude Code
`bypassPermissions` with its sandbox disabled, Kimi Code `auto`, and Grok Build
always-approve with its sandbox disabled. These all approve every action; Kimi's
mode happens to be named `auto` but is not a risk-selective guardian. Muse uses
the staged `:unrestricted` permission profile, `allowAll`, and
`--disable-sandbox` on every target, overriding the policy in the table.

Codex, Claude Code, and Grok Build can preserve guardian approvals on raw
targets. Kimi Code and Muse Code cannot, so Mjolnir displays a prominent warning when either is
selected without an isolation boundary. Read [Security boundaries](/security/)
before choosing a raw or `yolo` target.

## Bare runtimes

A bare runtime selects an existing absolute Git project directory rather than a
bundle. They do not accept supplemental directory attachments or container
resource sizing.

### Bare on this machine

```toml
[targets.localhost]
kind = "bare"
```

`machine` defaults to `local`, so it is omitted.

The directory must exist locally. For a Git project, the final review offers
**Create isolated checkout**. It is checked by default for a primary checkout
and unchecked for an existing linked worktree; you can change either choice.
When checked, Mjolnir creates a separate checkout under
`.mj/clones/<session-id>` as an independent clone, starting on the remote default
branch (or the source's current branch without a remote) and preserving the selected
subdirectory. Its commits and branch changes do not move refs in the source checkout.
Uncommitted source changes stay in the source checkout.

Uncheck it to use the selected directory directly. The choice survives stop
and resume. Plain directories are used directly with the checkbox disabled.

Local bare runs on a Linux or macOS controller. It uses the native
`mj-worker` installed beside `mj`, so on macOS no Linux worker is needed for
it. Container and remote targets still need a static Linux worker; see
[Install](/install/).

There is no process, filesystem, or network isolation between the harness and
your controller account. The harness runs from a staged copy of the configured
profile home, as on every target, but that copy isolates only its own state: the
harness can still read your whole home directory, the profile home included. Use
this target only when you trust both the agent and its approval configuration.

### Bare on an SSH machine

```toml
[machines.builder]
kind = "ssh"
host = "builder.example.com"
user = "ubuntu"
workspace_prefix = ".local/share/hel/workspaces"

[targets.builder]
kind = "bare"
machine = "builder"
permissions = "guardian"
```

The wizard validates an existing Git directory on the remote host. The same
**Create isolated checkout** choice and defaults apply on that host. The remote
machine persists across sessions. Mjolnir-created clones and worker/profile staging
areas are lifecycle-managed; a linked worktree you selected yourself remains
yours. `workspace_prefix` controls a separate per-session lifecycle/cleanup
path, not the selected project or its linked-worktree location.

The host does not need a preinstalled harness bridge. Its worker installs and
reuses the exact harness version pinned by Mjolnir in the remote user's cache.
It does require Node.js 22 and npm for Codex and Claude, or curl and Bash for
Kimi and Grok, or curl and tar for Muse. Mjolnir never uses sudo to add these prerequisites and does not
fall back to another harness executable from the remote `PATH`.

`permissions` accepts `guardian` or `yolo` and defaults to `guardian`. The
machine's SSH connection fields include `identity_file` and `extra_args`. See
[SSH and SSH Podman](/ssh/) for host prerequisites, connection checks, and
workspace cleanup.

## Container targets

All container targets use an image and accept optional `pull_policy`,
`platform`, `cpus`, `memory`, and target `environment`. If `image` is omitted,
they use this published multi-architecture default:

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
kind = "podman"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

Rootless Podman 4.3 or newer is the reference Linux/WSL2 runtime. It is the only
runtime with configurable workspace backing: a named volume by default, the
container layer, or a host path managed through an operator-supplied helper.
See [Podman](/podman/).

### Docker

```toml
[targets.docker]
kind = "docker"
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

### Podman on an SSH machine

```toml
[machines.builder]
kind = "ssh"
host = "builder.example.com"
user = "ubuntu"

[targets.remote-podman]
kind = "podman"
machine = "builder"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

The machine carries the SSH connection; the runtime carries every Podman
container field. The container and any workspace volume live on the remote host. Supplemental
directory sources are therefore paths on that remote host, not paths on the
controller. See [SSH and SSH Podman](/ssh/).

## AWS EC2

```toml
[machines.fleet]
kind = "aws-ec2"
aws_profile = "default"
region = "eu-west-1"
launch_template = "lt-0123456789abcdef0"
ssh_user = "ubuntu"
address_source = "public-dns"

[targets.aws]
kind = "bare"
machine = "fleet"
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
root. Every repository clones its network fetch remote's default branch. A
controller-side `local` path supplies only its configured network fetch and
push destinations; no Git connection back to that checkout is created.

Local and SSH bare sessions instead choose an existing project directory.
A single-repository isolated session can move into a raw local worktree when
its source repository is available and contains the archive's prerequisite
history.

A local session can move or resume the other way, into an isolated target, when
its checkout is a whole Git checkout on this machine with a network remote
(`https` or `ssh`). The target clones that remote and the session's unpushed
commits, staged, unstaged, and untracked files are restored over the clone. A
checkout with no network remote stays bare-only: add a remote (`git remote add
origin <url>`) or keep resuming on a bare target. A subdirectory of a checkout,
an SSH-hosted checkout, and a multi-repository bundle cannot become one
checkout either way.

Muse Code supports one ACP workspace root, so it requires a single-repository
bundle or one existing bare project directory, with no supplemental directories.
See [Workspaces and bundles](/workspaces-bundles/) for repository validation,
dirty state, Git caching, and project memory.

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
`config.toml`. They can be changed for the next recreation through the
command palette (**prefix+:**) → **Container settings** where supported. See [Session lifecycle](/sessions/).

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
| `auto` | Use the existing image. A missing image is downloaded when the daemon starts; eligible moving tags are also refreshed hourly. Apple resolves it during provisioning. |
| `always` | Refresh during launch. |
| `newer` | Refresh when the runtime supports a newer-only check; Docker treats it as `always`. |
| `missing` | Pull only if absent. |
| `never` | Never pull; fail if absent. |

Running containers are never replaced in place. A refreshed image is used by
the next new or recreated session. Digest references remain pinned and
versioned tags remain cached under `auto`: the daemon downloads either one once
if the host lacks it, then only checks that it is still there. A `localhost/`
or `local/` image cannot be downloaded from a registry, so a missing one is
reported once as a failure rather than retried into a notice every hour. Only
`never` keeps an image out of the daemon's startup download.

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
