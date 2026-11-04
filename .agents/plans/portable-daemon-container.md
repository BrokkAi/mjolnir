# Run the daemon in a container that shares nothing with the host

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`. It supersedes `.agents/plans/daemon-in-container.md`, whose design this plan replaces; read that plan only for history.


## Purpose / Big Picture

The daemon is the background process `mj daemon-run` (crate `mj-controller`). It owns the session database, serves the API and web viewer, and starts the per-session worker processes (crate `mj-worker`) that run agents. Users asked to run the daemon in a container so that it behaves the same on Linux, macOS and Windows and so that it can later run as a Kubernetes pod.

Release 2.65.0 shipped a version that only works on Linux. It runs the daemon in a Docker or Podman container that shares the host's network and process namespaces (`--network=host`, `--pid=host`), runs as the user's uid, and bind-mounts `$HOME`, the data and configuration directories, `/tmp`, `$XDG_RUNTIME_DIR`, `/etc/passwd` and the Docker socket at their host paths. Clients then treat the containerized daemon exactly like a host process: they probe its PID with `kill(2)` and read `/proc/<pid>/environ` and `/proc/<pid>/exe`. On macOS and Windows the engines run Linux containers inside a virtual machine, so "host" namespaces and host paths mean the VM's, not the user's; in Kubernetes there is no shared host at all. The module is compiled only on Linux (`#[cfg(target_os = "linux")]` in `mj-cli/src/daemon.rs`) and quietly starts a host daemon elsewhere. That is the narrow fallback `AGENTS.md` forbids.

After this plan, the containerized daemon is a network service that shares no directory, namespace or uid with the computer the user works on. Its store, logs and configuration live in a named volume. Clients reach it through a published TCP port with a token, the same way on every platform. Work that must happen next to the user's files (local sessions, git on local checkouts, harness logins, quota checks, transcript import, local container sessions) is done by a host node, `mj node`. The node is a small process on the user's computer that opens a TCP connection to the daemon and executes what the daemon asks over that connection. The daemon never interprets a host path; the node does.

To see it working, on any of Linux, macOS (Docker Desktop or `podman machine`) or Windows (Docker Desktop), in an isolated instance:

    mj --instance t config set daemon.runtime docker
    mj --instance t sessions                       # starts the daemon container and the node
    mj --instance t daemon status                  # "Daemon: container mj-daemon-… (docker), this build"
    docker inspect mj-daemon-<digest> --format '{{json .Mounts}}'
                                                   # exactly one entry: the named data volume
    mj --instance t new --harness claude ~/src/demo   # a local session; the agent edits ~/src/demo

Setting the runtime back to `host` moves the daemon and its store out of the volume with the same graceful handoff used for upgrades, and the running session keeps answering throughout.


## Terms

A placement is where the daemon runs: `host` (a detached process on this computer) or `container` (a Docker or Podman container). The configuration key is `[daemon] runtime`, values `host`, `podman`, `docker`.

The client is any of `mj` (CLI and terminal dashboard, crate `mj-cli`), the web viewer, and the desktop shell. They talk to the daemon through `mj-client`.

The node is the new `mj node` process. There is at most one per instance per computer. It is not a worker: workers run one session's harness, while the node runs short commands and relays worker connections for every local session. The protocol between node and daemon is the node protocol.

A locator (`TargetLocator`, `mj-core/src/targets.rs`) is the stored record of where a session's worker lives: local bare, local container, SSH, SSH container, EC2. `locator_command` (`mj-core/src/targets.rs`, around line 2390) wraps a command so that it runs on that target. Today local targets run the command directly on the daemon's computer, containers through `podman|docker exec`, and SSH through `ssh` over a shared ControlMaster.

The relay is the worker's protocol: newline-delimited JSON with a Hello handshake, served by the worker on `<worker root>/control.sock`. The daemon reaches it by spawning `<root>/hel worker proxy --root <root>` through the locator and talking to the proxy's stdin and stdout (`RelayClient`, `mj-controller/src/worker_client.rs` and `worker_client/connect.rs`).


## How it works

### One executor for local work: the node

Every operation that today runs "on the daemon's computer" because the target is local runs through the node, in both placements. This is the central decision. A host daemon and a container daemon then differ only in where the daemon process lives, not in how local work is done, and Linux CI exercises the same code path that macOS and Windows users run. There is no second "direct exec" path to keep in agreement.

The node protocol is a multiplexed stream protocol over one TCP connection. The node dials the daemon's published address, authenticates with the instance token plus a node identity (a random id stored in the node's own state directory), and then the daemon opens channels on that connection. A channel is either an exec (program, arguments, environment, working directory, and three byte streams for stdin, stdout and stderr, then an exit status) or a relay stream to a worker's `control.sock`. TCP carries both directions, so it does not matter that the node dialed. A node behind NAT, on a laptop, or outside a Kubernetes cluster works as long as it can reach the daemon's address. Use an existing multiplexer crate if the workspace already has one (search `Cargo.lock` for `yamux` or `tokio-util` codec use first). Otherwise write a small length-prefixed framing with per-channel flow control windows. Flow control is mandatory: a stalled stdout reader must not block other channels, and tests must drive more than 64KB through a channel in each direction (see `AGENTS.md`, Subprocess Rules).

`locator_command` and `command_on_locator` gain a single executor abstraction. For `LocalBare` and local container locators it is the node's exec channel; for SSH targets it is also the node, which runs the system `ssh` exactly as today, because the user's SSH keys, agent and configuration are on their computer. `RelayClient` changes from owning a `Child` to owning a boxed `AsyncRead + AsyncWrite` pair, so that it accepts a node relay channel. The worker side needs no change, because the node runs `hel worker proxy` through an exec channel exactly as the daemon spawns it today, and `forward_proxy_streams` (`mj-worker/src/worker_runtime/unix/supervisor.rs`) is already generic.

Worker roots for local sessions live under the node's state directory on the user's computer (`<data dir>/node/workers/<session id>`), not under the daemon's data directory. Existing host sessions keep their stored roots (`<data dir>/workers/<session id>`), which the node serves unchanged because both are host paths.

The node is started by the client, exactly where `connect_or_start` starts the daemon today (`mj-cli/src/daemon.rs`, function `connect_or_start`). It is detached and survives clients. On losing the daemon connection it redials with exponential backoff capped at a few seconds, so a daemon handoff reconnects it without any client. Workers are detached from the node (started with the existing `nohup` script), so a node restart never stops a turn. A node of a different build is replaced the way the daemon is: the client asks it to drain its open channels and exit, then starts its own build. Node replacement waits only for open exec channels, which are short, never for worker turns.

When no node is connected, the daemon reports local targets as unavailable with the reason "no node from this computer is connected". It never falls back to running anything itself. A daemon in Kubernetes with no node connected still serves SSH-less remote targets that it can reach itself (EC2 through the AWS API, for example) and the API; that is the expected shape there.

### What moves to the node

The daemon currently reads host files and spawns host programs in many places. Each moves to a node exec or to a node request, and the daemon keeps only what is about its own state. In terms of the current code:

- Harness logins and profile homes: credential sync reads (`mj-controller/src/worker_client/credential_sync.rs`, `opencode_logins.rs`, `aws_credentials.rs`, `kimi_auth.rs`), profile stage installation (`worker_binary/install.rs`) and `local_profile_homes.rs` become node requests.
- Quota checks (`claude_usage.rs`, `codex_usage.rs`, `grok_usage.rs`, `quota_runtime.rs`) run through the node.
- Model discovery (`controller/profile_config.rs`) and harness preflight (`worker_binary/harness.rs`) run through the node.
- Native transcript import (`import/native_import.rs`) and SessionWiki indexing (`sessionwiki.rs`, `native_adapters`) read host transcripts. The node reads the files and streams them to the daemon. The SessionWiki index itself moves into the daemon's data directory.
- Project paths, worktrees and clones (`controller.rs` `interpret_repository_source`, `project_catalog.rs`, `controller/worktree.rs`, `provision.rs` `validate_bare_project_path`, `review_host/environment.rs`, `import/bundles.rs`) and path completion (`controller/path_completion.rs`) run on the node.
- Build and git caches (`mbx.rs`, `tool_cache.rs`, `git_cache.rs`, `cache_host.rs`) are host directories, so they are created and mounted through the node.
- Checkpoint pulls from local workers (`checkpoint_transfer.rs`, currently `cp`) become a stream over a node exec of `tar` or of the worker's own export command.
- Pinned worker binaries (`binary_source.rs`) are installed onto the node's computer through the node.
- Local container sessions (`targets/container.rs` `container_run_args`) are created by running the engine CLI through the node, so every bind-mount source is a host path that the host engine understands. Docker copy-on-write helper containers and smoke-test mounts move the same way.
- Tailscale (`tailscale.rs`) runs through the node.
- The web viewer's port-owner check (`web_viewer.rs`, `/proc/net/tcp` and pidfd) applies only to a host daemon. In a container the daemon owns its own network namespace, and the published port is checked by the engine.

Path text crosses the node protocol as UTF-8 strings plus a flag naming the node's platform. They are converted to `PathBuf` on the node only. The daemon stores and displays them but never joins, canonicalizes or checks them. Put this conversion in one helper next to the node protocol types and reuse it everywhere (`AGENTS.md`: one place for shared interpretation).

### Clients stop touching the store and the daemon's process

Clients read the SQLite store directly in several places. Those reads become API calls before the store can move into a volume:

- the dashboard state loads in `mj-cli/src/dashboard/io/spawn.rs` (`load_state`, `load_materialized_projection_tail`, `load_workspace_layout`, `load_workspace_pane_sizes`)
- `load_mount_history` in `mj-cli/src/dashboard/actions.rs`
- `client_read_frontier` in `mj-cli/src/dashboard.rs`
- `database_path` and `list_workspaces` in `mj-cli/src/main.rs`
- `mj import` saving state directly (`mj-cli/src/import.rs`)

Each gets a daemon action in `mj-client/src/daemon.rs`. Extend the daemon API golden exchange rather than adding unit tests.

Every client use of the daemon's PID is replaced by something the daemon or the engine answers:

- Liveness and "was it replaced": a `Status` call returning a per-start instance id. `Status` already returns build version and revision (`mj-controller/src/daemon/actions.rs`).
- Waiting for exit after a stop or handoff: for a host daemon, the store guard (`ControllerStoreGuard::try_acquire`) becoming free; for a container, the engine reporting the container stopped.
- The SIGTERM fallback: for a container, `docker|podman stop`.
- Build identity (`process_runs_this_executable`, `/proc/<pid>/exe`): `BuildIdentity::same_source` on `Status`. `daemon_runs_this_build` stays the one owner of that answer.
- Placement (`/proc/<pid>/environ`, `MJ_DAEMON_CONTAINER`): `Status` reports the placement, and `decide_existing_daemon` stays its one owner.
- `Attach { pid }` and `prune_dead_clients`: a lease bound to the attachment connection, refreshed by the existing two-second `maintain_attachment` and expired after a few missed refreshes.

The host daemon keeps working for older clients during the compatibility window below.

`daemon.json` stays the discovery file and stays on the client's computer. For a host daemon, the daemon writes it as today. For a container daemon, the client launcher writes it: the published address, the token it generated and passed to the container as an environment variable, `pid: 0`, and the build version. Because `DaemonMetadata` is `deny_unknown_fields`, the placement is not added to it. It lives in a second file, `daemon-placement.json`, that only new clients read, and which a container launch writes before `daemon.json`. An older client that finds `pid: 0` treats the daemon as gone and would try to start a host daemon on the host store. The host-store tombstone described next makes that attempt fail visibly instead of forking the data.

### The store moves with the placement

The store, `daemon.log`, the SessionWiki index and the daemon's configuration live in the named volume `mj-<instance digest>-data`, mounted at `/var/lib/mjolnir` in the container, with `MJ_DATA_DIR` and `MJ_CONFIG_DIR` pointing into it. Moving between placements is the existing graceful handoff with one extra step between "the old daemon exited and released the store" and "launch the new daemon": the client copies the store. That means `daemon.sqlite3` with its WAL checkpointed, the configuration, and the SessionWiki index. Into the volume it uses a short-lived helper container and `tar` over stdin; out of the volume it uses the same in reverse. After copying out of the host, the client leaves a tombstone in the host data directory: a store marker that sets the minimum compatible revision above every existing release, with the reason "this instance's store moved to container <name>". Older binaries then refuse to open the stale host copy, through the existing read-compatibility check (`database::check_read_compatibility`), and say why. Moving back to the host replaces the tombstoned host store with the volume's copy. The copy is one bounded control operation inside the handoff, so the handoff admission rules in `AGENTS.md` hold: workers are never waited on, because they live on the node's computer and reconnect.

The daemon's own configuration and the client's placement setting are split. `[daemon]` stays in the host `config.toml`, because the client needs it before any daemon exists. Every other table is served and edited through the existing settings API (`server/settings.rs`, `server/api/config.rs`), backed by the file in whichever data directory the daemon uses. Users who edited `config.toml` by hand and then move to a container keep their file. The copy step carries it, and the settings surfaces remain the way to edit it afterwards; the client prints where the file now lives.

### The container

`mj-cli/src/daemon/container.rs` is rewritten. It is compiled on every platform and uses no `libc`, no `/proc` and no `std::os::unix`. A container spec is:

- the engine
- the image
- the named data volume
- one published port, bound to `127.0.0.1:<port>` on the host, where `<port>` is the port the daemon listens on inside the container
- the token as an environment variable
- the daemon as the container's entrypoint (`mj daemon-run --listen 0.0.0.0:<port>`)

It has nothing else: no host network, no host PID, no user mapping, no bind mounts, no engine socket. The container's lifetime is the daemon's. A handoff stops the old container after the old daemon exits and starts a new one; there is no `sleep infinity` holder, because no worker runs in the daemon's container any more.

The daemon image is a new, small image built from `containers/Containerfile.daemon`. It holds only the `mj` binary built for linux-musl and CA certificates. It is published per release as `ghcr.io/brokkai/mjolnir/daemon:<version>` for amd64 and arm64 by the release workflow, and on master pushes as `:latest` and `:<revision>` the way `publish-agent-dev-image.yml` publishes agent-dev. The client uses `daemon:<its own version>` (or the configured `[daemon] image`), checks the `org.brokk.mjolnir.revision` label against its own revision, and on a development build with no matching image builds one locally with `scripts/build-daemon-image.sh`. The agent-dev image loses the `mj` daemon binary, `docker-cli` and `podman-remote` that 2.65.0 added, because harnesses no longer run beside the daemon.

`mj daemon-run` gains `--listen <addr>`. With it, the daemon binds that address instead of `127.0.0.1:0` and does not write `daemon.json` (the launcher owns it). In container mode the daemon requires the token on every connection, including from loopback, because the published port is the only way in.

Kubernetes uses the same image, volume layout and flags. A manifest (a `StatefulSet` with one replica, a `PersistentVolumeClaim` at `/var/lib/mjolnir`, a `Service`, and a `Secret` holding the token) goes into `containers/k8s/` as the last milestone. A user's laptop joins with `mj node --connect <address> --token-file <file>`. Shipping a Helm chart or operator is out of scope.

### Upgrade from 2.65.0

Configuration version 22 is breaking, because older readers must not act on its meaning. It keeps `[daemon] runtime` and `image`, drops `mounts` (nothing is bind-mounted any more), and migrates version 20 and 21 files forward with a notice listing any dropped mounts and saying they are no longer needed.

A daemon running in a 2.65.0-style container is recognized by the old `MJ_DAEMON_CONTAINER` environment on Linux; that is the only platform where it can exist. The new client hands it off as an ordinary `Replace`. Its local workers live inside that old container but under host paths and host PIDs, because the old design shared both. The new node can therefore reach their `control.sock` and pidfiles at the same paths. The old container is removed once only its `sleep` placeholder runs in it, using the existing `remove_idle_predecessors` check. Keep that one function and its legacy recognizer, label both "2.65.0 legacy", and plan to delete them once no supported upgrade path starts from 2.65.x.

This is an isolated upgrade regression: start a 2.65.0 container daemon with a running session in an isolated instance, upgrade, and check that the session answers a turn afterwards and the old container goes away after the session is destroyed.


## Plan of work

Milestone 1 makes clients independent of the store and of the daemon's process. Add the API actions for every direct store read and the `Status` instance id and placement fields. Replace every PID use listed above, and convert attachments to connection leases. At the end, `grep -rn "process_is_alive\|/proc/\|load_state\|database_path" mj-cli mj-client mj-tui` finds no client use outside the host-daemon launcher and its tests, and the full suite passes with the daemon still on the host.

Milestone 2 adds `mj node` and the node protocol, and routes every local exec and every local relay connection through it with the daemon still on the host. Start with a prototype: one daemon, one node, an exec channel running `cat` on 1MB in both directions concurrently, plus a relay channel to a real worker. That proves the framing and flow control before converting call sites. Then convert `locator_command`, `command_on_locator`, `start_worker_command` and `RelayClient`, followed by the host-reading call sites in the "What moves to the node" list, one area per commit. At the end, a host daemon run with an empty `PATH` and a working directory it cannot read still runs a local session end to end, because only the node touches the host.

Milestone 3 rewrites the container launcher as described, adds `--listen`, the daemon image, the store copy with tombstone, the configuration version 22 migration and the 2.65.0 legacy handoff. It deletes everything else in the old launcher.

Milestone 4 validates on every platform and documents it: update `docs/src/content/docs/targets.md`, `cli-reference.md` and `mj-core/assets/skills/mj/references/configuration.md`, and add the Kubernetes manifest.


## Concrete steps and validation

Run all commands from the repository root. Use `--instance` for every daemon, CLI and end-to-end run, as `AGENTS.md` requires, and run `cargo test` outside the sandbox.

After each milestone run `cargo clippy --all-targets -- -D warnings` and the touched crates' suites, for example:

    cargo test -p mj-client -p mj-cli -p mj-controller -p mj-worker

The tests to add, per the "Which tests to add" rules in `AGENTS.md`:

- Extend the daemon API golden exchange with the new actions and the `Status` fields.
- An end-to-end test runs a real `mj`, daemon, node and worker in an isolated instance with the host placement and runs a turn through a recorded harness.
- A tricky-condition test drives more than 64KB both ways through concurrent exec channels, with one reader stalled.
- A tricky-condition test checks that a node reconnects after a daemon handoff while a worker turn continues.
- An integration test covers the store copy and the tombstone against an older daemon revision's read-compatibility check.
- The isolated 2.65.0 upgrade regression described above.

Container validation on Linux CI runs the same end-to-end test with `runtime = "docker"`. It also asserts that `docker inspect` shows exactly one mount (the named volume) and no `HostConfig.NetworkMode` of `host`. This is the guard against the old design returning.

macOS and Windows: GitHub's hosted macOS runners have no Docker, and its Windows runners do not run Linux containers. Therefore the nightly lanes run the host-placement end-to-end test, which exercises the node protocol on those platforms, and the container placement is validated by hand on a Mac with Docker Desktop and a Windows machine with Docker Desktop. Record each run's transcript under `Outcomes & Retrospective`. Do not describe the container placement as validated on a platform where that hand run has not happened.

Each milestone ends with a commit pushed to master, as `AGENTS.md` requires. Platform-sensitive milestones (2 and 3) also dispatch `nightly.yml` on master after the push.


## Progress

- [x] (2026-11-03) Diagnosed the 2.65.0 design and wrote this plan.
- [x] (2026-11-03) Milestone 1a: the terminal dashboard and CLI no longer open the store. New daemon actions `DashboardStartup`, `WorkspaceLayouts`, `StoredSessionMetadata`, `StoredProjectionTail`, `StoredSessionSummary`, `SaveSetup`, `ArchiveSpacePreview`, `ResumeRepositoryPreflight` and `ReplaceResumeRepositoryOrigin` (protocol 72). Git status and working context use the dashboard's live runtime state; the new-session preflight uses a configuration-only controller. The remaining store access in clients is the host-daemon launcher (`ControllerStoreGuard`, `check_read_compatibility` in `mj-cli/src/daemon.rs`), which Milestone 3 confines to the host placement.
- [x] (2026-11-03) Milestone 1b: client attachments are leases renewed by the two-second heartbeat and expired after 15 seconds (`ATTACHMENT_LEASE`), not client PIDs probed with `kill(2)`.
- [x] (2026-11-04) Milestone 1c: the daemon answers a non-frozen `Identity` action (protocol 75) with a per-start instance id, its build version and its placement. Clients use it for replacement detection, build identity and the placement decision instead of `kill(2)`, `/proc/<pid>/exe` and `/proc/<pid>/environ`; exit waits use the store guard, and a container daemon that reports its engine is stopped through the engine. The legacy `/proc` placement probe survives only for daemons that predate `Identity` (2.65.x), beside the legacy launcher.
- [x] (2026-11-04) Milestone 1d: configuration ownership. A daemon `Configuration` action serves the config (protocol 76, chunked like the runtime snapshot), and `PersistImportedSession` now writes an imported bundle with its session. `go`/`simple`/`import`, the dashboard's new-session preflight, remote-repair and imported-session paths use the daemon's config or actions instead of `Config::load`/`Config::update`. The remaining direct reads are the client's own: `[daemon]` placement, terminal theme/spinner/symbols/keybinds, splash, and the bootstrap commands that must work before a daemon exists (`mj setup`, `mj login`, first-run setup).
- [x] (2026-11-03) Milestone 2: node prototype in `mj-core/src/node.rs`. The node protocol is HTTP/2 (the `h2` crate, already in the lock file) over the connection the node opens; the daemon is the HTTP/2 client and each exec is one stream. Tested with megabytes both ways through concurrent execs past a stalled reader, and with a reset killing the child's process group.
- [x] (2026-11-03) Milestone 2: `mj node` runs beside every daemon this build launches. The launcher chooses the daemon's endpoint and token (`MJ_DAEMON_LISTEN`, `MJ_DAEMON_TOKEN`), writes them to `node-endpoint.json` and starts the node, which connects while the daemon is still starting (the daemon serves only `NodeConnect` until it is ready). `mj_controller::node_registry` owns which node is connected. The `mj daemon-run` process installs a command host, so every `ProcessExecutor` and `CancellableProcessExecutor` command runs on the node with the node's own executor; commands on the daemon's own data opt out with `CommandSpec::in_daemon()`. Verified live in an isolated instance: `mj recover scan` ran `sh`, `docker`, `ps` and `podman` on the node.
- [x] (2026-11-04) Milestone 2: the relay proxy to workers runs through the node's `/exec` stream (`mj-controller/src/worker_client/proxy.rs`), with the SSH session lease and admission slot taken on the node, where SSH runs. A daemon that stops for good (explicit stop, idle exit, owner exit) dismisses its node, which exits with it; a handoff does not, so the node serves the next daemon. Verified with the reliability lab (`tests/e2e/run-reliability.sh`, multi-client-happy-path and active-stop): every provisioning command, worker start and stop, and every relay connection ran on the node, with no leaked processes.
- [x] (2026-11-04) Milestone 2: daemon-data commands that are self-contained are marked `in_daemon()`: the controller staging-space probe before a Move and the Move verification of the controller's staging directory. Cross-node checkpoint and workspace copies are not marked because one side lives on the node; they become the streamed transfers described next.
- [x] (2026-11-04) Milestone 2: streaming children go through the shared `mj-controller/src/streamed_child.rs` module (the relay proxy moved there). The capacity poller, Codex `app-server` quota probe and Grok `agent stdio` probe use it, so they run on the node when a command host is installed and locally in tests. `CommandSpec` gained `env_remove`, applied by the local executor and the node, so the Codex probe still hides inherited API keys. Bounded output collection keeps a chatty child from blocking a pipe. `controller/update.rs` and the import `git` calls run in the client and stay.
- [x] (2026-11-04) Milestone 2: the node filesystem protocol: `/fs/read`, `/fs/write`, `/fs/list` and `/fs/metadata` streams in `mj-core/src/node.rs`, with `NodeClient::fs_*` methods, path text crossing as UTF-8, and a test that moves 300KB both ways. The `HostFs` handle and call-site conversion remain.
- [x] (2026-11-04) Milestone 2: `mj-controller/src/host_fs.rs` installs the daemon's `HostFs` handle over the node registry: async and blocking read/write/list/metadata run through the node in a `daemon-run` process and locally in tests or in-process daemons. The doctor's review-residue scan and worker freshness check use it. The remaining call-site conversion is below.
- [ ] Milestone 2: convert the daemon's host-file reads to `HostFs`. Daemon files that read the user's home today: `doctor.rs`, `utility_llm.rs`, `setup.rs`, `setup/availability.rs`, `sessionwiki.rs`, `quota.rs`, `targets/provision.rs`, `project_catalog.rs`, `pollers/worker_targets.rs`, `pollers/provider_auth.rs`, `opencode_logins.rs`, `import/*`, `controller/worker_binary/{launch,harness,catalog}.rs`, `controller/reviewer.rs`, `controller/move_session.rs`, `controller/cache_host.rs`, `codex_usage.rs`, `aws_credentials.rs`, plus `std::fs` on project paths in `controller/worktree.rs` and `controller/path_completion.rs`. The container CI run in Milestone 3 is what proves none is missed.
- [ ] Milestone 3: portable launcher, `--listen`, daemon image, store copy and tombstone, config version 22, 2.65.0 legacy handoff.
- [ ] Milestone 4: hand validation on macOS and Windows, docs, Kubernetes manifest.


## Surprises & Discoveries

Clients read the SQLite store directly (dashboard state, workspace layout, mount history, read frontier, `mj import`). The 2.65.0 design only worked because the store was bind-mounted at the same path. Any container design that does not share the data directory has to remove those reads first, which is why Milestone 1 exists.

Local container sessions bind-mount host paths (project workspaces, build caches, the git cache, attachments). A daemon in a container cannot name those paths for the host engine, so the engine commands that create them must run on the node, not in the daemon.


The dashboard was a second writer. Repairing a stopped session's repository origin before resume (`replace_resume_repository_origin`) ran in the terminal process and saved the session record and `config.toml` itself. It now runs in the daemon. Most other `Controller::load()` calls in the dashboard loaded the whole store and then ignored it: lifecycle closures never used the controller they were given, and reloads only needed mount history and container sizes.

`DaemonStatus` is part of the frozen management subset and cannot gain fields, so the daemon's instance identity and placement need a separate, non-frozen action.

The first prototype deadlocked in exactly the test meant to catch it. HTTP/2 counts unread bytes against the whole connection as well as the stream, and the default connection window is 64KB, so one exec nobody reads stalled every other exec. The connection window is now the per-stream window times the stream limit (256KB times 256), so a stalled stream can only ever stall itself.

Tests that start `mj daemon-run` directly have no node, so a command in them waits the 30-second `NODE_CONNECT_WAIT` before it fails; `mj-cli/tests/daemon_startup.rs` went from 24 to 67 seconds. Those tests should start a node beside their daemon, as the launcher does.

## Decision Log

The daemon shares no directory with the host. The alternative considered, bind-mounting declared host directories and translating paths at the protocol boundary, was rejected because it keeps the daemon dependent on host paths. It also cannot work in Kubernetes and adds a translation table every path-handling site must agree with. The owner asked why the daemon needs to share any directories at all, and the honest answer was that it does not once local work moves to the host. (2026-11-03)

Local work always goes through the node, including with a host daemon. A direct-exec path for host daemons would be a second way to do the same thing and would leave the container path untested on Linux CI. (2026-11-03)

The node dials the daemon. TCP carries both directions, so only reachability decides who dials. The daemon's address is the one stable, published endpoint in every placement; a laptop's is not. (2026-11-03)

The node protocol is HTTP/2 rather than a hand-written multiplexer. The workspace already depends on `h2` through hyper; it provides per-stream flow control, resets and a stream limit, which are exactly the parts a hand-written framing would get wrong. (2026-11-03)

SSH runs on the node, because the user's keys, agent and configuration are there. A Kubernetes daemon without a connected node therefore cannot reach SSH targets. That is accepted for now and stated in the Kubernetes docs. (2026-11-03)

The 2.65.0 design is replaced, not kept as a Linux fast path, per the owner. (2026-11-03)

With no daemon running, `mj new` refuses an unknown or missing `--workspace` without starting one (launch findings R2-14 and R5-9). Only the store knows the workspaces then. For the host placement the client reads its local store read-only while no daemon runs, as before; for a container placement the store is in the volume, so the name is let through and the daemon refuses it. Starting a daemon only to refuse a typo was judged a worse regression than this split. (2026-11-03)

Attachment leases expire after 15 seconds. A client renews every two seconds and each renewal may take up to five, so a live client never comes close; a daemon set to exit when idle now exits up to 15 seconds after its last client stops rather than on its next idle tick. (2026-11-03)

The daemon owns stored settings. A client may read the host file only for `[daemon]` (it must decide the placement before a daemon exists), for terminal presentation it draws itself (theme, spinner, symbols, keybinds), and for bootstrap commands that must work before any daemon or database exists (`mj setup`, `mj login`, first-run setup). Everything that runs beside a daemon asks the daemon: `Configuration` for reads, `SaveSetup` for settings edits, `CreateProject` for project creation, and `PersistImportedSession` for an import's bundle and session. Milestone 3 moves the file into the data volume; the bootstrap exception is then served by the node or the daemon rather than by a host file read. (2026-11-04)


## Outcomes & Retrospective

None yet.
