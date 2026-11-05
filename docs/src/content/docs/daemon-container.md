---
title: Run the daemon in a container
description: Run the Mjolnir daemon as a self-contained Docker or Podman instance with its own data volume.
---

Mjolnir runs its daemon on the host by default. Setting `[daemon] runtime` to
`docker` or `podman` is an opt-in toggle that moves the whole daemon into one
long-lived container.

The container is its own machine. It has a private network and PID namespace,
its own filesystem, and one named data volume mounted at `/var/lib/mjolnir`.
It does not mount your home directory, project checkouts, or the host's data
directories unless you ask for them in `[daemon] mounts`. Everything Mjolnir
owns — the session database, configuration, harness homes, caches,
checkpoints, and SessionWiki index — lives in that volume.

## Enable it

```toml
[daemon]
runtime = "docker"          # or "podman"; "host" is the default
# image = "ghcr.io/brokkai/mjolnir/agent-dev:2.65.0"
# mounts = ["/srv/projects"]  # optional host directories
```

Then run any command:

```console
mj sessions
```

The first start pulls the agent-development image, creates the named volume
and the container, and starts `mj daemon-run` inside it. Later starts reuse the
same container and volume. `mj daemon status` reports the container name.

The image carries the daemon, the session worker, and every harness, so local
sessions run their worker inside the same container. Files and harness logins
created there persist in the volume.

Everything that runs a harness in the container uses the harnesses the image
ships, at the versions this release pins: local sessions, model discovery,
`mj login`, and quota checks. Mjolnir never downloads its own copy of a harness
or of Node.js there, the way it does on a host. If a custom `[daemon] image`
lacks a harness or carries an incompatible version, that harness reports an
error naming the image; remove `[daemon] image` to use the release's own image.

## Data and restarts

The volume is the durable part. Stopping the daemon, replacing its build, or
starting it again does not lose the store or configuration. Removing the
container while its volume remains is also safe; the next start creates a new
container and mounts the same volume.

`mj daemon stop` asks the daemon to stop. If it does not answer, Mjolnir stops
the container through the engine. A forced stop also stops the workers running
in that container.

## Setup and logins

Run setup and harness logins through the normal commands:

```console
mj setup
mj login --profile codex
```

When container placement is enabled, these run inside the daemon container
with `HOME`, the data directory, and the configuration directory pointing into
the volume. They do not write the host's configuration. The dashboard's
automatic first-run setup also runs in the daemon, so a fresh container volume
is configured in place.

## Host directories

The container has no host files by default. To let a session edit a directory
on the host, name it explicitly:

```toml
[daemon]
runtime = "docker"
mounts = ["/srv/projects"]
```

The directory appears at the same absolute path inside the container. It is a
normal bind mount: changes the agent makes are host changes. Nothing else from
the host is visible.

## Switching back to the host

`host` and `docker`/`podman` are different placements with different data
directories. Switching back is not required for normal use. If you do, treat
the volume as the source of truth and copy or export what you need before
starting a host daemon; the two stores are not automatically merged.

## Requirements

- Docker with a reachable Linux daemon, including Docker Desktop on macOS or
  Windows.
- Or rootless Podman on Linux or `podman machine` on macOS.
- The container engine must be able to pull from `ghcr.io`.
