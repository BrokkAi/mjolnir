---
title: Apple container targets
description: Install and verify Apple's container runtime for disposable Mjolnir sessions on Apple silicon.
---

An `apple-container` target runs each session in a separate Linux container
through Apple's `container` CLI. It is available only on Apple silicon with
macOS 26 or newer and, like every isolated target, runs the selected harness in
its unrestricted mode.

## Install the runtime

Install Apple's official signed package by following the
[container project installation instructions](https://github.com/apple/container#initial-install).
Mjolnir does not download or install it for you.

Start the service before running setup:

```console
container system start
container system status
```

On an Intel Mac or an older macOS release, use a [remote SSH target](/ssh/) or
an [EC2 target](/aws/) instead.

## Configure a target

`mj setup` detects a usable Apple container installation and writes this target
when you select it:

```toml
[targets.apple-container]
kind = "apple-container"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

The target accepts the common container fields:

| Key | Required | Meaning |
| --- | --- | --- |
| `image` | yes | Linux image used for every new session. |
| `pull_policy` | no | `auto` (default), `always`, `newer`, `missing`, or `never`. |
| `platform` | no | Explicit image platform when runtime selection needs it. |
| `cpus` | no | Backend CPU fallback when an API launch omits a per-session allocation. |
| `memory` | no | Backend memory fallback when an API launch omits a per-session allocation. |
| `environment` | no | Environment variables added inside the container. |

Apple container does not accept Podman's `workspace_storage` override. Each
session gets an isolated workspace managed by the runtime.

The TUI new-session wizard does not use `cpus` and `memory` as its initial
selection. It starts with the allocation remembered for the local container
host, or the 8-CPU/32-GiB baseline when none has been remembered. The selected
per-session allocation takes precedence over these target-level backend
fallbacks.

Apple container also does not participate in the daemon's Podman/Docker image
refresh loop. It evaluates `pull_policy` while provisioning each session, so an
eligible image refresh happens as part of launch.

## Verify it end to end

Run the static checks, then opt into the disposable smoke test:

```console
mj doctor --json
mj doctor --json --smoke
```

The smoke test uses the configured target image, creates a disposable
container, runs `true`, and removes it. If no `apple-container` target exists,
doctor uses a small stock image for the runtime check. The target is ready only
when the smoke result is no longer `fixable`.

At launch Mjolnir checks `container system status`, prepares the image, creates
the labeled container, installs or discovers Git and the selected ACP bridge,
and clones the configured [bundle](/workspaces-bundles/) under `/workspace`.
The same [verified checkpoint and fresh-target resume](/durability/) rules as
other isolated targets apply.

See [Container targets](/containers/) for image refresh, clone cache,
attachments, credentials, and recovery behavior shared by all container
runtimes. See [Custom images](/custom-images/) before replacing the reference
agent image.
