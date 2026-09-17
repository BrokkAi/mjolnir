---
title: Container targets
description: Set up a disposable container target for Mjolnir and start your first isolated session.
---

## What container targets give you

Each session on a container target runs in its own disposable, labeled
container: local Podman or Docker on Linux or WSL2, Apple's `container`
runtime on macOS 26 or newer on Apple silicon, or Podman over SSH. Container
isolation always
selects Mjolnir's `unconstrained` execution policy. The `permissions` setting is
only available for raw `ssh-bare` targets. Mjolnir translates the policy into the
selected harness's own control: Codex `agent-full-access`, Claude Code
`bypassPermissions`, Kimi Code `auto`, or Grok Build's `--always-approve`
launch flag. Every one of those approves every call. Note that Kimi Code's
mode is named `auto` but is not a guardian policy that approves only low-risk
calls.

Raw localhost worktrees preserve the profile and harness's configured approval
behavior instead. Codex, Claude Code, and Grok Build expose guardian modes
through their harnesses; Kimi Code does not. Mjolnir warns against running an
unsupported harness on a raw, unsandboxed target.

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

Closing a session first writes and verifies a recovery archive, then removes
that exact container. No mutable session workspace persists past the session
except what the recovery archive captured and whatever you pushed to a
remote. Mjolnir may retain read-only Git objects in the host clone cache described
below.

## Prerequisites

Install each runtime you want to use as a target:

- **Rootless Podman 4.3 or newer** on Linux or WSL2. See
  [Podman for Mjolnir](/podman/) for installation and verification steps.
- **Docker with a reachable Linux daemon** on Linux or WSL2. See
  [Docker for Mjolnir](/docker/) for its OverlayFS and lifecycle contract.
- **Apple's `container` CLI** on macOS 26 or newer on Apple silicon.

Linux releases are static musl binaries, so the controller itself runs the
session relay in same-architecture Linux containers. The installer also places
the other supported Linux architecture's
`mj-worker-<arch>-unknown-linux-musl` companion next to `mj`. On macOS it
installs both Linux companions.

## Get the agent-dev image

Mjolnir ships a reference container image with everything a session needs
pre-installed: Rust, cargo-nextest, Node 24, OpenJDK 25, Git, GitHub CLI, and
the Codex and Claude ACP bridges. It also carries Playwright's Chromium system
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
stopped service, then press **F5** to recheck. No `mj setup` command is required.

Use **F7 Settings → Machines and Runtimes** to override the container image,
resource defaults, or environment, or to add an SSH or EC2 connection. The
optional CLI setup command remains available.

A plain image such as `ubuntu:24.04` still works if you enter it here: Mjolnir
auto-installs Git, GitHub CLI, and Node the first time a session needs them.
But that installation runs inside every new container, which slows down the
start of each session. The default agent-dev image avoids that cost.

Container targets default to `pull_policy = "auto"`. Podman and Docker launches
do not wait on a registry under that default: they start from the image the host
already has and pull only when the host has no copy at all. The Mjolnir daemon
refreshes eligible remote `:latest` images for local or SSH Podman and local
Docker once an hour, and removes the dangling images each pull leaves behind.
Versioned tags remain cached, digest references stay pinned, and
`localhost/...` images remain local. Apple container is not part of that
background loop; it resolves `auto` and refreshes the image while provisioning
a session.

Set `pull_policy` beside `image` to `always`, `newer`, `missing`, or `never`
when a target needs an explicit policy. On Podman and Docker, `always` or
`newer` pulls during launch and remains eligible for the background refresh.
Apple evaluates the policy only during provisioning; `always` and `newer`
request a pull there. Existing running containers are never replaced in place.

## Git clone cache

Local Podman, local Docker, SSH Podman, and Apple container targets cache GitHub repository
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
end-to-end test: it creates and removes a disposable container the same way
`mj setup` does, confirming the full path works, not just static
prerequisites. For Docker, this also verifies that a temporary writable
attachment is copy-on-write and that its managed OverlayFS volume cleans up.

```console
mj doctor --json --smoke
```

## First session

```console
mj
```

This opens Mjolnir's terminal surface. Press **Create**, **Alt-N**, or
**Alt-W** from anywhere for the full new-session wizard.
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

On rootless Podman, every session container runs in a user namespace that
maps the image's user onto your host user, so an `rw` attachment is writable
and the files the container creates in it are owned by you. Mjolnir reads the
image's user and group ids once per image before it creates the container. If
that read fails, the container runs with Podman's default user mapping, the
way it did before this mapping existed, and the session log says so.

## Build cache (mbx)

Rust sessions on Podman and Docker container targets share one
[mbx](https://github.com/jdx/mr-boxington) build cache per container host, with
no setup. mbx wraps Cargo: it looks each compiler action up in a
content-addressed store and restores the cached output instead of recompiling.
The second session to build a project on a host reuses the first one's work.

Mjolnir turns this on for a session when all of the following hold:

- The target is Podman or Docker, local or over SSH. Apple `container`, bare
  targets, and EC2 targets never get a build cache.
- The primary repository has a `Cargo.toml` at its root. A Cargo workspace in
  a subdirectory is not detected.
- The resolved cache directory is on a filesystem that supports reflinks, which
  is what makes restoring a cached output nearly free.
- The container host either has no mbx of its own, or has one at least as new
  as the version Mjolnir installs into containers. An older native mbx must not
  write the same store, so Mjolnir runs those sessions without the cache.

The cache directory lives on the container host and is mounted read-write into
the container at the same absolute path. Nothing is synchronized between hosts,
and Mjolnir never runs mbx garbage collection: the host's own mbx and the
automatic collection inside containers are the only collectors.

### Settings

There is a global switch, **Build cache (mbx)**, that turns the feature off
everywhere. Each container target can override three values:

| Setting | Default when blank |
| --- | --- |
| Enabled | On when the cache filesystem supports reflinks. |
| Cache directory | The host's native mbx cache if mbx is installed there, otherwise `~/.cache/mbx` on that host. |
| Cache size limit | The host's own mbx limits if it has a configuration file, otherwise the smaller of 100 GB and a quarter of the free space. |

Opening a target's build cache page asks its host for these values, so each
blank field shows what a session there would actually use, such as
`/mnt/fast/mbx-cache`. When sessions on that host run without
the cache, the page says why, for example because the host has no
reflink-capable filesystem or its own mbx is too old.

When the host has `~/.config/mbx/config.toml`, Mjolnir copies it into the
container so the container's mbx uses the host's own budgets. If that file
relocates `[target] root` outside the cache directory, that directory is
mounted read-write at its own path too.

Inside the session, `cargo` is the mbx shim, which runs the image's real Cargo
underneath. `mbx stats` reports what the cache holds.

`MJ_MBX_BINARY` overrides the pinned download with a local mbx binary, for
development against an unreleased mbx.

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
