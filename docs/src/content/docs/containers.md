---
title: Container targets
description: Set up a disposable container target for Mjolnir and start your first isolated session.
---

## What container targets give you

Each session on a container target runs in its own disposable, labeled
container: local Podman on Linux or WSL2, Docker with a reachable Linux daemon
(including a VM on macOS), Apple's `container` runtime on macOS 26 or newer on
Apple silicon, or Podman or Docker over SSH. Container
isolation always
selects Mjolnir's `unconstrained` execution policy. The `permissions` setting is
only available for a bare runtime on an SSH machine. Mjolnir translates the policy into the
selected harness's own control: Codex `agent-full-access`, Claude Code
`bypassPermissions`, Kimi Code `auto`, Grok Build's `--always-approve` launch
flag, or OpenCode's `"permission": "allow"` config setting. Muse uses
`allowAll`, `--disable-sandbox`, and the staged
`:unrestricted` profile. Every one of those approves every call. Note that Kimi Code's
mode is named `auto` but is not a guardian policy that approves only low-risk
calls.

Bare localhost sessions preserve configured approvals for supported harnesses.
Codex, Claude Code, Grok Build, OpenCode, and Muse Code expose guardian modes;
Kimi Code does not. Mjolnir warns against running an unsupported harness on a
raw, unsandboxed target.

A container session's repository content always comes from a network clone. A
local session that runs the agent in a directory on this machine can still move
or resume into a container: Mjolnir re-snapshots that checkout, the container
clones the checkout's own network remote, and the snapshot restores the
checkout's unpushed commits and its staged, unstaged, and untracked files over
the clone at `/workspace/<session id>/<directory name>`. Each session gets its
own directory under `/workspace`, so two sessions on one host never work at the
same path; sessions whose container was created before this version keep the
shared `/workspace`. Files Git ignores and anything
outside the checkout do not travel, and a checkout with no network remote cannot
become a container workspace. See
[Resume a local session into a container](/sessions/#resume-a-local-session-into-a-container).

Suspending a session first writes and verifies a recovery archive, then removes
that exact container. No mutable session workspace persists past the session
except what the recovery archive captured and whatever you pushed to a
remote. Mjolnir may retain read-only Git objects in the host clone cache described
below.

Podman and Docker sessions, local or over SSH, mount a separate private
disk-backed volume at `/tmp`. Temporary files use the host filesystem directly
instead of the container's writable overlay, without consuming a tmpfs memory
allocation. The directory has the usual `1777` permissions and works with
nonroot image users. An explicitly attached directory at `/tmp` keeps its
configured behavior.

Temporary files are excluded from checkpoints. Mjolnir removes the temporary
volume after removing the container; stopping or restarting the session loses
its contents. After upgrading Mjolnir, **Restart session** applies this mount to
an existing Podman or Docker session by checkpointing it and restoring it into
a newly provisioned container. A daemon or worker process restart alone does
not change an existing container's mounts.

For Podman sessions using a workspace volume or host-managed workspace storage,
the mount covers `/workspace/<session id>`; the parent `/workspace` directory
itself remains on the container's writable overlay. Legacy sessions whose
recorded workspace is `/workspace` keep that mount location. Keep durable
results inside project repository directories: arbitrary files elsewhere on
the workspace volume are not checkpointed.

## Prerequisites

Install each runtime you want to use as a target:

- **Rootless Podman 4.0 or newer** on Linux or WSL2. See
  [Podman for Mjolnir](/podman/) for installation and verification steps.
- **Docker with a reachable Linux daemon**, including Colima on macOS. See
  [Docker for Mjolnir](/docker/) for its OverlayFS and lifecycle contract.
- **Apple's `container` CLI** on macOS 26 or newer on Apple silicon.

Linux controller releases require glibc 2.28 or newer. Session workers are
separate static musl binaries: both Linux architectures are bundled beside
`mj` as `mj-worker-<arch>-unknown-linux-musl`. The controller selects and uploads
the worker matching the container or remote host. macOS bundles also include
a native `mj-worker` for local bare sessions.

## Get the agent-dev image

Mjolnir ships a reference container image with everything a session needs
pre-installed: Rust, cargo-nextest, Node 24, OpenJDK 25, Python 3 with its
native development headers and library, uv, Git, GitHub CLI, the Codex and
Claude ACP bridges, and Muse Code with `muse-acp`. Kimi, Grok, and
OpenCode install on demand. It also carries Playwright's Chromium system
libraries and the pre-installed Chromium headless shell in
`PLAYWRIGHT_BROWSERS_PATH=/ms-playwright`, so headless browser tests need no
privileged install and no run-time browser download, and the profiling tools
`perf`, `cargo-flamegraph`, `samply`, and `heaptrack`; `perf` additionally needs
the host's `kernel.perf_event_paranoid` set to 1 or lower, or a host/runtime
that already provides the required capability. Mjolnir does not expose a
container-capability override. Optional local coverage tooling includes the
`llvm-tools-preview` component, pinned `cargo-llvm-cov`, and `lcov` for
`genhtml`. It's published at
`ghcr.io/brokkai/mjolnir/agent-dev:latest`, public and
multi-arch for both `linux/amd64` and `linux/arm64`, so the same image name
works whether Mjolnir is running it through Podman, Docker, Apple's `container`
runtime, or an arm64 SSH host.

The standard local targets already use this published image. Podman, Docker,
and Apple's container runtime pull it automatically when first needed.

Building it yourself remains a supported alternative, for example to
customize the image or to work offline:

```console
podman build --pull=always \
  --file containers/Containerfile.agent-dev \
  --tag localhost/mjolnir/agent-dev:latest \
  .
```

## Choose a runtime in the UI

Create a session and choose a local runtime in the target picker. Mjolnir checks
its availability in the background and blocks unavailable choices. Start a
stopped service, then press **prefix+shift+r** to recheck. No `mj setup` command is required.

Use **prefix+s Settings → Runtimes** to override the container image,
resource defaults, or environment, or to add an SSH or EC2 connection. The
optional CLI setup command remains available.

A plain image such as `ubuntu:24.04` still works if you enter it here: Mjolnir
auto-installs Git, GitHub CLI, and Node the first time a session needs them.
But that installation runs inside every new container, which slows down the
start of each session. The default agent-dev image avoids that cost.

Container targets default to `pull_policy = "auto"`. Podman and Docker launches
do not wait on a registry under that default: they start from the image the host
already has and pull only when the host has no copy at all.

A few seconds after the Mjolnir daemon starts, it downloads every configured
container image the host does not have, so your first session does not wait on
the registry. Local Podman, local Docker, SSH Podman, SSH Docker, and Apple
container all take part. The dashboard shows "Downloading image ..." while a
download runs and "Image ... is ready" when it finishes. If you create a
session while its image is still downloading, the session shows a "Pull image"
stage and waits for that download instead of starting a second one.

The daemon also refreshes eligible remote `:latest` images once an hour and
removes the dangling images each pull leaves behind. Versioned tags and digest
references are downloaded once if absent and then only checked, because their
content cannot change. A `localhost/...` image cannot be downloaded at all; if
it is missing, the daemon reports that once rather than every hour.

Set `pull_policy` beside `image` to `always`, `newer`, `missing`, or `never`
when a target needs an explicit policy. On Podman and Docker, `always` or
`newer` pulls during launch and is refreshed hourly in the background. Apple
evaluates the policy during provisioning. Only `never` keeps an image out of
the startup download entirely. Existing running containers are never replaced
in place.

## Git clone cache

Local Podman, local Docker, SSH Podman, SSH Docker, and Apple container targets cache GitHub repository
objects under the container host user's `~/.cache/mjolnir/git`. Before launch, Mjolnir
refreshes a bare mirror and creates an isolated session snapshot whose
immutable objects are shared with ordinary filesystem hardlinks. The snapshot
is mounted read-only and the normal in-container clone borrows its objects, so
branch selection, checkout filters, and image-specific Git behavior remain
unchanged.

This is an optimization rather than a prerequisite. If host Git, credentials,
or local hardlink cloning are unavailable, Mjolnir reports the cache miss and uses
the ordinary network clone. The first launch still populates the complete
mirror. Mjolnir removes session snapshots after their container, removes mirrors
unused for 30 days, and enforces a 20 GiB least-recently-used soft cap. The
cache can contain objects from private repositories and is created with
user-only permissions. You can remove `~/.cache/mjolnir/git/mirrors` while no
launch is updating it; do not remove the `sessions` directory while managed
containers are running.

It then shows a summary of what it's about to write and asks you to confirm
before writing `config.toml`. After you confirm, it runs a smoke test: it
creates a disposable container from the configured image, runs a trivial
command in it, and removes it, to prove the runtime actually works before you
start a real session.

## Verify with `mj doctor`

```console
mj doctor --json
```

This prints a machine-readable array of prerequisite checks. Resolve every
check reported as `fixable` — each one includes what's wrong and how to fix
it — then run `mj doctor --json` again. Repeat until none remain. The set of
checks Mjolnir runs is still growing, so treat the `fixable` status as
authoritative rather than checking for specific check names.

Once every check passes, run the same command with `--smoke` for an
end-to-end test: it creates and removes a disposable container, confirming
the full path works beyond static prerequisite checks. For Docker, this also verifies that a temporary writable
attachment is copy-on-write and that its managed OverlayFS volume cleans up.

```console
mj doctor --json --smoke
```

## First session

```console
mj
```

This opens Mjolnir's terminal surface. Press **Create** or **prefix+c** from
anywhere for the full new-session wizard.
It walks you through picking a profile, a target, and a bundle.

Before launch, you can size the container's CPU and memory allocation. The
wizard starts with the allocation remembered for that physical host, or with
8 CPUs and 32 GiB when no allocation has been remembered:

| Key | Effect |
| --- | --- |
| `+` | Doubles the current allocation |
| `-` | Halves the current allocation |
| `c` | Adds 8 CPUs |
| `m` | Adds 50% memory |
| `r` | Resets to the 8-CPU/32-GiB baseline |

The wizard ends on a review screen where you can add, edit, or remove
attached directories before launch. Each attached directory has an access
mode:

| Mode | Effect |
| --- | --- |
| `ro` (default) | The container can read the directory but not change it. |
| `cow` | The container writes to a private copy-on-write overlay. Your host directory never changes. |
| `rw` | The container's writes go straight to your host directory. |

Podman and Docker build `cow` from OverlayFS, which some filesystems cannot
host. When Mjolnir finds a source on NFS, SMB, FUSE, a FAT-family filesystem,
or another overlay, the wizard doesn't offer `cow` for it, and an existing
`cow` attachment is mounted read-only instead, with a notice while the
session launches.

New rootless Podman session containers run as uid and gid `0:0` in Podman's
default user namespace. Container uid 0 maps to the account running Mjolnir,
so read-write attachments, the workspace, and the mbx cache are writable and
files created there belong to you on the host. Mjolnir sets
`HOME=/home/hel` when creating these containers. A root login shell can reset
`HOME` to `/root`, the root account's passwd home; Mjolnir writes inherited Git
identity and behavior settings to `/home/hel/.gitconfig`. The worker's
session-global Git config includes that file by absolute path, independent of
the worker or harness `HOME`. The include path is stored with the worker launch
settings and is applied when a worker restarts, resumes, or upgrades. Harness
profiles remain staged under `/var/lib/hel/profiles/<session>`. This avoids
copying and changing ownership of image layers. Existing containers keep the
user and HOME recorded when they were created; Podman exec uses those saved
defaults when a stopped legacy session resumes, and the same absolute Git
config include applies. Docker and Apple Container keep their existing
image-user behavior.

## Build cache (mbx)

Rust sessions on Podman and Docker container targets can share one
[mbx](https://github.com/jdx/mr-boxington) build cache per container host when
that Linux host has mbx 1.22.0 or newer installed. mbx wraps Cargo: it looks each compiler action up in a
content-addressed store and restores the cached output instead of recompiling.
The second session to build a project on a host reuses the first one's work.

Mjolnir turns this on for a session when all of the following hold:

- The target is Podman or Docker, local or over SSH. Apple `container`, bare
  targets, and EC2 targets never get a build cache.
- The primary repository has a `Cargo.toml` at its root. A Cargo workspace in
  a subdirectory is not detected.
- The resolved cache directory is on a filesystem that supports reflinks, which
  is what makes restoring a cached output nearly free.
- The container host has mbx 1.22.0 or newer. A missing or older installation
  leaves sessions on that host without the shared cache.

The cache directory lives on the container host and is mounted read-write into
the container at the same absolute path. Mjolnir atomically copies the resolved
host executable to `<cache>/.mjolnir/bin/mbx` inside that directory, then checks
that this copy runs in the container and reports the expected version before
installing mbx's Cargo launcher. Session start, resume, and periodic machine
reconciliation refresh the copy when the native mbx version or file size
changes. A replacement uses a same-directory temporary file and atomic rename,
so running processes keep using the inode they already opened. If native mbx
is absent or too old, the old copy is left in place and new sessions run
without the cache. Nothing is synchronized between hosts, and Mjolnir never
runs mbx garbage collection: the host's own mbx and the automatic collection
inside containers are the only collectors.

### Settings

Caching is enabled by default where supported. Configure it under
**Machines → [machine] → Build cache (mbx)** in Settings, or search for
**mbx**, **cache**, or **build cache**. Each machine controls the cache shared
by its container runtimes:

| Setting | Default when blank |
| --- | --- |
| Enabled | On when the host has compatible mbx and the cache filesystem supports reflinks. |
| Cache directory | The directory reported by the host's mbx configuration. A missing or too-old mbx does not get a fallback cache. |
| Cache size limit | The host's own mbx configuration. Configure limits on the machine with mbx. |
| Concurrent compile permits | The host's mbx scheduler setting, shared by all builds on the machine. |
| Compile admission memory budget | The host's mbx scheduler setting. This weights concurrent compile admission; it is not a hard memory cap. |

The two scheduler controls are under
`[machines.<id>.build_cache.scheduler]` in `config.toml`. Leave either blank
to use mbx's own default. The configuration reference documents the accepted
memory sizes and the `none` option.

Opening a machine's build cache page asks its host for these values, so each
blank field shows what a session there would actually use, such as
`/mnt/fast/mbx-cache`. When sessions on that host run without the cache, the
page says why. If mbx is missing or too old, install or upgrade it from
**Settings › Setup › Machines**.

`mj doctor` checks the native mbx version on each configured Podman or Docker
host when the build cache is enabled. A missing or too-old version produces a
warning that sessions will run without the shared cache and points to **Settings
› Setup › Machines**. The warning does not make doctor fail because the cache
is optional.

When the host has `~/.config/mbx/config.toml`, Mjolnir mirrors it into the
shared cache and links it into the container so mbx uses the host's own
settings. If that file
relocates `[target] root` outside the cache directory, that directory is
mounted read-write at its own path too.

Inside the session, `cargo` is a small launcher that invokes the shared-cache
mbx through its supported Cargo-shim mode; mbx finds the image's real Cargo
underneath. The session PATH also includes an `mbx` launcher for that same
cache copy.
`mbx stats` reports what the cache holds.

## Two useful facts

If the `gh` CLI on the machine running Mjolnir is authenticated, Mjolnir continuously
syncs its active GitHub token into every live non-local session. That includes
managed containers, EC2, SSH Podman, and raw SSH targets, and lets `gh` and
HTTPS Git pushes work without copying SSH keys. The token never goes into a
recovery archive. Raw SSH targets are therefore inside the token's trust
boundary; raw localhost sessions are deliberately excluded.

If Mjolnir or the host crashes, containers it was managing can be orphaned —
still running, but no longer tracked in Mjolnir's state. Use `mj recover` to
find and reclaim them:

```console
mj recover scan --json
mj recover adopt --session <session-id> --target <target-id>
mj recover destroy --session <session-id> --target <target-id> --confirm <session-id>
```

`scan` lists managed containers that exist but aren't in Mjolnir's state. `adopt`
reconnects one back into Mjolnir as a tracked session; add `--profile` and
`--bundle` when the orphan predates Mjolnir's ownership markers and can't be
matched to a profile and bundle automatically. `destroy` removes one without
adopting it first; `--confirm` must repeat the session ID exactly, as a
safeguard against destroying the wrong container.
