---
title: Custom container images
description: What a container image must provide to work as a Mjolnir Podman, Docker, Apple container, or SSH Podman target.
---

mj can run a session in any container image that meets a small contract.
`containers/Containerfile.agent-dev` is the reference image and satisfies all
of it; start there if you're building your own. CI publishes this image as
`ghcr.io/brokkai/mjolnir/agent-dev:latest`, multi-arch for `linux/amd64` and
`linux/arm64`.

## The entrypoint

mj starts a session's container detached, running `sleep infinity` as its
command, and runs every later command — the worker upload, Git, the harness,
and the ACP bridge — with `exec` against that running container. Your image
needs a POSIX shell and a `sleep` binary on `PATH`. It does not need any
particular process supervisor or init system beyond that.

## Git and GitHub CLI

If `git` or `gh` is missing, Mjolnir installs both itself the first time a
session needs them, using whichever of `apt-get`, `dnf`, `yum`, or `apk` it
finds, and configures `gh auth git-credential` as the HTTPS credential helper
for `github.com` and `gist.github.com`. That auto-install needs root or
passwordless `sudo` inside the container, plus one of those package managers.

If your image runs as a non-root user with no `sudo`, or uses a different
package manager, bake `git` and `gh` into the image yourself. Either way,
`gh` is what lets HTTPS Git pushes work using the GitHub token Mjolnir syncs into
the session (see [Container targets](/containers/)).

## ACP bridges

For each harness, Mjolnir first looks for an image-baked bridge binary on
`PATH`: `codex-acp`, `claude-agent-acp`, `kimi`, `grok`, or
`dsh-acp-server`. If it doesn't find one, Codex and Claude Code fall back to
running the bridge with `npx -y`, pinned to Mjolnir's fallback versions. DeepSeek
Harness requires Node 22 or newer and follows its adapter's supported install
model: bake the pinned `@deepseek-ai/dsh` and `dsh-acp-server` packages into the
image. Kimi Code and Grok Build have no npm bridge: Mjolnir runs their official
installer with `curl` instead, which needs `curl` in the image.

Baking the bridges in, the way the reference image does, avoids that
per-session install cost and pins the exact bridge version through the image
instead of through Mjolnir's fallback.

Mjolnir-owned worker and bridge commands use non-login shells and do not source
`/etc/profile` or user dotfiles. Images must therefore expose required tools on
their ordinary process `PATH`; profile-only PATH setup is not part of the
container contract. Agent-requested shell commands remain `bash -lc` because
those commands intentionally use the session user's shell environment.

The DeepSeek bridge is the third-party `dsh-acp-server` package. Mjolnir pins both
it and `@deepseek-ai/dsh`, launches its self-managed ACP profile over stdio,
and stages only `.credentials.yaml`, settings, instructions, skills, and agent
presets from `DSH_HOME`. DeepSeek's adapter currently accepts one workspace
root, so a DeepSeek profile cannot launch a multi-repository bundle or a
session with additional mounted directories.

## Workspace and Mjolnir's own files

Sessions work under `/workspace`. Separately, Mjolnir writes its own session
relay binary and a staged, allowlisted copy of the harness profile
(credentials, config, skills, and similar) under `/var/lib/hel/` inside the
container — for example `/var/lib/hel/workers/<session-id>` and
`/var/lib/hel/profiles/<session-id>`.

mj creates these directories itself with `mkdir -p` and writes into them
with plain file copies; it does not pass any specific user to the runtime's `exec`,
so those commands run as whatever user the image's `USER` (or its absence)
puts them in. A rootless Podman container defaults to root inside when no
`USER` is set, which can write anywhere. If your image sets a non-root
`USER`, as the reference image does with its `hel` user, that user needs
write access to `/workspace` and `/var/lib/hel` — the reference image grants
it by creating both directories and `chown`-ing them to `hel:hel` before
switching to that user.

## Browser tests and profiling tools

Nothing in the container contract requires either, but the reference image bakes
both in because a session's unprivileged user cannot install them later.

Playwright's Chromium needs a set of X11, GTK, and font shared libraries that
most base images omit. The reference image installs them with
`npx playwright@<version> install-deps chromium`, pinned to the same Playwright
version `tests/e2e/web/package.json` uses, and pre-installs the Chromium
headless shell into `PLAYWRIGHT_BROWSERS_PATH=/ms-playwright` so a test run
needs no network. Only the shell is baked in, which is what a `headless: true`
config launches; add `npx playwright install chromium` yourself if you need
headed Chromium. If your image skips all of this, run
`npx playwright install --with-deps chromium` inside the session before browser
tests, which needs root or passwordless `sudo`.

For optional local coverage analysis, the reference image carries the
`llvm-tools-preview` rustup component, pinned `cargo-llvm-cov`, and `lcov` so
`genhtml` can render lcov output.

The reference image also carries `perf` (Debian's `linux-perf`),
`cargo-flamegraph`, `samply`, and `heaptrack`. `perf` depends on the
host kernel as well as the image: it needs `kernel.perf_event_paranoid` set to 1
or lower on the host, or the container run with `--cap-add SYS_ADMIN`.

## Resource metrics

mj samples `/sys/fs/cgroup` (`memory.current`, `memory.max`,
`memory.swap.current`, `memory.swap.max`, `cpu.stat`, `cpu.max`) inside the
container to drive the CPU and memory numbers in the resource pane. This
needs cgroup v2. If those files aren't readable, the sampling command
fails, and Mjolnir silently drops that failed sample instead of raising an
error — the session keeps running normally, but the resource pane won't show
current numbers for it. Nothing about running the container or the session
itself depends on cgroup v2 being present.

## What you can't override

The container-template configuration exposes exactly seven keys: `image`,
`platform`, `cpus`, `memory`, `environment`, `pull_policy`, and
`workspace_storage` (the last two only do something useful on the Podman
target kinds; Docker and Apple targets reject non-default `workspace_storage`).
There's no configuration key
for arbitrary extra container-runtime arguments. Mjolnir derives the
rest of the run command itself — the generated container name and the
`dev.mj.session` / `dev.mj.managed` ownership labels it uses to find and
recover its own containers later — and validates that nothing can override
those before starting a container.

## Example target

```toml
[targets.podman]
kind = "local-podman"
image = "localhost/mjolnir/agent-dev:latest"
# Selects the image platform and the matching mj worker architecture.
platform = "linux/amd64"
cpus = "8"
memory = "32g"

[targets.podman.environment]
# Extra container environment variables, merged in at container start.
RUSTFLAGS = "-D warnings"
```

Use `kind = "local-docker"` under a `[targets.docker]` table for the same image
contract on Docker.
