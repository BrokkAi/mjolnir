---
title: Apple container targets
description: Install and verify Apple's container runtime for disposable Mjolnir sessions on Apple silicon.
---

An `apple-container` runtime runs each session in a separate Linux container
through Apple's `container` CLI. It is available only on Apple silicon with
macOS 26 or newer and, like every isolated target, runs the selected harness in
its unrestricted mode.

## Install the runtime

Install Apple's official signed package by following the
[container project installation instructions](https://github.com/apple/container#initial-install).
Mjolnir does not download or install it for you.

Mjolnir starts the service when a session needs it. If it finds a stopped
service, it runs `container system start --enable-kernel-install`, which
installs the default kernel if needed. The Mjolnir daemon also starts the
local service in the background at startup when a local Apple container
target is configured.

On an Intel Mac or an older macOS release, use a [remote SSH target](/ssh/) or
an [EC2 target](/aws/) instead.

## Configure a target

Mjolnir supplies this local target automatically on macOS and checks the runtime
before allowing selection. Customize it in **prefix+s Settings → Runtimes**:

```toml
[targets.apple-container]
kind = "apple-container"
```

The target accepts the common container fields:

| Key | Required | Meaning |
| --- | --- | --- |
| `image` | no | Linux image used for every new session; defaults to `docker.io/library/buildpack-deps:noble`. |
| `pull_policy` | no | `auto` (default), `always`, `newer`, `missing`, or `never`. |
| `platform` | no | Explicit image platform when runtime selection needs it. |
| `cpus` | no | Backend CPU fallback when an API launch omits a per-session allocation. |
| `memory` | no | Backend memory fallback when an API launch omits a per-session allocation. |
| `environment` | no | Environment variables added inside the container. |

Apple container does not accept Podman's `workspace_storage` override. Each
session gets an isolated workspace managed by the runtime.

The TUI new-session wizard does not use `cpus` and `memory` as its initial
selection. It starts with the allocation remembered for the Mac, or the
4-CPU/8-GiB baseline when none has been remembered. Selecting **Save as default
sizing** stores the chosen per-host default. The selected per-session
allocation takes precedence over these target-level backend fallbacks.

Mjolnir budgets Apple containers against the Mac's physical CPU and RAM, not
against each VM independently. It reserves one fifth of physical RAM for macOS
and sums every owning container's configured limits. The UI reports the hard
RAM left: memory compression and reclaimable pages do not count as additional
capacity. A launch that would exceed that RAM budget is refused, including a
resume or a move onto an Apple container target. Apple's VMs do not return
freed memory to macOS. CPU overcommit produces a warning and the launch
proceeds. Docker and Podman on the same Mac allow RAM overcommit with a
warning as well. Mjolnir never stops a session automatically to make room.

Apple container takes part in the daemon's background image download like
Podman and Docker: a configured image the host does not have is downloaded a
few seconds after the daemon starts, and an eligible moving tag is refreshed
hourly, with `container image inspect` and `container image pull`. It also
evaluates `pull_policy` while provisioning each session, so an eligible image
refresh still happens as part of launch.

## Run it on another Mac

The runtime can live on an SSH machine, which lets a Linux controller — or an
Intel Mac — use the Apple-silicon Mac beside it:

```toml
[machines.mac]
kind = "ssh"
host = "mac.example.com"
user = "dev"

[targets.mac-apple]
kind = "apple-container"
machine = "mac"
```

Install the runtime on the remote Mac by following the [installation
instructions](https://github.com/apple/container#initial-install). Mjolnir
starts the service on the remote Mac when a session needs it and runs
`container` with the Mac's system `PATH`, which includes `/usr/local/bin`.
Check the setup before adding the machine:

```console
ssh mac.example.com /usr/local/bin/container --version
ssh mac.example.com /usr/local/bin/container system status --format json
```

When you save that machine, Mjolnir probes it over SSH — `uname -sm`,
`sw_vers -productVersion`, `container --version`, and
`container system status --format json`. Both running and stopped services
are eligible in **Settings → Runtimes**.
`mj doctor` checks it the same way. Every command for the session runs on that
Mac; the controller needs `ssh` but no local container runtime. The Mac and its
container service stay running when sessions stop.

## Verify it end to end

Run the static checks, then opt into the disposable smoke test:

```console
mj doctor --json
mj doctor --json --smoke
```

The smoke test uses the configured target image, creates a disposable
container, runs `true`, and removes it. If no `apple-container` runtime exists,
doctor uses a small stock image. The smoke test starts a stopped service.

At launch Mjolnir checks the service, prepares the image, creates
the labeled container, installs or discovers Git and the selected ACP bridge,
and clones the configured [bundle](/workspaces-bundles/) under `/workspace`.
The same [verified checkpoint and fresh-target resume](/durability/) rules as
other isolated targets apply.

See [Container targets](/containers/) for image refresh, clone cache,
attachments, credentials, and recovery behavior shared by all container
runtimes. See [Custom images](/custom-images/) before replacing the reference
agent image.
