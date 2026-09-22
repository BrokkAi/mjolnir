# Shard shared SSH connections and remove the silent direct-connection fallback

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir talks to remote machines over `ssh`. Today it sends every session for one machine through a single shared SSH connection. A stock `sshd` allows at most 10 sessions per connection (`MaxSessions 10`). When Mjolnir has more than 10 sessions on one machine, `sshd` refuses the eleventh, and OpenSSH's `ControlMaster=auto` then silently opens a separate direct connection instead. Many of those at once exceed `sshd`'s other stock limit (`MaxStartups 10:30:100`), which drops connections that have not finished logging in, and Mjolnir retries in a loop. On 2026-09-22 the daemon logged more than 3,000 such refusals against one host in a day, and the only remedy was to edit `sshd_config` on the remote machine.

After this change, Mjolnir works against a stock `sshd` with any number of sessions. It spreads sessions across several shared connections (called shards below), never lets `ssh` open a direct connection behind its back, and reports a clear error when a shared connection cannot be opened. A user can verify this by running more than 10 sessions against a remote host with default `sshd` settings and observing on the local machine that there are ceil(N / 8) master connections and no direct ones, and in the daemon log that no `Session open refused by peer` or `disabling multiplexing` lines appear.

## Progress

- [ ] Milestone 1: per-instance socket directory and shard-aware control paths.
- [ ] Milestone 2: session ledger with explicit master opening and the no-fallback guard.
- [ ] Milestone 3: route every `ssh`/`scp` spawn through the ledger, including the relay.
- [ ] Milestone 4: validation against a real host with stock `sshd`, then commit.

## Surprises & Discoveries

- Observation: OpenSSH falls back to a direct connection even with `ControlMaster=yes` when the socket already exists.
  Evidence (OpenSSH_10.2p1, 2026-09-22): a second `ssh -f -N -o ControlMaster=yes -o ControlPath=...` against a live socket printed `ControlSocket ... already exists, disabling multiplexing`, exited 0, and left a background `ssh -N` process holding its own TCP connection.
- Observation: `ControlMaster=no` plus `ProxyCommand=false` makes a client that can only use an existing master. With no master it exits 255 immediately with `Connection closed by UNKNOWN port 65535` and never opens a TCP connection. With a master present it runs normally, because a multiplexed client never invokes `ProxyCommand`.
  Evidence: tested by hand on 2026-09-22 against `morannon`.
- Observation: `ssh -f -N -o ControlMaster=yes -o ControlPersist=20 host` returns to the caller as soon as authentication completes, does not keep the caller's stdout/stderr pipes open, and the background master exits on its own about 20 seconds after its last client disconnects. `ssh -O check host` reports `Master running (pid=...)` while it lives and `No such file or directory` after.
  Evidence: tested by hand on 2026-09-22; the piped foreground command returned with exit 0 within a second.
- Observation: a reloaded `sshd` applies new `MaxSessions` only to new connections; an existing master keeps the limit it was started with.
  Evidence: `60-mj-mux.conf` was written on morannon at 18:16:59Z; refusals continued until the old master was replaced at about 18:24Z.

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

## Outcomes & Retrospective

(To be written at completion.)

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
