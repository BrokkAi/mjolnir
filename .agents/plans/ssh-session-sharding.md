# Shard shared SSH connections and remove the silent direct-connection fallback

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir talks to remote machines over `ssh`. Today it sends every session for one machine through a single shared SSH connection. A stock `sshd` allows at most 10 sessions per connection (`MaxSessions 10`). When Mjolnir has more than 10 sessions on one machine, `sshd` refuses the eleventh, and OpenSSH's `ControlMaster=auto` then silently opens a separate direct connection instead. Many of those at once exceed `sshd`'s other stock limit (`MaxStartups 10:30:100`), which drops connections that have not finished logging in, and Mjolnir retries in a loop. On 2026-09-22 the daemon logged more than 3,000 such refusals against one host in a day, and the only remedy was to edit `sshd_config` on the remote machine.

After this change, Mjolnir works against a stock `sshd` with any number of sessions. It spreads sessions across several shared connections (called shards below), never lets `ssh` open a direct connection behind its back, and reports a clear error when a shared connection cannot be opened. A user can verify this by running more than 10 sessions against a remote host with default `sshd` settings and observing on the local machine that there are ceil(N / 8) master connections and no direct ones, and in the daemon log that no `Session open refused by peer` or `disabling multiplexing` lines appear.

## Progress

- [x] (2026-09-22) Milestone 1: per-instance socket directory and shard-aware control paths. Sockets now live in `$XDG_RUNTIME_DIR/mjolnir/<instance>` (or `<data dir>/ssh`) and are named `<hash>-<shard>`; every command still uses shard 0 with `ControlMaster=auto` until Milestone 3. User `ssh_args` that configure sharing now suppress Mjolnir's options entirely.
- [x] (2026-09-22) Milestone 2: session ledger with explicit master opening and the no-fallback guard. `SshSessions::lease`, `SshSessionLease` (`control_path`, `invalidate`), `push_session_args`, `SESSIONS_PER_CONNECTION_ENV` and `SSH_MASTER_OPEN_TIMEOUT` exist in `mj-core/src/targets/ssh.rs`, tested with the hand-written `FakeMasters` executor. Nothing calls them yet. Validated with `cargo test -p mj-core` and `cargo clippy --all-targets -- -D warnings`; `mj-controller` does not use the new code yet, so its suite runs with Milestone 3.
- [x] (2026-09-22) Milestone 3: route every `ssh`/`scp` spawn through the ledger, including the relay. `CommandSpec` has `ssh_session: Option<SshTarget>` with the builder `ssh_session(&SshTarget)` and `open_ssh_session(&dyn CommandExecutor) -> SessionCommand`. `ssh_command_owned`, `scp_upload`/`scp_download`, `cache_host.rs` and `recovery_scan.rs` set it; `push_connection_sharing_args` is gone. The three executor spawn paths in `mj-core/src/targets.rs`, the relay in `mj-controller/src/worker_client/connect.rs` (lease stored in `RelayClient::ssh_session`, released only after the proxy is reaped), and the resource and capacity pollers in `mj-controller/src/pollers/capacity.rs` lease a session before spawning. `docs/SSH.md` describes the new behaviour.
- [x] (2026-09-22 22:05Z) Milestone 4: validation against `precision-3260`, which runs a stock `sshd` (`MaxSessions 10`). The env-gated test `sessions_shard_across_masters_on_a_real_host` (run with `MJ_E2E_SSH_HOST=precision-3260`) leased seven sessions at three per master, saw three masters running and a fourth absent, ran all seven concurrently with exit 0, and saw a guarded session with no master exit 255. A 45-second `mj --instance sshshard daemon-run` opened one master explicitly at startup (`opened a shared SSH connection ... /run/user/1000/mjolnir/sshshard/<hash>-0`) with no `disabling multiplexing` or `Session open refused` lines. The 12-attached-session check from Validation and Acceptance was not run; the mechanism it would exercise is covered by the test above and by the unit tests with fakes.

## Surprises & Discoveries

- Observation: OpenSSH falls back to a direct connection even with `ControlMaster=yes` when the socket already exists.
  Evidence (OpenSSH_10.2p1, 2026-09-22): a second `ssh -f -N -o ControlMaster=yes -o ControlPath=...` against a live socket printed `ControlSocket ... already exists, disabling multiplexing`, exited 0, and left a background `ssh -N` process holding its own TCP connection.
- Observation: `ControlMaster=no` plus `ProxyCommand=false` makes a client that can only use an existing master. With no master it exits 255 immediately with `Connection closed by UNKNOWN port 65535` and never opens a TCP connection. With a master present it runs normally, because a multiplexed client never invokes `ProxyCommand`.
  Evidence: tested by hand on 2026-09-22 against `morannon`.
- Observation: `ssh -f -N -o ControlMaster=yes -o ControlPersist=20 host` returns to the caller as soon as authentication completes, does not keep the caller's stdout/stderr pipes open, and the background master exits on its own about 20 seconds after its last client disconnects. `ssh -O check host` reports `Master running (pid=...)` while it lives and `No such file or directory` after.
  Evidence: tested by hand on 2026-09-22; the piped foreground command returned with exit 0 within a second.
- Observation: a reloaded `sshd` applies new `MaxSessions` only to new connections; an existing master keeps the limit it was started with.
  Evidence: `60-mj-mux.conf` was written on morannon at 18:16:59Z; refusals continued until the old master was replaced at about 18:24Z.

- Observation: `ssh` refuses the `-J` flag when a `ProxyCommand` came earlier on the command line, but accepts `-o ProxyJump=...` and then ignores it.
  Evidence (OpenSSH_10.2p1, `ssh -G`, 2026-09-22): `-o ProxyCommand=false -J foo` prints `Cannot specify -J with ProxyCommand`; `-o ProxyCommand=false -o ProxyJump=foo` reports `proxycommand false`; a `ProxyJump` from `ssh_config` is likewise overridden. `-J foo -o ProxyCommand=false` keeps `proxyjump foo`, so the guard must come first.
- Observation: `command_over_ssh` in `mj-controller/src/targets/container.rs`, which moves a container command to the remote host, kept the original command's metadata and so dropped the wrapped command's `ssh_destination`. Every provisioning, cleanup, recovery and preflight command it built ran without admission.
  Evidence: the function copied only `program` and `args` from the result of `ssh_command_owned`.
- Observation: the resource and capacity pollers (`execute_resource_command` in `mj-controller/src/pollers/capacity.rs`) spawn `ssh` themselves with `tokio::process`, outside the executors.
  Evidence: `grep -rn '.args(&command.args)' mj-controller/src`.

## Decision Log

- Decision: Shard sessions across several masters (about 8 per connection) instead of multiplexing all workers through one custom relay over a single SSH session.
  Rationale: the constraint is `sshd`'s per-connection session cap, not the connection count. Sharding needs no new protocol, no new remote process, and stays within the stock `MaxStartups` because `SshAdmission` already limits concurrent connection starts to 6 per destination. A custom multiplexer would still need session accounting for uploads and container commands.
  Date/Author: 2026-09-22, Jonathan Ellis with Claude.
- Decision: Remove OpenSSH's silent direct-connection fallback for every daemon-owned command by opening masters explicitly and running all other commands with `ControlMaster=no` and `ProxyCommand=false`.
  Rationale: `AGENTS.md`, "Engineering Guidance": a narrow fallback hides the primary design not working. The fallback here produced thousands of retries and a `MaxStartups` storm while looking like success.
  Date/Author: 2026-09-22, Jonathan Ellis with Claude.
- Decision: The default sessions-per-connection cap is 8, overridable with the environment variable `MJ_SSH_SESSIONS_PER_CONNECTION`.
  Rationale: stock `MaxSessions` is 10. Two are left free for `ssh` commands from other Mjolnir processes on the same machine (the doctor probe and Tab completion join a master but are not counted by the daemon's ledger). A per-machine config key can come later if anyone raises `MaxSessions`; do not add it now.
  Date/Author: 2026-09-22, Jonathan Ellis with Claude.
- Decision: Control sockets move into a per-instance directory.
  Rationale: the daemon's ledger is per process. Two daemons (for example `--instance hel` and `--instance hel2`) sharing one socket directory would each count only their own sessions and together exceed the cap. Separate directories give each daemon its own masters.
  Date/Author: 2026-09-22, Jonathan Ellis with Claude.
- Decision: If the user's own `ssh_args` for a machine already contain `ControlMaster`, `ControlPath`, or `-S`, Mjolnir adds no sharing options at all and does no ledger accounting for that destination.
  Rationale: today the user's option wins because OpenSSH keeps the first value it sees. Keeping that promise while adding the guard would be contradictory (the guard must come first to work). A user who configures sharing owns it.
  Date/Author: 2026-09-22, Jonathan Ellis with Claude.
- Decision: The doctor probe and Tab completion keep their current behaviour: they may join an existing master (shard 0) and otherwise open their own direct connection, without the guard.
  Rationale: they run in a CLI process with no ledger, and for a diagnosis a direct connection is the correct, stated behaviour, not a hidden fallback. `mj doctor` must still never leave a master behind.
  Date/Author: 2026-09-22, Jonathan Ellis with Claude.

- Decision: Name sockets `<hash>-<shard>`, where `<hash>` is the first 16 hex digits of a SHA-256 over the destination and the user's `ssh_args`, instead of `ssh`'s `%C-<shard>`.
  Rationale: `%C` hashes the resolved host, port and user, while the ledger counts sessions by the configured destination. Two destinations that resolve to one host (an alias and `user@address`) would share a `%C` socket but have separate counts, which could exceed the server's limit. With Mjolnir's own name, the socket and the count use the same key. The daemon also knows the concrete path, which it needs to remove a stale socket before opening a master; with `%C` it would have to run `ssh -G` first. The length check becomes exact: the reserve after the directory covers the separator, the 16-digit hash, `-` and a four-digit shard index, and the 17 characters (`.` plus 16 random characters) that `ssh` appends to the path while it binds a new master.
  Date/Author: 2026-09-22, Claude (implementation).
- Decision: `push_connection_reuse_args` (and the internal sharing helpers) take the `SshTarget`, and the doctor probe names its socket after the target as configured rather than after its probe-only overrides.
  Rationale: the socket name depends on the target's `ssh_args`; the probe adds `BatchMode=yes` and `StrictHostKeyChecking=yes`, and hashing those would point it at a socket no daemon uses.
  Date/Author: 2026-09-22, Claude (implementation).

- Decision: `SshSessionLease::control_path` returns `Option<&Path>` rather than `PathBuf`, and the lease has an `invalidate` method.
  Rationale: a lease for an unshared connection (escape hatch set, user-configured sharing, or no Unix sockets) has no socket. `invalidate` lets a caller that saw a session fail with a transport error make the next lease on that shard check the master again at once instead of trusting a check up to 5 seconds old; without it a retry within 5 seconds of a master's death would reuse the dead socket.
  Date/Author: 2026-09-22, Claude (implementation).
- Decision: The ledger does not retry a failed opener itself. The opener runs through the executor, which already admits it and retries a transport rejection up to three times; after that `lease` returns the error.
  Date/Author: 2026-09-22, Claude (implementation).

- Decision: The session options always go at the front of the argument list, instead of at an index recorded in the session request. The request is just the `SshTarget`.
  Rationale: the options must precede any user `ProxyCommand` or `ProxyJump`, and the front satisfies that for both `ssh` and `scp`. Some code rewrites a command's arguments after it is built (for example `CommandPlan`'s secret environment wrapping edits the last argument), and a stored index would silently go stale. At the front the options also override `ssh_config`.
  Date/Author: 2026-09-22, Claude (implementation).
- Decision: A `-J host` or `-Jhost` in the user's `ssh_args` is rewritten to `-o ProxyJump=host` in a session command (not in the opener, not for `scp`).
  Rationale: `ssh` rejects `-J` after `ProxyCommand` (see Surprises). The rewrite is a no-op for a session, which never connects by itself; the opener still receives the user's `-J` unchanged.
  Date/Author: 2026-09-22, Claude (implementation).
- Decision: The executors and the relay lease the session before taking the `SshAdmission` permit, not after as the plan said.
  Rationale: opening a master goes through the executor and takes a permit of its own. A caller that held a permit while waiting for the opener's permit could exhaust the gate: six callers holding all six permits, each waiting for an opener, would wait forever.
  Date/Author: 2026-09-22, Claude (implementation).
- Decision: When a session command fails with a transport rejection (exit 255 and a message such as `Connection closed by UNKNOWN port 65535`, which is how a guarded session reports a missing master), the executor and the relay invalidate the lease before the retry, so the retry checks the master and reopens it.
  Date/Author: 2026-09-22, Claude (implementation).
- Decision: `command_over_ssh` now copies `ssh_destination` and `ssh_session` from the wrapped command, and the pollers' `execute_resource_command` leases a session in `spawn_blocking` (with a `BoundedProcessExecutor` of `SSH_MASTER_OPEN_TIMEOUT`, 60 seconds per command). The relay uses the same executor for its lease.
  Rationale: without these, those commands would lose connection sharing entirely and open a direct connection each time, which is the load this plan removes. Copying `ssh_destination` also puts the container-over-SSH commands under admission, as the other SSH commands already were.
  Date/Author: 2026-09-22, Claude (implementation).
- Decision: The `mj doctor` runtime checks after the connectivity probe (for example the SSH Podman preflight) use ordinary session commands and therefore can open a master that lingers for `ControlPersist`. This was already true before this plan (they used `ControlMaster=auto`); only the connectivity probe is reuse-only. Left unchanged.
  Date/Author: 2026-09-22, Claude (implementation).

## Outcomes & Retrospective

Completed 2026-09-22 in three commits (`c4f88121`, `8a072cd8`, `59e249d5`) plus the real-host test. The daemon now works against a stock `sshd` with any number of sessions: sessions are placed on masters at 8 per connection, masters are opened explicitly and verified, and every other command can only use its master. What remains outside this plan: the running default-instance daemon still uses the old build and keeps 6 direct relay connections to `morannon` from before the fix; they clear when that daemon is upgraded to this build and its relays respawn. `morannon`'s `60-mj-mux.conf` (`MaxSessions 200`) is no longer needed but harmless. Lessons: OpenSSH's fallback exists in both `auto` and `yes` modes, so "no fallback" needs the guard on every non-master command, not a different `ControlMaster` value; and a per-process ledger only works when each process has its own socket directory.

## Context and Orientation

Mjolnir is a Rust workspace. The daemon (`mj daemon-run`, crate `mj-controller`) starts sessions on targets. For a target on another machine, every command runs through `ssh`, and files are copied with `scp`. All of that argument building lives in `mj-core/src/targets/ssh.rs`. The executors that actually spawn the processes live in `mj-core/src/targets.rs` (`CommandExecutor` implementations at about lines 585 and 902, and the retry wrapper `with_ssh_admission` at about line 493). One more spawn site is the relay: `mj-controller/src/worker_client/connect.rs` (`connect_attempt`, `spawn_and_handshake`) starts a long-lived `ssh ... hel worker proxy ...` process per attached session and talks to the worker through its stdin and stdout.

Terms used below:

- A "master" is a background `ssh` process that owns one authenticated TCP connection to a host and listens on a local Unix socket (the "control socket", chosen with `-o ControlPath=...`). OpenSSH calls this connection sharing or multiplexing.
- A "session" (OpenSSH also says "channel") is one remote command running inside a master's connection. `sshd` limits sessions per connection with `MaxSessions` (stock value 10).
- A "shard" is one master plus the sessions the daemon has assigned to it. This plan introduces the term; it does not exist in the code yet.
- The "relay" is the per-session `ssh` process described above. It is one session for as long as the daemon is attached to that Mjolnir session.

How sharing works in the code today: `push_control_args` in `mj-core/src/targets/ssh.rs` appends `-o ControlMaster=auto -o ControlPath=<dir>/%C -o ControlPersist=60` to every `ssh` and `scp` argument list (`ControlMaster=no` and no persist for the probe and completion paths via `push_connection_reuse_args`). `<dir>` is `$XDG_RUNTIME_DIR/mjolnir`, or `<data dir>/ssh` as a fallback, prepared by `control_socket_path` and `prepare_control_path`. `%C` is expanded by `ssh` to a hash of local host, remote host, port and user. Sharing can be switched off with `MJ_SSH_CONTROL_MASTER=off`, and tests pin the directory with `set_ssh_connection_sharing_for_test`.

Every `ssh`/`scp` `CommandSpec` carries `ssh_destination: Option<String>` (set by `.ssh_destination(...)` in `mj-core/src/targets.rs`). The executors use it to take an `SshAdmission` permit (a per-destination counting semaphore, default 6, `mj-core/src/targets/ssh.rs` around line 662) before spawning, so that no more than 6 connections are logging in at once. `with_ssh_admission` also retries up to `SSH_RETRY_ATTEMPTS` (3) when `is_transport_rejection` matches the stderr of a connection `sshd` dropped before authentication.

The instance name (from `--instance`) is available as `mj_core::config::instance_name()` (`mj-core/src/config/loading.rs:68`), and the data directory as `mj_core::config::data_dir()`.

Unix socket paths are limited to about 104 bytes. `MAX_CONTROL_PATH` and `prepare_control_path` already check this, reserving 64 bytes for the `%C` expansion. The shard suffix adds a few more characters, and the instance directory adds the instance name; both must be included in that check.

## Plan of Work

Milestone 1 changes where the sockets live and what they are called. In `control_socket_path`, make the directory `$XDG_RUNTIME_DIR/mjolnir/<instance>` where `<instance>` is `instance_name()` or `default`. Keep the data-directory fallback (it is already per instance). Change `CONTROL_PATH_FILE` usage so that a shard's socket is `<dir>/%C-<shard>` (for example `%C-0`, `%C-1`), and include the longest expected suffix in the length check. Update the existing unit tests that assert on the exact `ControlPath` value.

Milestone 2 adds the ledger and the two new argument shapes. In `mj-core/src/targets/ssh.rs`, next to `SshAdmission`, add a process-wide ledger keyed by destination. For each destination it tracks a list of shards; each shard has an index, the number of leased sessions, and the time of the last successful `-O check`. Provide:

    /// A leased session slot on one shard of a shared connection. Dropping it
    /// frees the slot.
    pub struct SshSessionLease { /* destination, shard index, Arc to ledger */ }

    impl SshSessionLease {
        /// The control socket path this session must use.
        pub fn control_path(&self) -> PathBuf;
    }

    pub struct SshSessions;
    impl SshSessions {
        /// Reserve a session on a shard for `destination`, opening that shard's
        /// master if it is not running. Blocks like `SshAdmission::acquire`
        /// (std::sync primitives, callable from plain threads and
        /// spawn_blocking). Returns an error only when a master could not be
        /// opened or verified.
        pub fn lease(ssh: &SshTarget, executor: &dyn CommandExecutor) -> Result<SshSessionLease>;
    }

`lease` picks the lowest-index shard whose count is below the cap (`MJ_SSH_SESSIONS_PER_CONNECTION`, default 8), creating a new shard when all are full. Before handing out the first lease on a shard, and whenever the shard's last check is older than 5 seconds, it verifies the master with `ssh <user args> -o ControlPath=<socket> -O check <destination>`. If that fails it removes a stale socket file if one exists, then runs the opener under a per-shard mutex:

    ssh <user ssh_args> -o BatchMode=yes -f -N -o ControlMaster=yes -o ControlPath=<socket> -o ControlPersist=60 <destination>

through the executor (so it gets an `SshAdmission` permit and the transport-rejection retry), then runs `-O check` again. If the check still fails, `lease` returns an error naming the destination and the opener's stderr; nothing else is attempted. The opener must exit within a bounded time; the executor's normal timeouts apply. `BatchMode=yes` is deliberate: an opener must never wait on a password prompt in the daemon.

Also in Milestone 2, replace `push_connection_sharing_args` with a function that appends the per-session options for a given lease:

    /// Options for a command that runs as one session on an already open
    /// master. `ProxyCommand=false` makes it impossible for `ssh` to open a
    /// direct connection: a multiplexed client never runs the proxy command,
    /// and a client that fails to reach the master exits 255 instead of
    /// connecting on its own.
    pub fn push_session_args(args: &mut Vec<String>, lease: &SshSessionLease)

which appends `-o ControlMaster=no -o ControlPath=<lease socket> -o ProxyCommand=false`. Because OpenSSH keeps the first occurrence of an option, these must be placed before any user `ssh_args` that could set `ProxyCommand` or `ProxyJump`; see the decision that a user who sets `ControlMaster`/`ControlPath`/`-S` in `ssh_args` opts out of Mjolnir sharing entirely (in that case `lease` returns a lease with no socket, and `push_session_args` appends nothing). Keep `push_connection_reuse_args` for the probe and completion paths, pointing at shard 0 of the per-instance directory and without `ProxyCommand=false`.

Milestone 3 wires the ledger into every spawn. The clean way is to let the `CommandSpec` say that it wants a session rather than carrying finished sharing options. Add to `CommandSpec` a field such as `ssh_session: Option<SshSessionRequest>` holding the `SshTarget` (destination and user args) and the index in `args` where the session options must be inserted. `ssh_command_with_control`, `scp_args`, `cache_host.rs:160` and `recovery_scan.rs:1013` set it instead of calling `push_connection_sharing_args`. Then, in the three executor spawn sites in `mj-core/src/targets.rs` and in `connect_attempt` in `mj-controller/src/worker_client/connect.rs`, call `SshSessions::lease` immediately after taking the `SshAdmission` permit, splice the session options into a copy of the args at the recorded index, and keep the lease alive for the child's lifetime. For the executors that means until the output is collected. For the relay it means storing the lease in the `WorkerClient` (or whatever struct owns the child) so it is dropped with the process; note that the `SshAdmission` permit is intentionally released after the handshake while the session lease must not be. Inside `with_ssh_admission`, take the lease inside the retry loop so a retry after a dead master re-leases and reopens.

Two places must not go through the ledger: the opener itself (it is the master) and the reuse-only commands (doctor probe, completion, `ssh_validation_command`). Give the opener a `CommandSpec` with `ssh_destination` set (for admission) but no session request.

Milestone 4 is validation on a real host (see below), followed by updating `README.md` or the relevant docs section if it describes `ControlMaster` behaviour, and committing.

Throughout, keep the `MJ_SSH_CONTROL_MASTER=off` escape hatch: with it set, no ledger, no opener, no guard, and every command opens its own direct connection as it does today. That is an explicit user choice, not a fallback.

## Concrete Steps

Work in `/home/jonathan/Projects/hel2` on branch `hel2`. Build and test outside the sandbox with elevated permissions, on the dev profile:

    cargo test -p mj-core
    cargo test -p mj-controller
    cargo clippy --all-targets -- -D warnings

Manual check against a host with stock `sshd` (`precision-3260` has the default `MaxSessions 10`; `morannon` has been raised to 200, so use `precision-3260` or temporarily point at another default host). Use an isolated instance:

    cargo build
    ./target/debug/mj --instance sshshard daemon-run &
    # create 12 or more sessions on a podman-on-ssh target for that host

Then on the local machine:

    ps -eo pid,args | grep '\[mux\]'          # expect ceil(N/8) masters for the host
    ss -tnp | grep ':22 ' | grep -v mux        # expect no relay with its own TCP connection
    grep -c 'disabling multiplexing' ~/.local/share/mjolnir/instances/sshshard/logs/*   # expect 0
    grep -c 'Session open refused' ~/.local/share/mjolnir/instances/sshshard/logs/*    # expect 0

(Adjust the log path to where that instance writes its daemon log; `mj --instance sshshard doctor` prints the data directory.)

## Validation and Acceptance

Unit tests in `mj-core/src/targets/ssh.rs` must prove: the ledger assigns leases to the lowest shard with room and opens a new shard at the cap; dropping a lease frees the slot; the per-instance directory and `%C-<n>` naming appear in generated arguments; `push_session_args` emits exactly `ControlMaster=no`, `ControlPath=<socket>`, `ProxyCommand=false` in that order and before user proxy options; a user `ssh_args` containing `ControlMaster=` or `ControlPath=` or `-S` produces no Mjolnir sharing options; the reuse-only commands still emit `ControlMaster=no` without `ProxyCommand=false` and never `ControlPersist`. Use `set_ssh_connection_sharing_for_test` so tests never touch the developer's runtime directory. For the opener and `-O check` logic, drive `lease` with a hand-written `CommandExecutor` fake that records the commands it was asked to run and returns scripted results (check fails, opener succeeds, check succeeds; and check fails, opener fails, expect an error naming the destination).

Acceptance is behavioural: on a stock-`sshd` host with 12 attached sessions, the local machine shows 2 masters and no direct relay connections, the daemon log has no `disabling multiplexing` or `Session open refused` lines, and killing a master (`ssh -o ControlPath=<socket> -O exit host`) causes the affected relays to reconnect through a freshly opened master rather than through direct connections.

## Idempotence and Recovery

All steps are ordinary code edits and can be repeated. Manual validation uses a named instance and never the default one. If a test leaves a master behind, `ssh -o ControlPath=<socket> -O exit <host>` stops it, and every master stops by itself 60 seconds after its last session. Stale socket files in the instance directory are removed by the opener path.

## Artifacts and Notes

Daemon log lines that this change must make disappear (from `~/.local/share/mjolnir/logs/mj-daemon-20260922T180706.081Z-3718686.log`):

    purpose=connect to Mjolnir worker line=mux_client_request_session: session request failed: Session open refused by peer
    purpose=connect to Mjolnir worker line=ControlSocket /run/user/1000/mjolnir/f97c... already exists, disabling multiplexing

Matching `sshd` journal lines on the host during the same window:

    sshd[6024]: drop connection #13 from [192.168.1.180]:62089 on [192.168.1.77]:22 Maxstartups

## Interfaces and Dependencies

No new crates. In `mj-core/src/targets/ssh.rs` the public surface after this plan is: `SshSessions::lease`, `SshSessionLease` (with `control_path`), `push_session_args`, `push_connection_reuse_args` (kept), `SshAdmission` (unchanged), `SESSIONS_PER_CONNECTION_ENV` (`MJ_SSH_SESSIONS_PER_CONNECTION`), `CONTROL_MASTER_ENV` (unchanged), and `set_ssh_connection_sharing_for_test` (unchanged). `push_connection_sharing_args` is removed. `CommandSpec` in `mj-core/src/targets.rs` gains the session request field described in Milestone 3 and a builder method for it.

## Revision note (2026-11-06): learned capacity replaces the fixed cap

The fixed 8 sessions per connection and `MJ_SSH_SESSIONS_PER_CONNECTION` are removed, and so are the `SshAdmission` gate and the Decision Log entries that relied on it (the entries above are the 2026-09-22 record). `AGENTS.md` now says not to add caps that guard against a failure nobody has observed, and to read a real limit from the other side when it reports one. The server reports this limit: `sshd` refuses a session past `MaxSessions` with `mux_client_request_session: session request failed: Session open refused by peer` and exit status 255, before the remote command starts. A fixed 8 was wrong in both directions: it wasted capacity on hosts that allow 64 or 200 (`precision-3260`, `morannon`), and it was above `MaxSessions` on a host configured below 8.

The ledger now has one placement rule: a new session goes on the lowest-numbered master whose capacity is not known to be reached. A shard's capacity starts unknown. When `sshd` refuses a session on a shard, `SshSessionLease::note_refusal` records the number of other sessions the shard carried at that moment (at least one; a later refusal can only lower it) and the command is retried on the next eligible shard without a delay or a spent attempt, because the recorded capacity is the state change that ends the loop. A refusal that records nothing new (capacity already known, or a connection closed before authentication) keeps the existing jittered backoff and three attempts. A shard forgets its capacity when its master is reopened. Each learned capacity is logged once at info level.

Paths that cannot retry (a caller's stream through `execute_with_stdin`, and the commands a node runs for the daemon) still record the capacity, so the next lease goes elsewhere; they return the refusal to the caller. The relay connect, the resource poller and the executors retry through the same `note_refusal` result. The env-gated real-host test is replaced by `sessions_past_the_servers_max_sessions_succeed_across_masters_on_a_real_sshd`, which starts a user-mode `sshd` with `MaxSessions 2` on 127.0.0.1.

Same revision, retry budget: without `SshAdmission`, a burst of connection starts can keep `sshd`'s `MaxStartups` full for longer than the old three attempts (about six seconds of delay). A refusal before authentication means the server is up and asking the client to come back, and the command never ran, so `SshRefusalRetry` (`mj-core/src/targets/ssh.rs`) now retries it until it succeeds or the caller's cancellation or deadline ends it. The delay keeps its jittered shape (0.5 to 2 seconds, then 2 to 4 seconds for every later attempt); that is a backoff shape, not a limit. One warning is logged when a command has been refused for over a minute. Retries that waiting cannot help, or that nothing outside could stop, keep `SSH_BOUNDED_RETRY_ATTEMPTS` (3): a session refused by a master whose capacity is already known, and `ProcessExecutor`, which has no cancellation.

## Revision note (2026-11-06, later): every SSH session proves it started

Review of 5cb64e019 found four gaps in learning the limit from a refusal. (1) A streamed command (`execute_with_stdin`, the checkpoint and Move uploads, a node-hosted relay) returned the refusal instead of retrying, because the caller's stdin might already be partly consumed, so the first refusal on a full master, which is now an ordinary event, could fail a Move or a checkpoint. (2) Capacity was inferred as `leased - 1` when the refusal was handled; sessions that ended meanwhile made it too low (it only went down, so it stayed low) and reservations not yet opened made it too high. (3) A node-hosted relay leases on the node, so the daemon's retry loop saw no lease and counted each learned refusal as unproductive; three refusals failed the relay. (4) Node preparation opened masters with `ProcessExecutor`, which stops after three pre-authentication refusals and returns an error without an SSH status, so a `MaxStartups` burst failed node-hosted reconnects.

One mechanism closes all four: **every remote command mj sends over `ssh` first prints a start marker on stdout, then runs.** A session that ended without any stdout byte provably never started its command, so it is always safe to retry; a session that printed the marker has started and is never retried, even when it later exits 255. The ledger counts a session as open only from the marker.

### Design

- **Marker** (`mj-core/src/targets/ssh.rs`, `SSH_START_MARKER` = `MJOLNIR-SSH-SESSION-STARTED\n`). `CommandSpec::open_ssh_session` prefixes the remote command line (the last argument of every `ssh` session command) with `printf '%s\n' 'MJOLNIR-SSH-SESSION-STARTED'; `. It is added at lease time, not where builders compose the line, because every leased `ssh` session passes through that one function, builders and their test fixtures stay unchanged, and the one caller that must not have it (below) can ask for a lease without it. The line stays POSIX shell text for the account's login shell, exactly the assumption `posix_quote` already makes; `;` (not `&&` or `exec`) is used because callers such as `provide_target_environment_secret` prepend compound statements (`IFS= read -r X || exit 1; export X; exec ...`) to the line, and `;` is valid in sh, bash, zsh, fish and csh.
- **Excluded** (keep today's behaviour, classified by stderr alone as before): `scp` (the program is not `ssh`, there is no remote command line), the master opener (`-f -N`, no command) and `ssh -O check/exit` (they run no remote command, and are not sessions), `path_completion`'s `ssh_completions` (a reuse-only command that is not a leased session), and the shell wrapper that Move hands to `rsync --rsh` (rsync reads ssh's stdout as its own protocol, and appends its server arguments to the remote line, so a marker would corrupt the stream; `open_ssh_session_without_marker`). Sessions on direct connections (sharing off, user-configured sharing, Windows) do get the marker, as decision A says.
- **Reading.** `StartScan` is a pure state machine fed stdout chunks. The first stdout byte proves the session started. If the stream begins with the marker, the scanner removes it and passes the rest on; if the first bytes diverge from the marker (a shell rc file printing before the command, or a forced command), the bytes pass through untouched and the session still counts as started; a partial marker split across reads is held until the next chunk decides it, and flushed as ordinary output at end of file. `StartGate` (one per lease, `SshSessionLease::gate`) records Waiting / Started / Ended (end of stdout with no byte) and, on start, confirms the lease in the ledger. It has a blocking wait (the sync executors) and an async wait (the node and the relay). Readers: `PipeCollector` (both process executors and `stream_command_with_stdin`), the node's stdout pump, and `streamed_child::spawn_proxy` for the daemon's local relay proxy and resource probes.
- **Refusal** (`StartGate::classify`): exit 255 with the existing stderr classification (`SessionLimit` or `BeforeAuthentication`) *and* no stdout byte. Anything that produced a byte, including the marker, is a real result. Commands that carry no marker keep the stderr rule.
- **Stdin hold-back** (`stream_command_with_stdin`, node `run_child`). Nothing is read from the caller's stream or written to ssh's stdin until the gate reports Started; when it ends without starting, the writer returns without having read. `execute_with_stdin` on both executors therefore retries a refused streamed command with `SshRefusalRetry`, reading the same reader afresh. The writer still copies in 64 KB chunks while stdout and stderr are drained concurrently (Subprocess Rules). A host whose command never prints the marker (it must run a POSIX shell command line for the login shell, which mj needs anyway) would hold stdin until ssh exits; this is the one cost of D and is listed under limitations in `docs/SSH.md`.
- **Capacity from confirmed sessions** (`SessionLedger`). A shard keeps `reserved` (leased, not yet started), `open` (started and not ended), `high_water` (largest `open` since the master opened) and `capacity`. A lease moves Reserved to Open when its gate starts and leaves either state when dropped, all under the ledger lock (one owner of each fact). On the first `SessionLimit` refusal on a shard whose capacity is unknown, capacity becomes `max(high_water, 1)`: the server accepted that many at once, so it is a lower bound. It is never lowered; if later confirmations push `high_water` past it, capacity rises to match. A later refusal teaches nothing and uses the ordinary backoff and the three-attempt bound, except when the refused lease was placed while the shard's capacity was still unknown (a stale placement made before another refusal taught it): that refusal is retried at once without spending an attempt, because the next placement lands elsewhere. A reopened master starts a new *generation*: capacity, high-water and the counts reset, and only leases that join the new generation (every lease finishes `lease()` by rebinding to the shard's current generation) count; a late refusal from an older generation is retried at once and teaches nothing. Placement: the lowest shard where `open + reserved` is below its known capacity (unknown means unlimited); a new shard only when all are full.
- **Retry where the lease lives** (`mj-core/src/node.rs`). `serve_stream` (the relay and other live streams the daemon runs on its node) now owns a retry loop: lease with a `CancellableProcessExecutor` tied to the stream (a reset or drop cancels it, so preparation no longer stops after three pre-authentication refusals), spawn, hold stdin back until the marker, withhold stderr until the attempt starts or ends, and on a refusal discard the attempt and retry with `SshRefusalRetry::new(true)` without sending anything to the daemon. `serve_run` already runs the node's `CancellableProcessExecutor`, so its streamed commands inherit the executor fix. The daemon sees at most the node's final refusal, treated as an unexplained one. The daemon's resource poller lets the node lease its probes instead of leasing on the daemon and sending an already bound command to the node.
- **Relay in the daemon's own process** (`connect.rs`, `capacity.rs`): `spawn_proxy` strips the marker from the proxy's stdout and exposes the lease's gate; the handshake failure path classifies a refusal with the gate, so exit 255 after the marker is a failure of the worker and never a retry.

### File map

`mj-core/src/targets/ssh.rs` (marker, `StartScan`, `StartGate`, ledger counts, generations, `note_refusal`), `mj-core/src/targets.rs` (`open_ssh_session`, `SessionCommand`, executors, `stream_command_with_stdin`, `PipeCollector`, retry), `mj-core/src/node.rs` (`serve_stream`, `run_child`), `mj-controller/src/streamed_child.rs` (`spawn_proxy`), `mj-controller/src/worker_client/connect.rs`, `mj-controller/src/pollers/capacity.rs`, `mj-controller/src/controller/move_session/transfer.rs` (rsync wrapper opts out), `docs/SSH.md`.

### Tests

The real-`sshd` test (`MaxSessions 2`) runs concurrent plain commands and a streamed command with 192 KB of stdin while the master fills, and asserts the learned capacity is 2. Tricky-condition tests: a marker split across reads, a stream that diverges from the marker, exit 255 after the marker is not retried, a streamed command refused twice still delivers its whole stdin once, a reopened master forgets capacity and ignores a late refusal from the old one, and a node-hosted relay refused repeatedly connects without the daemon seeing the refusals.

## Revision note (2026-11-06, last): a walk across shards replaces learned capacity

Pre-push review of 75da0c2b0 found that the ledger cannot know how many sessions the server will accept on a master. (1) Another process on the same control socket (an overlapping daemon during a handoff, `mj doctor`, Tab completion, each with its own ledger) can fill a master. This ledger recorded a capacity of at least one from its own low count, kept placing on that shard, and the refusals at "known capacity" taught nothing and exhausted three attempts, so commands failed while the daemon was healthy. (2) A channel can close on the server and another be accepted before the first local lease drops, so local `open` overlapped and inflated `high_water` above the real `MaxSessions`; nothing lowered it. (3) A race between `verified_at` and the generation reset in `restart_shard` let a lease join the old generation for a new master. The fact "how many sessions this server accepts on this master right now" belongs to the server, so the ledger stops estimating it.

This revision replaces the "Capacity from confirmed sessions" design above (and the 2026-11-06 learned-capacity note). The start marker stays: it still makes every refusal before the remote command starts safe to retry.

### Design

- **The ledger owns only what this process owns.** Per connection key: which shards exist, each one's control socket (named from the key and the shard index, so other processes find the same sockets), when its master was last verified alive, and a soft hint. There is no capacity, high-water, open/reserved split or generation.
- **Placement is a walk, one per command.** `SshWalk` (owned by the command's `SshRefusalRetry`) lists the shards the command has already tried. Each lease takes the first shard it has not tried, in order starting at the hint and wrapping, and opens a new shard only when it has tried all that exist. A `SessionLimit` refusal (exit 255 before the start marker, existing stderr classification) therefore moves the same command to the next shard at once, with no backoff and no attempt counted: the walk is the state change that ends the loop. A command whose lease has no shared master (unshared) cannot walk and keeps three attempts.
- **The hint** is the shard that most recently started a session for the connection. It only chooses where a walk starts. A stale hint costs extra refusals and never a failure.
- **Termination guard.** A master this process opens is checked with one real session (`true`) while the process holds the master's opening locks, so that session is the first one the server sees on it and no burst of other leases can race it. If the server refuses that first session for `SessionLimit`, it does not allow shared sessions (`MaxSessions 0`). The master is closed and `lease` fails with an error that names the host and the sshd setting. The process neither opens masters in a loop nor falls back to a direct connection (`AGENTS.md`: no quiet degradation). A burst of leases cannot trigger the guard by itself: when many leases are refused at once, their markers have not arrived yet, so only a session that runs alone proves "refused with nothing else in flight".
- **Pre-authentication refusals** keep `SshRefusalRetry`'s behaviour (backoff until the caller's cancellation or deadline; three attempts without one).
- **Move's rsync** (`transfer.rs`) cannot carry the marker, because its stdout is the rsync protocol. It is idempotent, so when rsync fails and the `ssh` stderr it relays shows a refusal, `copy_workspace` runs the whole rsync again with the same walk: the next attempt uses the next shard, and pre-authentication refusals back off as above.
- **Node stderr** (`node.rs`): `run_child` now holds stderr until the attempt has started, not only while it waits, so a refused attempt's stderr is classified with the exit and discarded (logged at debug) instead of flushing when stdout ends.
- **Probes** (`lease_probe`) walk the same way but never open a master; past the last known shard a probe uses the next index, which joins that shard's master if some process opened it and otherwise connects directly, as probes always did.

### File map

`mj-core/src/targets/ssh.rs` (ledger, `SshWalk`, first-session check, `SshRefusalRetry`), `mj-core/src/targets.rs` (`open_ssh_session` takes the walk), `mj-core/src/node.rs`, `mj-controller/src/worker_client/connect.rs`, `mj-controller/src/pollers/capacity.rs`, `mj-controller/src/controller/move_session/transfer.rs`, `docs/SSH.md`.

### Tests

The real-`sshd` test (`MaxSessions 2`) gains a second ledger that fills the first master through the same control socket; the first ledger's commands still all succeed by walking. A `MaxSessions 0` server fails the lease with the guard error and leaves no master behind. Ledger tests of capacity, high-water and generations are removed. Tricky-condition tests: a refused-then-successful node stream delivers no stderr from the refused attempt; Move's rsync retry moves to the next shard.
