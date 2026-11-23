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
# image = "ghcr.io/brokkai/mjolnir/daemon:latest" # optional override
# mounts = ["/srv/projects"]  # optional host directories
```

Then run any command:

```console
mj sessions
```

The first start pulls `ghcr.io/brokkai/mjolnir/daemon`, creates the named volume
and the container, and starts `mj daemon-run` inside it. Releases use their
version tag and development builds use `:latest`. Later starts reuse the same
container and volume. `mj daemon status` reports the container name.

This default is separate from the `buildpack-deps:noble` default for session
containers. Changing a session target image does not change the daemon image.

The image carries the daemon and session worker. Sessions targeting
`localhost` run their worker inside the daemon container. Mjolnir prepares
the pinned harnesses and Node.js on demand for sessions, model discovery,
`mj login`, and quota checks. Harness installations and logins persist
in the volume.

The image is built from `containers/Containerfile.daemon`. To build it from
a Git checkout, run `scripts/build-daemon-image.sh podman` (or pass `docker`)
and set `[daemon] image` to `localhost/mjolnir/daemon:dev`.

## Data and restarts

The volume is the durable part. Stopping the daemon, replacing its build, or
starting it again does not lose the store or configuration. Removing the
container while its volume remains is also safe; the next start creates a new
container and mounts the same volume.

`mj daemon stop` asks the daemon to stop. If it does not answer, Mjolnir stops
the container through the engine. A forced stop also stops the workers running
in that container.

## Setup and logins

Start `mj` on a fresh volume and wait for the welcome dialog to finish
discovering agents. First-run setup prepares the harnesses in the daemon
container and configures the volume. Use `mj login` to sign in.

```console
mj login --profile codex
```

When container placement is enabled, `mj login` and the prerequisite checks
from `mj doctor` run inside the daemon container with the home, data, and
configuration directories pointing into the volume.

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
- Or rootless Podman on Linux.
- The container engine must be able to pull from `ghcr.io`.
