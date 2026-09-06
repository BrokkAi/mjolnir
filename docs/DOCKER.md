# Docker for Mjolnir

This is the operational contract for hosts that run Mjolnir Docker targets.
Mjolnir drives the Docker CLI and requires it to reach a Linux Docker daemon.
For `local-docker`, the daemon shares a filesystem host with the attached
directories and Mjolnir's cache. For `ssh-docker`, the CLI, daemon, overlay
backing directories, and attached source paths are all on the configured SSH
host; the controller does not install Docker locally or use a Docker API
library. The `--smoke` check below is authoritative for environments, such as
desktop virtual machines, where the CLI endpoint and host filesystem may not be
the same machine.

## Configure a target

```toml
[targets.docker]
kind = "local-docker"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

An SSH Docker target uses the existing OpenSSH configuration and runs Docker
filesystem operations on the remote host:

```toml
[targets.builder-docker]
kind = "ssh-docker"
host = "builder"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

The `host` value is an SSH destination (usually an alias from
`~/.ssh/config`). It must support noninteractive `ssh builder true`; configure
keys and host verification before running `mj doctor`. The remote user must be
able to run Docker, and Docker Engine must be installed and running there.
Mjolnir does not forward a Docker socket, create a Docker context, or install
Docker on the controller machine.

Before launch, Mjolnir runs:

```console
docker version --format '{{.Server.Version}} {{.Server.Os}}'
```

The command must succeed and report `linux` as the server operating system.
For each session Mjolnir starts one detached, labeled container, uses `docker exec`
and `docker cp` for the worker and its files, and removes that exact container
only after checkpointing succeeds.

The default `pull_policy = "auto"` launches with `docker run --pull=missing`, so
a session starts from the cached image. The daemon keeps remote `:latest` images
current instead: once an hour it runs `docker pull` and then
`docker image prune -f` for every such image. Versioned tags, local names, and
digest-pinned references stay cached. Docker has no `--pull=newer` spelling, so
Mjolnir maps an explicit `newer` and `always` to `docker run --pull=always`:
Docker checks the registry manifest digest and reuses unchanged layers.
`missing` and `never` map directly to Docker's matching run policies.

## Writable attached directories

Mjolnir preserves the same attached-directory behavior on Docker and Podman. A
writable attachment reads the selected host directory but writes into
session-owned OverlayFS upper and work directories, leaving the original host
directory unchanged. Read-only attachments remain ordinary read-only bind
mounts.

For each writable attachment, Mjolnir creates a labeled Docker local volume in
this form:

```console
docker volume create --driver local \
  --label dev.mj.managed=true \
  --label dev.mj.session=<session-id> \
  --opt type=overlay \
  --opt device=overlay \
  --opt o=lowerdir=<source>,upperdir=<upper>,workdir=<work> \
  <volume-name>
```

Docker's built-in local volume driver passes these options to the Linux mount
operation. The upper and work directories live below
`~/.cache/mjolnir/docker-overlays/<container-name>` on the Docker filesystem
host. For `ssh-docker`, that means the remote user's cache directory, not the
controller's cache. Mjolnir records an ownership marker there, verifies labels
before reusing a volume, and refuses a colliding foreign volume or backing
directory.

On a failed launch, Mjolnir removes only resources carrying the expected session
identity. On normal close it removes the container first, then its labeled
volumes, then the upper/work directory. It retains the backing directory if
the container or a volume could not be removed, preventing deletion beneath a
live mount.

OverlayFS requires a Linux daemon with working overlay mounts. Its upper and
work directories must be on the same compatible filesystem. Mjolnir automatically
switches known-incompatible attachment sources, such as NFS, SMB, FUSE, FAT,
or another OverlayFS, to read-only and reports that change during launch.

## Verify Docker and the image

For a local target, first make sure the CLI can reach the daemon:

```console
docker info
docker pull ghcr.io/brokkai/mjolnir/agent-dev:latest
```

For an SSH target, run the equivalent checks on the configured host:

```console
ssh builder docker info
ssh builder docker pull ghcr.io/brokkai/mjolnir/agent-dev:latest
```

Then run Mjolnir's checks:

```console
mj doctor --json
mj doctor --json --smoke
```

The regular check verifies the daemon and each configured image. The smoke
check also creates its temporary lower directory on the Docker filesystem host,
attaches it through the managed OverlayFS path, writes through the container
view, confirms that the lower directory did not change, and removes the
container, volume, and backing directory. Resolve every `fixable` result before
launching a session.

## Git clone cache and recovery

Docker targets use the same host Git clone cache as local Podman targets. For
`ssh-docker`, this cache is on the SSH host. Mjolnir mounts a session snapshot
read-only, lets the in-container clone borrow its objects, and falls back to a
normal network clone if cache preparation fails. Session snapshots are removed
after their owning container.

If Mjolnir exits while a container survives, `mj recover scan` finds Docker
containers carrying both Mjolnir ownership labels, including on an SSH Docker
host. Adoption verifies those labels, starts a stopped container when safe, and
reconnects its worker. A normal checkpoint/resume instead provisions a fresh
Docker container from the verified recovery archive on the same host.
