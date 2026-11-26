# Podman for Mjolnir

This is the operational contract for a host that runs Mjolnir Podman
targets. For a coding-agent handoff, run `mj doctor instructions --platform
linux` and `mj doctor --json`, then give the instructions page
plus the output of `mj doctor --json` to the coding agent. The host is ready
only when every postcondition in the Verification section below passes: for
the same unprivileged user that will run the container on Linux or WSL2.

Mjolnir supports rootless Podman **4.0.0 or newer**. Podman 4 is the minimum
because Mjolnir relies on the rootless user-namespace behavior and CLI
interfaces that it probes (`podman info`). Podman 3.x is not a supported
Mjolnir runtime.

## Platforms

Mjolnir runs rootless Podman on Linux hosts, including WSL2 and remote Linux
hosts reached over SSH. Use a Podman runtime on an SSH machine to run its
engine directly ([SSH and SSH Podman](/ssh/)). On macOS, use Docker or Apple
container.

New session containers run as uid and gid `0:0` in Podman's default rootless
mapping. Container uid 0 maps to the account running Mjolnir, so files written
to host bind mounts belong to that account. Mjolnir sets `HOME=/home/hel` when
creating these containers, matching the reference image's configured home.
Harness credentials and profiles remain staged under
`/var/lib/hel/profiles/<session>`. It does not remap or chown the image layers.
Claude's root login shell can resolve `HOME=/root` from the root passwd entry.
Provisioning stores inherited Git settings in `/home/hel/.gitconfig`. The
worker's session-global `GIT_CONFIG_GLOBAL` file includes that path absolutely,
so Git sees the settings in both shell contexts regardless of HOME. Mjolnir
stores the include path with the worker's launch settings, so it is applied
again when a worker restarts, resumes, or upgrades.

Stopped containers created by older releases keep their original user
namespace, configured image user, and HOME. Mjolnir leaves `--user` off normal
`podman exec` commands so Podman uses that saved container user when resuming
them. Their inherited Git settings remain in the configured user's global Git
config. Their worker config includes `/home/hel/.gitconfig` by absolute path,
so Claude's root login shell can still read those settings. Only new containers
use the default-map root identity.

## How Mjolnir uses Podman

For a target such as:

```toml
[targets.podman]
kind = "podman"
image = "ghcr.io/brokkai/mjolnir/session-base:latest"
```

Mjolnir invokes the `podman` CLI as the user running Mjolnir; it does not use
`sudo` or a shared Podman socket. Before each Podman session and while
`mj doctor` evaluates Podman, Mjolnir performs these fast runtime checks:

```console
podman --version
podman info --format json
```

The second check reads `host.security.rootless` and `host.idMappings.uidmap`.
Mjolnir rejects an engine that reports `serviceIsRemote` because the target
would record containers on another host. SSH preflight checks for Linux
before it runs the Podman commands.

For a session, Mjolnir starts a detached, labeled container from the configured
image, uses `podman exec` for the worker, Git, harness, and clone commands, and
mounts a dedicated named volume at `/workspace/<session id>` and a separate
private disk-backed temporary volume at `/tmp`. The parent `/workspace`
directory remains on the container's writable overlay; legacy sessions whose
recorded workspace is `/workspace` keep that mount location. `/tmp` has mode
1777 and its contents are excluded from checkpoints. Each session gets a different
volume, so concurrent builds never share a Cargo target directory. On stop,
Mjolnir first takes and verifies the checkpoint, stops the exact container, and
marks the session stopped. It then removes the container, workspace and temporary volumes, and
Git-cache snapshot in a supervised background operation. A slow storage removal
therefore does not keep the session logically running, and its current cleanup
stage remains visible. No extra host service or privileged helper is required
for this default.

Daemon upgrades reconnect to compatible workers and leave existing containers,
workspace storage, temporary files and build caches in place. **Restart agent**
restarts the worker and harness inside the same container, preserving its
filesystem even when the configured image or container settings have changed.
New sessions use the configured image.

Choose **Upgrade container and restart…** from the session menu, or the upgrade
alternative in the Restart dialog, to explicitly replace a container. The
confirmation shows the current and target images. Existing independent
workspace and cache mounts are reattached; they are never reconstructed from a
checkpoint. State in the container's writable layer, including manually
installed packages, may be lost. A checkpoint does not preserve the whole
environment. The private worker journal and harness state are transferred so
accepted work and conversation state survive.

Upgrades requested during work wait for the worker's atomic idle reservation.
The pending request survives daemon replacement and can be cancelled before
replacement starts. The stopped source is retained until the replacement worker
is ready. A failed replacement retains its storage and durable recovery intent.

The operation supports owned local and SSH Podman/Docker containers whose
workspace is on a writable volume or bind mount. A workspace stored only in the
container layer, transient copy-on-write mounts, borrowed child containers and
unsupported engine settings produce a preservation refusal before the worker
is stopped. In particular, a default Docker session with its workspace in the
container layer cannot use this upgrade path. Apple containers are unsupported.
Nitro sandboxes (`kind = "nitro"`) are supported: the sandbox's root disk is
replaced with a clone of the new image in place, its workspace and `/tmp`
volumes are kept, and the old root disk stays on the host until the new worker
is ready. There is no destructive fallback.

Preview and queue through the CLI:

```console
mj upgrade-container --session <id>
mj upgrade-container --session <id> --acknowledge-writable-layer-loss
mj upgrade-container --session <id> --cancel
```

`workspace_storage` can override that policy on a Podman target:

```toml
# Default; this line can be omitted.
workspace_storage = { kind = "podman-volume" }

# Legacy workspace behavior: keep the workspace in the container writable layer.
workspace_storage = { kind = "container-layer" }
```

The container-layer mode exists for compatibility; `/tmp` still gets native
temporary storage. It makes `podman rm` delete
the whole workspace tree and can therefore make cleanup slow after large
builds.

### Optional host-managed workspace storage

Hosts with native fast dataset deletion can opt into a small helper protocol.
For example, the repository's
[`mj-zfs-workspace-helper.sh`](https://github.com/BrokkAi/mjolnir/blob/master/examples/mj-zfs-workspace-helper.sh) creates
one ZFS child dataset per session. Copy it to a root-owned host path, edit its
fixed dataset and mount roots, and grant the Podman user passwordless sudo for
that helper command only. Then configure the target with the helper's mount
root and argv:

```toml
workspace_storage = { kind = "host-helper", root = "/mnt/nvme/mj-workspaces", helper = ["sudo", "-n", "/usr/local/libexec/mj-zfs-workspace-helper"] }
```

Mjolnir invokes `HELPER create RESOURCE`, `HELPER status RESOURCE`, and
`HELPER destroy RESOURCE`. The example validates the deterministic resource
name and confines all operations beneath fixed roots; it is an example rather
than an installed component. The helper-created directory is mounted as the
session's complete workspace at its recorded path, not as a shared Cargo cache.
Temporary storage remains a separate Podman-managed volume and does not require
any changes to the helper.

The default `pull_policy = "auto"` starts a session from the image the host
already has, and pulls only when the host has no copy at all. Instead of pulling
during a launch, the daemon runs `podman pull` for every remote `:latest` image
once an hour and then `podman image prune -f`, keeping cached moving tags fresh
and removing dangling layers. The first launch still waits if the image has not
been downloaded. Versioned and local
tags stay cached, and a digest-pinned image is never replaced. Set `pull_policy`
to `always`, `newer`, `missing`, or `never` to override that inference; `always`
and `newer` pull during the launch as well. Podman's `newer` policy retains a
cached image when its registry is temporarily unavailable.
`mj doctor --smoke` additionally creates, executes `true` in, and removes a
disposable container from the configured image. The fast runtime probes themselves never
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

Concurrent launches share an overlapping mirror refresh. Each launch pins its
own local snapshot before releasing the mirror lock, then builds any
history-limited bundle and sets snapshot permissions independently. A slow
bundle does not block other launches, and cache eviction cannot remove objects
from a snapshot already being prepared.

The cache is opportunistic. Missing host Git, authentication trouble, or a
cache preparation failure produces a launch notice and falls back to the
ordinary in-container clone. A first cache miss still downloads the complete
mirror; later launches fetch only updates. Mjolnir removes session snapshots after
their owning container and prunes mirrors unused for 30 days, then applies a
20 GiB least-recently-used soft cap. Cache directories are private to the
Podman user because they may retain objects from private repositories. You can
remove `~/.cache/mjolnir/git/mirrors` while no launch is updating it; do not remove
the `sessions` directory while managed containers are running.

`mj doctor --json` runs those two checks only when a local Podman runtime
exists, and then checks `podman image exists` for each configured image. `mj doctor --json --smoke` replaces that presence check
with the full disposable run/exec/remove test, so it automates
Verification sections 3 and 4 for every configured image.

A Podman runtime on an SSH machine gets the same probes and the same smoke test, each
wrapped in a noninteractive `ssh` call to the configured host. Every
remediation below then applies on that remote host, as the user that SSH logs
in as. The probes run in `mj doctor` and when you test the
target in the dashboard's Targets pane. Before each session on an SSH machine,
Mjolnir checks only that the host answers over SSH and assumes Podman there is
still as those checks found it, so rerun `mj doctor` after changing Podman on
that host.

Such a runtime also gets a second check, `Host limits for target
<id>`, for the host limits that cause failures when many sessions start
at once. It reports how many kernel keys the SSH login user holds against its
quota in `/proc/key-users`, because every container takes a session keyring
and `podman run` fails with `crun: create keyring` once the quota is gone; fix
that on the host by raising `kernel.keys.maxkeys` and `kernel.keys.maxbytes`
with sysctl. It also reports `fs.inotify.max_user_instances`, which caps the
inotify instances the one login user may hold for every rootless container at
once; below mj's fleet value of 8192 (Ubuntu's default is 128) it warns,
because file watchers then fail with `Too many open files`. Raise both with
sysctl; hosts mj makes get `/etc/sysctl.d/90-mj-limits.conf` from
`mj ec2 create` or `mj ec2 tune`. It also reports the `MaxStartups` value set in
`/etc/ssh/sshd_config` or a readable drop-in, which caps how many
unauthenticated connections sshd accepts at once; raise it there and reload
sshd if provisioning sees dropped connections. Doctor reads the files rather
than running `sshd -T`, which needs root, so it says when a drop-in was
unreadable instead of guessing sshd's default.

Session targets default to `ghcr.io/brokkai/mjolnir/session-base:latest`,
Mjolnir's Ubuntu 24.04-based default session image with pinned Node.js and
agent harnesses preinstalled. Use
`mj image build` to build a per-bundle image from the project's mise recipe.

For daemon-container mode, Mjolnir publishes `ghcr.io/brokkai/mjolnir/daemon`
for `linux/amd64` and `linux/arm64`. Master publishes `:latest` and each
release publishes its own version tag. To build from a checkout, run:

```console
scripts/build-daemon-image.sh podman
```

Set `[daemon] image` to `localhost/mjolnir/daemon:dev` to use the local build.

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

Expected: the reported version is 4.0.0 or newer. For example,
`podman version 5.4.2` and `podman version 4.0.3` pass; `3.x` fails. Upgrade
Podman as described above.

### 2. Rootless mode and subordinate mappings work

On Linux and WSL2:

```console
grep -E "^$(id -un):" /etc/subuid
grep -E "^$(id -un):" /etc/subgid
podman info --format json
```

Expected:

- `podman info` reports `"rootless": true` under `host.security`.
- The `host.idMappings.uidmap` entries map container ID `0` and at least one
  additional ID (normally a first entry for ID `0` followed by an entry
  beginning at ID `1` with a large range). For example:

  ```json
  [
    { "container_id": 0, "host_id": 1000, "size": 1 },
    { "container_id": 1, "host_id": 100000, "size": 65536 }
  ]
  ```

- On Linux and WSL2, both `grep` commands print an entry for the current user.

Mjolnir runs `podman info` itself before every local-Podman session. It checks
the same two things, which catches a login or machine with no usable
subordinate range. If `podman info` reports rootful mode or cannot connect, see
"Podman reports rootful mode or cannot connect" under "Common failures and
exact remediations" below.

### 3. The configured runtime image is available

Set `IMAGE` to the image used by the target. The example uses the default:

```console
IMAGE=ghcr.io/brokkai/mjolnir/session-base:latest
podman pull "$IMAGE"
podman image exists "$IMAGE"
```

Both commands must exit zero. For a locally built image, use
`podman image exists "$IMAGE"` to check it.

### 4. A container can run, execute a command, and be removed

Set `IMAGE` to the configured target's image. The commands below work as
written with the default session image:

```console
IMAGE=ghcr.io/brokkai/mjolnir/session-base:latest
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

### Podman reports rootful mode or cannot connect

`podman info` reports `"rootless": false` when Podman runs as root.
Mjolnir requires a rootless engine.

On Linux and WSL2, run Mjolnir as the ordinary user without `sudo`. If a remote
connection is selected, unset it and check the local engine:

```console
unset CONTAINER_HOST CONTAINER_CONNECTION
podman info --format json
```

Mjolnir rejects a `serviceIsRemote` engine on Linux because the local target
would record containers on another host. Use a Podman `ssh` target for a
different machine instead.

### Pulls or GitHub HTTPS fail inside a container

First distinguish host DNS/TLS from container networking with the HTTPS command
above. Install `ca-certificates` and `slirp4netns` using the distro commands if
they are absent. Corporate proxies must be configured for the rootless Podman
environment and passed to containers according to the organization's policy;
never put proxy credentials in Mjolnir's committed configuration.

### WSL2

Use a current **WSL2** distribution, not WSL1. On Windows, verify the version
and update the WSL kernel when Podman reports `operation not permitted` or user
namespaces are unavailable:

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
