# Run the daemon in a container, chosen in Settings

Superseded by `.agents/plans/portable-daemon-container.md`: this design only works on Linux, because it shares the host's network and PID namespaces and bind-mounts host paths. Keep this file as history only.

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`.


## Purpose / Big Picture

The daemon is the background process `mj daemon-run`. It owns the session database, serves the API and web viewer, and starts the per-session worker processes that run agents. Before this change it always ran as a plain process on the user's computer. After it, a user opens Settings (terminal or web), goes to Setup → Daemon, and picks where it runs: Host (the old behaviour, still the default), Podman, or Docker. Saving moves the daemon there without stopping running sessions. Anyone who prefers files can write the same choice as `[daemon] runtime = "podman"` in `config.toml`, but nothing requires it.

To see it working, in an isolated instance (never the default one):

    printf 'version = 20\n[daemon]\nruntime = "docker"\n' > ~/.config/mjolnir/instances/t/config.toml
    mj --instance t sessions
    mj --instance t daemon status          # "This daemon runs this build."
    cat /proc/$(jq .pid ~/.local/share/mjolnir/instances/t/daemon.json)/cgroup
                                           # a docker-…scope / libpod-…scope

Setting `runtime = "host"` again and running any `mj --instance t` command prints "Settings run the Mjolnir daemon on this computer; moving it out of its container." and the daemon's cgroup is a user-session scope again.


## How it works

Every client finds the daemon through `daemon.json` in the data directory (`mj-client/src/daemon.rs`): a loopback TCP address, a token, and a PID that clients probe with `kill(2)` and `/proc`. The design keeps all three valid when the daemon is in a container. The container uses `--network=host` (the loopback address works), `--pid=host` (the PID in `daemon.json` is a host PID), and runs as the user's own uid (`--userns=keep-id` for rootless Podman, `--user uid:gid` plus `/etc/passwd` and `/etc/group` for Docker), so signals and `/proc` reads succeed. It mounts `$HOME`, the data and configuration directories, `/tmp`, `$XDG_RUNTIME_DIR` and any `[daemon] mounts` at their host paths, so every path the daemon writes into the store or passes to a worker or a session container means the same thing on the host. SELinux relabeling is disabled for the container (`--security-opt=label=disable`) because relabeling a home directory would break the host's use of it.

The daemon in the container is the image's own `mj` at `/opt/mjolnir/mj`. `containers/Containerfile.agent-dev` builds it in the `worker` builder stage, from the same commit and against the same Debian base as the final image, and copies it with the worker in one layer; the worker is also linked as `mj-worker-<musl triple>` so the daemon finds it for local and container sessions. The image is labelled `org.brokk.mjolnir.revision` with `MJ_BUILD_REVISION`, the same argument the binary's build identity comes from. The launcher (`resolve_image`) uses the configured image, or with none this build's default image or any local image carrying this client's revision, and only when the label equals this client's revision. Otherwise the daemon runs on the host with a notice that names `scripts/build-agent-dev-image.sh`. Because the image's file times differ from the client's, a containerized daemon is the same build when version and revision match (`BuildIdentity::same_source`); `daemon_runs_this_build` in `mj-cli/src/daemon.rs` is the one answer to "does the daemon run this build" for status, restart and the development refresh.

The container's main process is `sleep infinity`; it only holds processes. `mj-cli/src/daemon/container.rs` creates it on demand and starts the daemon in it with `exec bash -c '… setsid bash -c "exec -a mj \"$0\" daemon-run" …& echo $!'`, which prints the daemon's host PID. Everything after the launch (readiness wait, handoff, stop, restart) is the existing host code, unchanged, because the daemon looks identical to a host one.

Local ("bare") sessions' workers are started by the daemon, so they run in the container and use the image's tools. They outlive a daemon handoff just as host workers do, because the container keeps running. A container is never stopped while anything but `sleep` runs in it. The container name is `mj-daemon-<instance digest>-<spec digest>`, where the spec covers engine, image, mounts, environment and run flags. A changed spec gets a new container beside the old one. The new daemon reattaches to the old container's workers through their Unix sockets and host PIDs, and the old container is removed once `top` shows only its `sleep` (`remove_idle_predecessors`). A failed cleanup is logged and never blocks a start.

The decision "is the daemon where Settings say" has one owner, `decide_existing_daemon` in `mj-cli/src/daemon.rs`. The daemon launched in a container carries `MJ_DAEMON_CONTAINER=<name>` in its environment. A client of the same build reads that from `/proc/<pid>/environ`, compares it with the name the configuration yields (or with none for host), and on a mismatch returns `ExistingDaemon::Replace`, which is the ordinary graceful handoff used for upgrades. The dashboard calls `connect_or_start` from `spawn_daemon_placement` (`mj-cli/src/dashboard/actions.rs`) when the runtime feed delivers a changed `[daemon]` section (`drain_runtime_config` in `mj-cli/src/dashboard/drains.rs`), so a save from any surface reaches every attached terminal and is applied immediately.

Settings rows come from the shared schema `mj-client/src/settings/schema.rs` and `page.rs`, which drives both the terminal and the web viewer: `daemon` defaults, label, summary, choices `host|podman|docker`, help, and `mounts` entries as local paths.

Container sessions launched by a containerized daemon need the engine clients inside the image. `containers/Containerfile.agent-dev` installs `docker-cli` and `podman-remote` (linked as `podman`). The daemon reaches Docker through the mounted `/var/run/docker.sock` (with `--group-add` of its group) and Podman through `$XDG_RUNTIME_DIR/podman/podman.sock` via `CONTAINER_HOST`, starting `podman.socket` with `systemctl --user` when it is absent.

Configuration version 20 adds `[daemon]` and is breaking: older readers refuse the table, so they cannot start a host daemon while the file asks for a container.


## Progress

- [x] (2026-11-03) `[daemon]` config (`mj-core/src/config/daemon.rs`), version 20.
- [x] (2026-11-03) Container launcher and placement decision in `mj-cli`.
- [x] (2026-11-03) Settings UI rows; dashboard applies saved changes.
- [x] (2026-11-03) Image installs `docker-cli` and `podman-remote`.
- [x] (2026-11-03) Live validation in isolated instances with Docker and rootless Podman.
- [x] (2026-11-03) The image builds and carries its own `mj`; the launcher matches images by revision label instead of mounting the host executable. Validated with a Fedora-native development client.


## Surprises & Discoveries

The first version mounted the host's `mj` executable into the container. A binary linked against the host's glibc does not run on an image with an older one (Fedora 44's glibc 2.43 against trixie's 2.41), so a development build could not start there; the image now builds and carries its own `mj`. `docker top` passes its arguments to ps(1) (`-o pid`) while `podman top` takes field names (`pid`); the first cleanup used Podman's form and failed under Docker. 


## Decision Log

The setting mirrors `[quota] runtime` but has no `auto`, because detecting an engine would silently move existing daemons into containers. Host namespaces (network and PID) are shared instead of translating addresses and PIDs, because every client, the handoff, and the stop logic already speak host loopback and host PIDs; nothing past the launch had to change. Specs are versioned by name instead of recreating a container in place, because a container may still hold workers whose turns must not be cancelled.


## Outcomes & Retrospective

Validated with a real Claude turn in each case: a session answered in a Docker daemon container, its worker survived `mj daemon restart`, the daemon moved to the host and back while that worker kept answering, an image change created a new container while the old one kept the worker, and destroying the session let the next start remove the old container. Rootless Podman ran the daemon as the user's uid and answered a turn. Containerized daemons do not run on macOS or Windows; there the launch notes this and starts the daemon on the host.
