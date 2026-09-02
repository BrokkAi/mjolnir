# Podman for Mjolnir

This is the operational contract for a host that runs Mjolnir `local-podman`
targets. For a coding-agent handoff, run `mj setup instructions --platform
linux` and `mj doctor --json`, then give the instructions page plus the output
of `mj doctor --json` to the coding agent. The host is ready only when every
postcondition in [Verification](#verification) passes for the same unprivileged
user that will run `mj`.

Mjolnir supports Podman **4.0.0 or newer**. Version 4 is the minimum because Mjolnir's
local target relies on the mature rootless user-namespace behavior and CLI
interfaces that Mjolnir probes (`podman info` and `podman unshare`). Podman 3.x is
not a supported Mjolnir runtime.

## How Mjolnir uses Podman

For a target such as:

```toml
[targets.podman]
kind = "local-podman"
image = "localhost/mjolnir/agent-dev:latest"
```

Mjolnir invokes the local `podman` CLI as the user running Mjolnir; it does not use
`sudo`, a shared Podman socket, or a remote Podman connection. Before each
`local-podman` session and while `mj setup` evaluates Podman, Mjolnir performs
these fast runtime checks:

```console
podman --version
podman info --format '{{.Host.Security.Rootless}}'
podman unshare cat /proc/self/uid_map
```

For a session, Mjolnir starts a detached, labeled container from the configured
image, uses `podman exec` for the worker, Git, harness, and clone commands, and
removes that exact container with `podman rm --force` only after checkpointing.
The default `pull_policy = "auto"` starts a session from the image the host
already has, and pulls only when the host has no copy at all. Instead of pulling
during a launch, the daemon runs `podman pull` for every remote `:latest` image
once an hour and then `podman image prune -f`, so a session never waits on a
multi-gigabyte download and dangling layers do not pile up. Versioned and local
tags stay cached, and a digest-pinned image is never replaced. Set `pull_policy`
to `always`, `newer`, `missing`, or `never` to override that inference; `always`
and `newer` pull during the launch as well. Podman's `newer` policy retains a
cached image when its registry is temporarily unavailable.
`mj setup` additionally creates, executes `true` in, and removes a disposable
container from the configured image. The fast runtime probes themselves never
pull an image; a pull happens in the daemon's background refresh, or during
provisioning when the host has no copy of the image or the target sets an
explicit `always` or `newer` policy.

### Git clone cache

For GitHub-backed bundles, Mjolnir keeps bare object mirrors under
`~/.cache/mjolnir/git/mirrors` on the Podman host. A launch refreshes the mirror,
creates a session-specific local snapshot under `~/.cache/mjolnir/git/sessions`,
and mounts only that snapshot read-only into the container. The normal clone
then uses it as a Git object reference, avoiding repeated object downloads
while preserving the image's checkout behavior. Git object files are
immutable, so local snapshots can share them with hardlinks; filesystem
reflink or ZFS permissions are not required.

The cache is opportunistic. Missing host Git, authentication trouble, or a
cache preparation failure produces a launch notice and falls back to the
ordinary in-container clone. A first cache miss still downloads the complete
mirror; later launches fetch only updates. Mjolnir removes session snapshots after
their owning container and prunes mirrors unused for 30 days, then applies a
20 GiB least-recently-used soft cap. Cache directories are private to the
Podman user because they may retain objects from private repositories. You can
remove `~/.cache/mjolnir/git/mirrors` while no launch is updating it; do not remove
the `sessions` directory while managed containers are running.

`mj doctor --json` runs those three checks only when a `local-podman` target
exists, and then checks `podman image exists` for each configured
`local-podman` image. `mj doctor --json --smoke` replaces that presence check
with the full disposable run/exec/remove test, so it automates
[Verification](#verification) sections 3 and 4 for every configured image.

An `ssh-podman` target gets the same probes and the same smoke test, each
wrapped in a noninteractive `ssh` call to the configured host. Every
remediation below then applies on that remote host, as the user that SSH logs
in as.

Mjolnir's bundled agent-development image is published at
`ghcr.io/brokkai/mjolnir/agent-dev:latest` (multi-arch: `linux/amd64` and
`linux/arm64`, public, no authentication needed to pull). Pull it directly:

```console
podman pull ghcr.io/brokkai/mjolnir/agent-dev:latest
```

Building it locally remains a supported alternative, for example to customize
the image or to work offline:

```console
podman build --pull=always \
  --file containers/Containerfile.agent-dev \
  --tag localhost/mjolnir/agent-dev:latest \
  containers
```

## Install Podman rootlessly

Run all `podman` commands below as the normal Mjolnir user. Do not prefix them with
`sudo`.

### Debian and Ubuntu

On Debian 12 or Ubuntu 22.04 and newer, install Podman and the rootless helper
binaries with:

```console
sudo apt update
sudo apt install -y podman uidmap slirp4netns ca-certificates
```

Older distribution releases can package Podman 3.x. The verification command
below is authoritative: if it reports less than 4.0.0, upgrade to a currently
supported Debian/Ubuntu release or install a distribution-supported Podman 4+
package before using Mjolnir.

### Fedora

Install Podman and the packages that provide rootless mapping and networking:

```console
sudo dnf install -y podman shadow-utils slirp4netns ca-certificates
```

### Assign subordinate UID and GID ranges

Rootless containers need a subordinate range for the Mjolnir user in **both**
`/etc/subuid` and `/etc/subgid`. First inspect the current entries:

```console
grep -E "^$(id -un):" /etc/subuid
grep -E "^$(id -un):" /etc/subgid
```

If either command has no output, an administrator must assign an unused range.
For a new machine where `100000-165535` is not already allocated, this command
creates a 65,536-ID range for the current user:

```console
sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 "$USER"
```

Do not reuse that example range when it belongs to another account; choose a
different non-overlapping range according to the host's account policy. End the
login session completely and sign in again after changing either file. Then run:

```console
podman system migrate
```

## Verification

Every command here is a postcondition. Resolve a failure before running Mjolnir.
Run them by hand to diagnose a host; `mj doctor --json --smoke` checks
sections 1 through 4 for every configured target and reports the same failures
with an exact remediation.

### 1. Podman is installed and supported

```console
podman --version
```

Expected: the reported version starts with `4.` or a higher major version. For
example, `podman version 5.4.2` passes. A `3.x` result fails; upgrade Podman as
described above.

### 2. Rootless mode and subordinate mappings work

```console
podman info --format '{{.Host.Security.Rootless}}'
grep -E "^$(id -un):" /etc/subuid
grep -E "^$(id -un):" /etc/subgid
podman unshare cat /proc/self/uid_map
podman unshare cat /proc/self/gid_map
```

Expected:

- The first command prints exactly `true`.
- Both `grep` commands print an entry for the current user.
- Both map commands succeed. Their output must map container ID `0` and at
  least one additional ID (normally a first line mapping ID `0` followed by a
  line beginning at ID `1` with a large range). For example:

  ```text
           0       1000          1
           1     100000      65536
  ```

Mjolnir runs the UID-map command itself before every local-Podman session. It also
checks that container IDs `0` and `1` are mapped, which catches a login with no
usable subordinate range.

### 3. The configured runtime image is available

For Mjolnir's published image, set `IMAGE` to the exact `image` value from the
Mjolnir target and pull it:

```console
IMAGE=ghcr.io/brokkai/mjolnir/agent-dev:latest
podman pull "$IMAGE"
podman image exists "$IMAGE"
```

Both commands must exit zero. Replace the example with the configured image
if it differs. For Mjolnir's locally built `localhost/mjolnir/agent-dev:latest`, build
it with the command above and verify it without attempting a registry pull:

```console
podman image exists localhost/mjolnir/agent-dev:latest
```

### 4. A container can run, execute a command, and be removed

Use the same image as the configured target. The Mjolnir development image supports
the following verbatim:

```console
IMAGE=localhost/mjolnir/agent-dev:latest
CHECK_NAME="mj-podman-check-$$"
podman run --init --detach --name "$CHECK_NAME" "$IMAGE" sleep infinity
podman exec "$CHECK_NAME" /bin/sh -c 'printf "Mjolnir Podman exec works\n"'
podman rm --force "$CHECK_NAME"
```

Expected: each command exits zero, the `exec` command prints `Mjolnir Podman exec
works`, and `podman container exists "$CHECK_NAME"` exits nonzero after the
remove command.

### 5. Containers can reach GitHub over HTTPS

This independent check uses a small image with curl, so it does not assume curl
is present in every custom runtime image:

```console
podman run --rm docker.io/curlimages/curl:8.10.1 \
  -fsSIL --max-time 15 https://github.com/ -o /dev/null
```

Expected: exit status zero. This confirms DNS, rootless container networking,
CA certificates, and outbound HTTPS access needed to clone GitHub bundles.

## Common failures and exact remediations

### `newuidmap` or `newgidmap` is missing

Check the helpers:

```console
command -v newuidmap
command -v newgidmap
```

If either command fails, install the package that provides them, then start a
fresh login session and rerun verification:

```console
sudo apt install -y uidmap
# or, on Fedora:
sudo dnf install -y shadow-utils
```

### No `/etc/subuid` or `/etc/subgid` entry after the user was created

Create non-overlapping ranges for the Mjolnir user, then log out and log in:

```console
sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 "$USER"
grep -E "^$(id -un):" /etc/subuid
grep -E "^$(id -un):" /etc/subgid
podman system migrate
```

If the example range is already assigned, the administrator must choose an
unused range instead. Do not edit mappings for another account.

### Rootless check prints `false`

Mjolnir must run as an unprivileged user. Start a normal shell, do not invoke Mjolnir
through `sudo`, and remove a remote/rootful Podman override before retrying:

```console
unset CONTAINER_HOST
podman info --format '{{.Host.Security.Rootless}}'
```

If a named Podman connection is selected, switch back to the local rootless
connection with the site's normal `podman system connection default` policy.

### Pulls or GitHub HTTPS fail inside a container

First distinguish host DNS/TLS from container networking with the HTTPS command
above. Install `ca-certificates` and `slirp4netns` using the distro commands if
they are absent. Corporate proxies must be configured for the rootless Podman
environment and passed to containers according to the organization's policy;
never put proxy credentials in Mjolnir's committed configuration.

### WSL2

Use a current **WSL2** distribution, not WSL1. On Windows, verify the version
and update the WSL kernel when `podman unshare` reports `operation not
permitted` or user namespaces are unavailable:

```powershell
wsl -l -v
wsl --update
wsl --shutdown
```

Then reopen the Linux distribution, install Podman and `uidmap` inside that
distribution, configure `/etc/subuid` and `/etc/subgid` there, and rerun every
verification command. Mjolnir uses the Podman CLI directly and does not require a
Podman API socket or a systemd service, but rootless networking still requires
the WSL2 kernel support and `slirp4netns` (or the distro's `pasta` setup).
Keep Mjolnir repositories and Podman storage in the Linux filesystem (for example
under `~/`), not `/mnt/c`, for correct Linux permissions and substantially
better overlay-filesystem performance.
