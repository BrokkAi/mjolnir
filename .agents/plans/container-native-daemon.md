# Run the daemon as a container-native instance

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`. It supersedes `.agents/plans/portable-daemon-container.md`; that plan made the host node the primary data plane, which is the opposite of this goal. Read it only for the history of the rejected design.

## Purpose / Big Picture

Mjolnir has two daemon placements chosen by the existing `[daemon] runtime` setting. `host` is the default and behaves as it does today. `podman` or `docker` is an opt-in toggle that runs the daemon in a container. This plan makes the container placement genuinely container-native: the container is the daemon's machine, its named volume is its disk, and everything the daemon owns lives there.

After this plan, a person who enables container placement can run Mjolnir on a Linux host, Docker Desktop, `podman machine`, or later a Kubernetes pod without sharing a directory, PID namespace, network namespace, or uid with the host. They can restart or replace the daemon container without losing its store, configuration, SessionWiki index, harness homes, or session checkpoints. The host filesystem is not part of the architecture. A host directory appears inside the container only when the user explicitly adds it to `[daemon] mounts`.

To see it working on a machine with Docker or Podman, in an isolated instance:

    mj --instance t config set daemon.runtime docker
    mj --instance t sessions
    mj --instance t daemon status
    docker inspect mj-daemon-<digest> --format '{{json .Mounts}}'
    # exactly the named data volume, plus any explicit [daemon] mounts

The status says the daemon is in a container. `docker inspect` shows no host network mode and no host PID mode. Stopping and restarting the daemon reuses the same container and volume.

## Progress

- [x] (2026-11-04) Container placement is compiled on every platform instead of Linux only.
- [x] (2026-11-04) The launcher creates a container with a private network and PID namespace, one named data volume at `/var/lib/mjolnir`, one published loopback port, and only the explicit `[daemon] mounts`. It runs `sleep infinity` as the placeholder so the container can be reused.
- [x] (2026-11-04) The launcher starts `mj daemon-run` inside the container with `HOME`, `MJ_DATA_DIR`, `MJ_CONFIG_DIR`, `MJ_DAEMON_LISTEN`, `MJ_DAEMON_TOKEN`, `MJ_DAEMON_CONTAINER`, and `MJ_DAEMON_CONTAINER_RUNTIME` set. Logs go to the data volume.
- [x] (2026-11-04) The launcher publishes the host `daemon.json` with `pid: 0`; clients use `Identity` to learn the placement and container name.
- [x] (2026-11-04) A container daemon does not install the host command node or host filesystem. Its commands, workers, harnesses, and files all run inside its own container.
- [x] (2026-11-04) Stop, restart, and handoff follow the container daemon's endpoint instead of the host store guard. Forced stop uses the container engine.
- [x] (2026-11-04) The agent-dev image creates `/var/lib/mjolnir/home`, `/var/lib/mjolnir/data`, and `/var/lib/mjolnir/config` owned by its runtime user, so a fresh named volume is writable.
- [x] (2026-11-04) Use the existing agent-dev image as the daemon image. It already carries the daemon, the worker and every harness, which is what an all-in-one container needs; a separate small daemon image would leave local sessions without their runtime.
- [x] (2026-11-04) `mj setup` and `mj login` in container placement run inside the daemon container with the volume's `HOME`, data and config roots. The nested process carries `MJ_IN_CONTAINER` so it cannot try to launch another container.
- [x] (2026-11-04) The dashboard's automatic first-run setup is a daemon action (`RunFirstRunSetup`, protocol 77). It discovers and writes configuration in the daemon's own environment, so container placement writes the volume and host placement keeps using the host configuration.
- [ ] Give local session workers and their harness homes a container-native layout under the volume; verify a real turn in container placement.
- [ ] Add Docker/Podman end-to-end validation and a guard that the container has no host namespaces and no implicit mounts.
- [ ] Update human documentation and the Kubernetes manifest.

## Surprises & Discoveries

The old container placement shared the host network and PID namespaces and bind-mounted the host home, data directory, and configuration directory. That made the container look like a host process, so `daemon.json`, worker PIDs, and the SessionWiki scan all worked by accident. The first container-native implementation exposed every one of those assumptions: the host metadata file must be written by the launcher, the daemon cannot use the host store guard, and code that reads `~` means the container's home.

Host mode still uses those assumptions deliberately. This plan changes only the container placement. The `HostFs` and `mj node` work from the superseded plan remains useful for host placement but is not part of the container path.

## Decision Log

- Decision: Container placement is a container-native machine, not a host process with a different filesystem root. Rationale: the goal is to run the whole daemon in a container on every platform and in Kubernetes. Sharing host namespaces or bind-mounting the home only works on Linux and makes the container depend on the host. Date/Author: 2026-11-04.
- Decision: Keep the named volume and reuse the same container across daemon processes. Rationale: the volume is the durable data plane, and a `sleep infinity` placeholder keeps the container alive while a new daemon starts inside it. Date/Author: 2026-11-04.
- Decision: Host directories are opt-in through `[daemon] mounts`, never implicit. Rationale: users may want a project checkout, but the daemon must not require the host home, data directory, or runtime directory. Date/Author: 2026-11-04.
- Decision: The host launcher writes `daemon.json` with `pid: 0`. Rationale: a container PID is not a host PID. Clients already ask the daemon for `Identity`; `pid` remains only in the frozen `Status` shape. Date/Author: 2026-11-04.
- Decision: A container daemon runs commands locally inside its container instead of installing a node. Rationale: if everything is containerized, the container is where local work belongs. Date/Author: 2026-11-04.
- Decision: Use the agent-dev image as the daemon image rather than a small daemon-only image. Rationale: local sessions run their worker and harness in the same container, so that image must contain all three. Date/Author: 2026-11-04.

## Outcomes & Retrospective

The launcher and daemon process now follow the container-native model, but no Docker or Podman engine is installed in the development environment used for this change. The launcher's argument construction is unit-tested for the no-host-namespace and volume-only contract; a real container turn remains to be validated on a machine with an engine.

## Context and Orientation

The daemon is `mj daemon-run`, implemented in `mj-controller`. It owns the SQLite store, serves the API, and starts one `mj-worker` process per live session. The client command `mj` is in `mj-cli`; it reads `daemon.json`, connects to the daemon's loopback endpoint, and sends the protocol in `mj-client/src/daemon.rs`.

`[daemon] runtime` in `config.toml` is the toggle. `host` is the default. `podman` and `docker` select a container engine. The old implementation is `mj-cli/src/daemon/container.rs`. The container's own paths are `/var/lib/mjolnir/home` for `HOME`, `/var/lib/mjolnir/data` for `MJ_DATA_DIR`, and `/var/lib/mjolnir/config` for `MJ_CONFIG_DIR`.

## Plan of Work

The launcher in `mj-cli/src/daemon/container.rs` resolves an image whose `org.brokk.mjolnir.revision` label matches this build, creates a named volume, creates one container with `sleep infinity`, and starts `mj daemon-run` in it with `docker|podman exec`. It writes the host metadata file after the daemon answers `Ping`.

`mj-controller/src/daemon/process.rs` skips the node command host and host filesystem when `MJ_DAEMON_CONTAINER` is present. Commands therefore run in the container with the container's files.

`mj-cli/src/daemon.rs` treats a container daemon differently for liveness and exit waits: it follows the published endpoint and the container engine, not the host store guard or host PID.

Next, make container-mode configuration and first-run setup use the volume paths, give local workers their home and session state under `/var/lib/mjolnir`, and add an end-to-end test on a machine with Docker or Podman. The test must assert that `docker inspect` shows only the named volume (plus explicit mounts), no host network mode, and no host PID mode.

## Concrete Steps

Build and test without an engine:

    cargo fmt --all -- --check
    cargo test -p brokk-mjolnir daemon::container
    cargo test -p brokk-mjolnir --test daemon_startup
    cargo clippy -p brokk-mjolnir --all-targets -- -D warnings

On a machine with Docker or Podman, use an isolated instance:

    mj --instance container-native config set daemon.runtime docker
    mj --instance container-native sessions
    mj --instance container-native daemon status
    docker inspect mj-daemon-<digest> --format '{{json .HostConfig.NetworkMode}} {{json .HostConfig.PidMode}} {{json .Mounts}}'

Expect `default` (not `host`), an empty PID mode, and exactly the named volume plus any explicit mounts.

## Validation and Acceptance

A host-mode daemon must continue to pass the existing daemon startup and upgrade tests. A container-mode daemon must answer `mj daemon status` with its container name, must survive `mj daemon restart` while preserving the volume, and must run a real turn with the agent editing files inside the container or an explicit mount.

## Idempotence and Recovery

The launcher reuses an existing container and volume. If the container was created with another specification, an idle predecessor is removed; one still holding workers blocks a second container from opening the same volume and reports that instead of racing it. A failed launch removes the stale host metadata and names the container log in the volume.

## Artifacts and Notes

The unit test `a_daemon_container_shares_nothing_but_its_volume` asserts the `run` arguments contain the named volume, the explicit mount, a loopback publish, and none of `--network=host`, `--pid=host`, or `--userns`.

## Interfaces and Dependencies

`mj-cli/src/daemon/container.rs` exposes `Placement`, `ContainerSpec`, `launch`, `is_running`, `stop`, `log_tail`, and `remove_metadata` to the daemon launcher. `mj-client/src/daemon.rs` exposes `DaemonMetadata`, `DaemonClient`, `DaemonAction`, and `PROTOCOL_VERSION`. The container writes no host file except through the launcher's `write_launcher_metadata` in `mj-cli/src/daemon.rs`.
