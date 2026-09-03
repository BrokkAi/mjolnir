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
`bypassPermissions`, Kimi Code `auto`, Grok Build's `--always-approve` launch
flag, or DeepSeek Harness's `danger-full-access` permission mode. Every one of
those approves every call. Note that Kimi Code's mode is named `auto` but is
not a guardian policy that approves only low-risk calls.

Raw localhost worktrees preserve the profile and harness's configured approval
behavior instead. Codex, Claude Code, and Grok Build expose guardian modes
through their harnesses; Kimi Code and DeepSeek Harness do not. Mjolnir warns
against running either unsupported harness on a raw, unsandboxed target.

Closing a session first writes and verifies a recovery archive, then removes
that exact container. No mutable session workspace persists past the session
except what the recovery archive captured and whatever you pushed to a
remote. Mjolnir may retain read-only Git objects in the host clone cache described
below.

## Prerequisites

Install each runtime you want to use as a target:

- **Rootless Podman 4.0 or newer** on Linux or WSL2. See
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

mj ships a reference container image with everything a session needs
pre-installed: Rust, cargo-nextest, Node 24, OpenJDK 25, Git, GitHub CLI, the
Codex and Claude ACP bridges, and pinned DeepSeek Harness plus
`dsh-acp-server`. It also carries Playwright's Chromium system libraries and
the pre-installed Chromium headless shell in
`PLAYWRIGHT_BROWSERS_PATH=/ms-playwright`, so headless browser tests need no
privileged install and no run-time browser download, and the profiling tools
`perf`, `cargo-flamegraph`, `samply`, and `heaptrack`; `perf` additionally needs
the host's `kernel.perf_event_paranoid` set to 1 or lower, or the container run
with `--cap-add SYS_ADMIN`. Optional local coverage tooling includes the
`llvm-tools-preview` component, pinned `cargo-llvm-cov`, and `lcov` for
`genhtml`. It's published at
`ghcr.io/brokkai/mjolnir/agent-dev:latest`, public and
multi-arch for both `linux/amd64` and `linux/arm64`, so the same image name
works whether Mjolnir is running it through Podman, Docker, Apple's `container`
runtime, or an arm64 SSH host.

You don't need to do anything to get it: `mj setup`'s image prompt already
defaults to this published image, and Podman, Docker, and Apple's `container`
runtime pull an image automatically the first time it's needed. Accepting the
default when you run `mj setup`, below, is enough.

Building it yourself remains a supported alternative, for example to
customize the image or to work offline:

```console
podman build --pull=always \
  --file containers/Containerfile.agent-dev \
  --tag localhost/mjolnir/agent-dev:latest \
  containers
```

## Run `mj setup`

```console
mj setup
```

Setup reports the Codex, Claude Code, Kimi Code, Grok Build, and DeepSeek
Harness homes it found, the
GitHub origin of the current directory, and which local container runtimes
are usable. If one or more are usable, it prompts you for:

1. The container image, defaulting to `ghcr.io/brokkai/mjolnir/agent-dev:latest`
   — press Enter to accept it, or enter `localhost/mjolnir/agent-dev:latest` here
   if you built the image yourself above.

It creates one ordinary target for every usable runtime (for example, `podman`
and `docker`). Those targets appear independently in Mjolnir's normal target
picker; setup does not choose one on your behalf.

A plain image such as `ubuntu:24.04` still works if you enter it here: Mjolnir
auto-installs Git, GitHub CLI, and Node the first time a session needs them.
But that installation runs inside every new container, which slows down the
start of each session. The default agent-dev image avoids that cost.

Container targets default to `pull_policy = "auto"`. A launch never waits on a
registry under that default: it starts from the image the host already has, and
pulls only when the host has no copy at all. The Mjolnir daemon keeps remote
`:latest` images current on its own, an hour at a time, and removes the dangling
images each pull leaves behind. Versioned tags remain cached, digest references
stay pinned, and `localhost/...` images remain local.

Set `pull_policy` beside `image` to `always`, `newer`, `missing`, or `never`
when a target needs an explicit policy. An explicit `always` or `newer` still
pulls during the launch itself, and the daemon refreshes it in the background
too. Existing running containers are never replaced in place.

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

This opens Mjolnir's terminal surface. Press **Tab** to focus the Sessions pane,
then **n** to start the new-session wizard.
It walks you through picking a profile, a target, and a bundle.

Before launch, you can size the container's CPU and memory allocation. The
baseline is 8 CPUs and 32 GiB:

| Key | Effect |
| --- | --- |
| `+` | Doubles the current allocation |
| `-` | Halves the current allocation |
| `c` | Doubles CPU only |
| `m` | Doubles memory only |
| `r` | Resets to the 8-CPU/32-GiB baseline |

The wizard ends on a review screen where you can add, edit, or remove
attached directories before launch. On container targets, each attached
directory is mounted using the runtime's isolated mount mode, so a container
can't write back into your host filesystem through it.

Each attached directory also has a read-only checkbox. Podman and Docker use
copy-on-write OverlayFS mounts, which some filesystems cannot host: when Mjolnir finds
a source on NFS, SMB, FUSE, a FAT-family filesystem, or another overlay, it
attaches that directory read-only instead and says so while the session
launches.

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
