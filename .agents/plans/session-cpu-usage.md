# Show per-session CPU in the sessions list and a "CPU by session" report

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

A person running many agent sessions on one machine wants to find the sessions that use the most CPU, so they can move them from a local machine to an EC2 machine before those sessions slow the others down. Today nothing shows per-session CPU. A container-only resource poller existed until commit `5a7ec3f3` ("Remove the per-session resource poller"); it was removed because every open TUI ran `podman exec` plus `podman container inspect --size` for every container session once a minute, with no limit on concurrency. That stalled podman for 7 to 15 seconds on a host with about 47 containers, and nothing displayed the samples anyway.

After this change:

1. Each row in the TUI sessions list shows the session's current CPU use as a share of its machine, for example `23%`, refreshed about every 10 seconds. Sessions using less than 1% show nothing, so idle rows stay quiet.
2. The menu under the Targets pane title (opened by clicking the title, or with the `TargetsMenu` command) has a new item, "CPU by session…". It opens a dialog that lists the live sessions grouped by machine, with each session's CPU share averaged over roughly the last hour, highest first. The dialog updates while open.

The cost is one scan of the worker's own process tree every 10 seconds inside each worker, and one extra small request on a connection the daemon already holds open. No `podman`, `docker`, or `ssh` processes are started.

To see it working: start an isolated daemon and TUI with `--instance cpu-test`, start a session, have the session run a CPU-bound command, and watch its row show a rising percentage within about 20 seconds. Then open the Targets menu, choose "CPU by session…", and see the session at the top of its machine's group.


## Progress

- [ ] Milestone 1: worker-side measurement (process-tree CPU time on Linux and macOS, sampler with recent and hourly figures), with unit and behavior tests.
- [ ] Milestone 2: relay protocol 29 with the connection-only `CpuUsage` request, worker handler, controller client method, with relay tests.
- [ ] Milestone 3: daemon sampling in the session actor, the shared CPU table, publication through the runtime feed, with feed tests.
- [ ] Milestone 4: TUI row display and the "CPU by session…" dialog, with render tests.
- [ ] Milestone 5: end-to-end check in an isolated instance; full `cargo test` and `cargo clippy --all-targets -- -D warnings` on the dev profile; commit.


## Surprises & Discoveries

- Observation: the Linux per-thread `children` file, which would let a process list its direct children without scanning all of `/proc`, is missing on the WSL2 development kernel. It exists on morannon. A full `/proc` scan is therefore the portable way to find descendants.
  Evidence: `ls /proc/self/task/*/children` on WSL2 6.18.33.2 printed "No such file or directory". The same command on morannon listed the file.

- Observation: a full `/proc/<pid>/stat` scan is cheap enough to run every 10 seconds. In Python on the WSL2 machine it took about 18 ms for 424 processes, and Rust will be faster. morannon has about 2,742 processes host-wide and 96 CPUs. Container workers see only their container's processes, because each container has its own process ID namespace, so their scans are much smaller.
  Evidence: a timing loop over `os.listdir('/proc')` with reads of each `stat` file printed `424 procs 18.3 ms per scan (python)`.

- Observation: sub-agent child sessions can borrow their parent's container. The `TargetLocator` variants `LocalPodman`, `LocalDocker`, `AppleContainer` and `SshPodman` in `mj-core/src/targets.rs` carry `borrowed_from: Option<String>`. A container-wide counter such as the cgroup file `/sys/fs/cgroup/cpu.stat` therefore measures the parent and all its borrowing children together, not one session.

- Observation: the Mr Boxington build cache (`mbx`), which Rust builds here use, starts no long-running daemon. Its README says it starts an agent for each build and stops it when the build ends. Rust builds started by an agent therefore stay inside the worker's process tree and are counted.


## Decision Log

- Decision: each worker measures the CPU time of its own process tree on every target type, instead of reading the container's cgroup counter.
  Rationale: borrowed containers make a cgroup counter shared between sessions (see Surprises). One code path also covers bare hosts, containers and EC2 machines, and the worker needs no knowledge of its target type. Accepted limitation: processes that leave the worker's tree are not counted. Examples are programs that detach by forking twice (so they are re-parented to process 1), and processes that `podman exec` starts directly in the container rather than through the worker. The plan does not make the worker a "child subreaper" (a Linux process setting that re-parents orphaned descendants to it), because the worker would then have to reap processes it did not spawn, which conflicts with tokio's own child handling.
  Date/Author: 2026-10-01, Claude (plan author) with the user.

- Decision: the worker computes both the recent and the hourly figure, and the daemon only relays them.
  Rationale: `AGENTS.md` ("Control plane and data plane") says the daemon must not keep in memory anything a worker or the database cannot give back. The daemon restarts on every upgrade. A worker-side average survives those restarts, while a worker restart resets the process-tree counter anyway.
  Date/Author: 2026-10-01, Claude.

- Decision: report CPU as a share of the machine's online logical CPUs (0% to 100%), using `libc::sysconf(libc::_SC_NPROCESSORS_ONLN)` read by the worker. Do not use `std::thread::available_parallelism`.
  Rationale: the user preferred a percentage to "cores", which is confusing with hyperthreading. A share of the machine also adds up to the host CPU figure the Targets pane already shows. `available_parallelism` applies the container's cgroup CPU quota (`--cpus=4` would give 4), which would overstate a container session's share of the machine. An earlier idea in the conversation was for the daemon to divide by the capacity poller's core count. It changed because the worker runs on the measured machine, and EC2 fleet entries have no per-machine core count that a session could be matched to.
  Date/Author: 2026-10-01, Claude.

- Decision: the hourly figure is a time-weighted, exponentially decaying average with a one-hour time constant, corrected for start-up, plus a "covered seconds" figure capped at 3,600.
  Rationale: this is the same idea as the Unix load average shown by `uptime`. It needs two numbers of state instead of an hour of stored samples, and it handles irregular intervals. The start-up correction (dividing by the decayed sum of interval lengths) makes a 5-minute-old session show its true 5-minute average rather than a value pulled towards zero. Covered seconds let the dialog mark figures that do not yet cover a full hour.
  Date/Author: 2026-10-01, Claude.

- Decision: sample every 10 seconds. The sessions list shows the share over the newest interval.
  Rationale: the user wants "near real time" in the list. Ten seconds keeps the scan cost negligible and limits runtime-feed publications to one every 10 seconds.
  Date/Author: 2026-10-01, Claude.

- Decision: carry CPU figures in a new keyed map in the runtime feed (`RuntimeProjection.session_cpu`), published at most once every 10 seconds for all sessions together. Do not put them in `RuntimeSessionView` or `RuntimeMetadata`.
  Rationale: a change to `RuntimeSessionView` passes through `publish_view` and the remote poller's `PublishedView` comparison, which can trigger a full projection read for that session. `RuntimeMetadata` is resent whole (configuration included) whenever any field changes. That would push configuration-sized deltas every 10 seconds and use up the feed's 4,096-entry, 16 MB history budget in `mj-controller/src/daemon/feed.rs`. A keyed map sends only the entries that changed.
  Date/Author: 2026-10-01, Claude.

- Decision: name the menu item and dialog "CPU by session".
  Rationale: "usage" already means harness quota usage in this codebase (the quota poller, `mj-client/src/quota.rs`, and the "usage endpoint"), so "Usage report" would be ambiguous. The user suggested "usage report" and asked for a better name.
  Date/Author: 2026-10-01, Claude.

- Decision: sessions list rows show the figure only when it is at least 1.0%.
  Rationale: most sessions are idle most of the time, and `0.1%` on every row would be noise. On a 96-CPU machine, one fully busy core is about 1%, so any meaningful load still appears.
  Date/Author: 2026-10-01, Claude.


## Outcomes & Retrospective

Nothing implemented yet.


## Context and Orientation

Read this section before editing anything. It explains the parts of the system involved and names the files.

Mjolnir runs coding agents ("harnesses" such as Claude Code or Codex) in "sessions". Each session runs on a "target": the local machine directly ("bare"), a local Podman, Docker or Apple container, a remote machine over SSH, or an AWS EC2 machine. `mj-core/src/targets.rs` defines `TargetLocator`, the enum that records where one session runs.

Each session has one "worker", an `mj-worker` process that runs on the target. The worker starts the harness process and everything the agent runs (builds, tests, shells, and the second-opinion reviewer). All of those are descendants of the worker process in the operating system's process tree. The worker code lives in `mj-worker/src/`. Its connection handling is in `mj-worker/src/worker_runtime/unix.rs`.

The "daemon" (`mj daemon-run`, crate `mj-controller`) is the control plane. It owns the database and talks to every worker over a "relay" connection, a request/response protocol defined in `mj-core/src/relay/protocol.rs` (`RelayRequest`, `RelayResponsePayload`). `RELAY_PROTOCOL_VERSION` in `mj-core/src/relay.rs` is the protocol version, currently 28. A worker serves exactly its own version. The daemon can still talk to older workers, and it refuses to send a request the older worker cannot decode. `RelayRequest::minimum_protocol` and `RelayRequest::supported_at` in `protocol.rs` implement that check. Some requests are "connection-only": the worker answers them directly on the connection, and they never enter the worker's durable journal. Examples are `CredentialState`, `SkillsState` and `GithubTokenState`. The worker routes them in `mj-worker/src/worker_runtime/unix.rs` near the `matches!(&envelope.request, RelayRequest::CredentialState | ...)` block, and `mj-worker/src/relay/requests.rs` lists them as invalid if they ever reach the durable relay.

Inside the daemon, each live session has a "session actor": a tokio task in `mj-controller/src/session_manager/actor.rs` that holds a long-lived relay connection (`StandaloneSession`, in `mj-controller/src/session_manager/standalone.rs`). On each tick it calls `sync_actor_connection`, which calls `StandaloneSession::sync_in_place`. Ticks run every 150 ms while the worker is busy and every 2 s while it is quiet (`SESSION_SYNC_INTERVAL` and `QUIET_SESSION_SYNC_INTERVAL` in `mj-controller/src/session_manager.rs`). The controller's typed relay calls are methods on `RelayClient` in `mj-controller/src/worker_client/relay.rs`, for example `history_requests`, which also shows how to skip a call an older worker does not support.

The daemon publishes what clients display through the "runtime feed". `mj-client/src/runtime_feed.rs` defines `RuntimeProjection`, the full picture, which holds keyed maps such as `sessions` and `native_agents` plus `metadata`. It also defines `RuntimeDelta`, the change between two projections, built by `RuntimeDelta::between`, checked by `is_empty` and applied by `apply`. A `SnapshotMap` (`mj-core/src/snapshot_map.rs`) is the map type used for those keyed maps. In the daemon, `mj-controller/src/daemon/feed.rs` captures projections from the state owner (`RuntimeStateOwner` in `mj-controller/src/daemon/owner.rs`) and keeps a bounded history of them. `RuntimeHistory::live_projection` narrows a projection to live sessions for terminal clients. A capture is triggered by `publish_revision()` (see `publish_quotas` in `mj-controller/src/daemon/views.rs` for a small example).

On the client side, the TUI's feed consumer turns frames into dashboard updates. `mj-controller/src/pollers/runtime_feed.rs` (`RemoteDashboardWorkerPoller`, `RuntimeStateUpdate`) and `mj-cli/src/dashboard/drains.rs` (see `set_native_agent_snapshot(update.native_agents)` near line 313) pass them into `DashboardState`, whose ingest methods are in `mj-tui/src/ingest.rs`. The native-agents map is the model to copy for a keyed map that reaches the TUI.

The TUI sessions list is drawn by `mj-tui/src/render/sessions.rs`. Each row's second line is built by `session_activity_line`, which lays out the status word, an optional queue count, then the muted target and profile identity, cutting text to fit the width. A width of 24 cells or less is "compact".

The Targets pane menu is built in `mj-tui/src/pane_controls.rs`, `begin_support_pane_menu`. For `SupportPane::Targets` it lists "Refresh", then `("Runtimes…", CommandId::ManageTargets)` and `("Machines…", CommandId::ManageMachines)`. Commands are declared in `mj-tui/src/actions.rs` (the `CommandId` enum, the command table entry with label and description, and the handler `match`), and help grouping is in `mj-tui/src/help.rs`. A read-only modal dialog with a close button already exists for the notice log: `NoticeLogDialog` (`Mode::NoticeLog` in `mj-tui/src/lib.rs`, `impl DialogModal for NoticeLogDialog` in `mj-tui/src/modal_surface.rs`, rendered by `render_notice_log` from `mj-tui/src/render.rs`, events in `mj-tui/src/component_events.rs`, `DialogControl::NoticeLogClose` in `mj-tui/src/dialogs.rs`). Copy that dialog's structure.

The Targets pane groups targets by machine. `mj-tui/src/render/capacity.rs` reads `dashboard.capacity_details`. Each detail has `target.host`, the machine's label, and `target.target_ids`, the target template IDs that run on that machine. Every session record has `target_template_id`. This is how the dialog maps a session to its machine.

Two OS facts the measurement code relies on:

On Linux, `/proc/<pid>/stat` is one line. Its second field is the command name in parentheses, which may itself contain spaces and parentheses, so parse the remaining fields after the last `)`. Counting the whole line's fields from 1, field 4 is the parent process ID, 14 `utime` and 15 `stime` (CPU time this process used, in user and kernel mode), and 16 `cutime` and 17 `cstime` (CPU time of its children that have exited and been waited for). Units are clock ticks, `libc::sysconf(libc::_SC_CLK_TCK)` per second (usually 100). The values include all threads.

On macOS, `libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, ...)` fills a `libc::rusage_info_v2` whose `ri_user_time`, `ri_system_time`, `ri_child_user_time` and `ri_child_system_time` have the same meaning. On Apple Silicon these are in Mach absolute-time units, not nanoseconds. Convert with `libc::mach_timebase_info` (nanoseconds = value × numer ÷ denom). On Intel the ratio is 1:1. `libc::proc_listchildpids(pid, buffer, size)` lists a process's direct children. Verify whether its return value is a count of PIDs or of bytes against Apple's `libproc.c` before relying on it. The crate's `libc` 0.2.186 exposes all of these (confirmed in `src/unix/bsd/apple/mod.rs`).

Why the sum is correct: when a process exits and its parent waits for it, the kernel adds the child's total (its own time plus its own waited-for children) into the parent's `cutime`/`cstime`. So adding `utime + stime + cutime + cstime` over every live process in the tree, the root included, counts each CPU-second used by any present or past descendant exactly once, as long as the waiting parent was itself in the tree. The total can go down: when a process in the tree is re-parented out of it, or when a parent that has not yet waited for a child is itself re-parented. The sampler treats a decrease as zero use for that interval and continues from the new value.


## Plan of Work

### Milestone 1: worker-side measurement

At the end of this milestone the worker crate can measure its own process tree's CPU time and turn successive measurements into the two figures. No other code uses it yet. Tests prove the measurement on a real busy child process.

Create `mj-worker/src/cpu_usage.rs` and declare it in `mj-worker/src/lib.rs`. It contains:

A function `process_tree_cpu_time(root: u32) -> anyhow::Result<std::time::Duration>`, which dispatches to a platform module. On Linux (`#[cfg(target_os = "linux")]`, in `mj-worker/src/cpu_usage/linux.rs`): list the numeric entries of `/proc`, read each `stat`, and build a map from parent PID to child PIDs. Walk breadth-first from `root` and add `utime + stime + cutime + cstime` of every process reached, then convert ticks to a `Duration`. A process that disappears between listing and reading (`NotFound`, or a read error for a PID other than `root`) is skipped. Failing to read `root` itself is an error. On macOS (`#[cfg(target_os = "macos")]`, in `mj-worker/src/cpu_usage/macos.rs`): walk from `root` with `proc_listchildpids`, add the four `rusage_info_v2` fields from `proc_pid_rusage`, and convert using `mach_timebase_info`. A child that has exited by the time of the call (`ESRCH`) is skipped. On every other platform, return an error saying CPU sampling is not supported on this platform. Do not add a second measurement method anywhere. `.github/macos-ci-paths.txt` matches files with a `target_os = "macos"` gate automatically, so macOS CI runs for this change.

A function `online_cpus() -> anyhow::Result<u32>` that calls `libc::sysconf(libc::_SC_NPROCESSORS_ONLN)` on both Unix platforms and returns an error for zero or negative results.

A plain struct `CpuSampler`, independent of time sources so tests can drive it:

    pub struct CpuSampler { /* last: Option<(Instant, Duration)>, weighted_share: f64, weight: f64, covered: Duration, latest: Option<SessionCpuUsage> */ }
    impl CpuSampler {
        pub fn observe(&mut self, at: Instant, cpu_time: Duration, online_cpus: u32);
        pub fn latest(&self) -> Option<SessionCpuUsage>;
    }

For each observation after the first: interval `dt = at - last_at`, used `= cpu_time.saturating_sub(last_cpu)` (a decrease counts as zero), and `share = used / (dt × online_cpus)`, clamped to 0..=1. With `decay = exp(-dt / 3600 s)`, update `weighted_share = weighted_share × decay + share × dt` and `weight = weight × decay + dt`. The hourly share is `weighted_share / weight`. `covered = min(covered + dt, 3600 s)`. Ignore intervals shorter than one second, so repeated calls cannot divide by tiny numbers. The first observation only records the baseline, and `latest()` stays `None` until a second observation.

The wire type `SessionCpuUsage` lives in `mj-core` because the worker, the daemon and the TUI all use it. Create `mj-core/src/cpu_usage.rs` (declared in `mj-core/src/lib.rs`):

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SessionCpuUsage {
        /// Share of the machine's online logical CPUs used by the session's
        /// process tree over the newest sample interval, in tenths of a percent.
        pub recent_permille: u16,
        /// The same share averaged over about the last hour, in tenths of a percent.
        pub hourly_permille: u16,
        /// Seconds of samples behind `hourly_permille`, at most 3600.
        pub hourly_covered_secs: u32,
        pub online_cpus: u32,
    }

Integer tenths of a percent keep the type `Eq`, which the feed's `SnapshotMap` change detection needs, and give 0.1% resolution on large machines.

Add the formatting helper `format_cpu_permille(permille: u16) -> String` to `mj-client/src/usage_format.rs`, next to `format_clock`. Below 10.0% it shows one decimal (`4.2%`), otherwise a whole number (`42%`).

Tests for this milestone, colocated in `#[cfg(test)] mod tests` blocks:

- `cpu_sampler_reports_share_of_online_cpus`: two observations 10 s apart with 5 s of CPU on 4 CPUs give `recent_permille == 125`.
- `cpu_sampler_hourly_average_is_not_diluted_at_start`: a constant 50% share over five 10-second intervals gives `hourly_permille == 500` and `hourly_covered_secs == 50`.
- `cpu_sampler_treats_a_falling_counter_as_idle`: a smaller CPU time than before gives `recent_permille == 0`, and the next interval measures from the new value.
- On Linux, `parse_proc_stat_reads_fields_after_a_command_name_with_parentheses`, using a line whose command name is `(a) (b)`.
- `process_tree_cpu_time_counts_a_busy_child_after_it_is_reaped`, run on both Linux and macOS: read `process_tree_cpu_time(std::process::id())`. Spawn `sh -c 'end=$(($(date +%s)+2)); while [ $(date +%s) -lt $end ]; do :; done'` through the shared subprocess helper (see "Subprocess Rules" in `AGENTS.md`), wait for it, and read again. Assert that the increase is at least 1 s and at most 3 s. The upper bound catches a missed Mach time-base conversion on Apple Silicon, which would inflate the value about 41 times. Waiting for the child before the second read proves that waited-for children are counted. The test process's own threads add noise, so keep the bounds loose.

### Milestone 2: relay protocol 29 and the `CpuUsage` request

At the end of this milestone a daemon can ask a protocol-29 worker for its CPU figures on the existing connection, and nothing about it is journaled.

In `mj-core/src/relay.rs`, raise `RELAY_PROTOCOL_VERSION` from 28 to 29 and add `pub const RELAY_CPU_USAGE_PROTOCOL: u32 = 29;` with a one-line comment. Search for other places a protocol bump touches by running `git show 60145fdb -- mj-core/src/relay.rs mj-worker/src/relay.rs` (the bump to 28) and `rg -n "RELAY_PROTOCOL_VERSION|protocol_version: 28|\b28\b" mj-core/src/relay* mj-worker/src/relay*`, and follow the same pattern.

In `mj-core/src/relay/protocol.rs`, add `RelayRequest::CpuUsage` with a doc comment saying it is connection-only and never journaled. Its method name is `"cpu_usage"`, and `minimum_protocol` returns `RELAY_CPU_USAGE_PROTOCOL`. Add `RelayResponsePayload::CpuUsage { usage: Option<SessionCpuUsage> }`, where `None` means the worker has not completed its first 10-second interval yet. A sampler failure is returned as a `RelayResponseBody::Error` whose message is the sampler's error, with a `RelayErrorCode` that does not mean "retry the connection". Pick the closest existing code and document the choice in the Decision Log.

In the worker: start one background task when the worker begins serving, in `mj-worker/src/worker_runtime/unix.rs` where the connection loop receives shared handles such as `credentials` and `relay_root`. Every 10 seconds the task calls `process_tree_cpu_time(std::process::id())` and `online_cpus()` inside `tokio::task::spawn_blocking`, feeds the result to a `CpuSampler`, and publishes either `Ok(sampler.latest())` or `Err(message)` on a `tokio::sync::watch` channel. The task must not end silently. If it panics or returns, log at ERROR with the session ID, following how the worker already supervises its other background tasks. In the connection loop, add `RelayRequest::CpuUsage` as a connection-only request answered from the watch channel's current value. Add `RelayRequest::CpuUsage` to the connection-only list in `mj-worker/src/relay/requests.rs` that returns `InvalidState` if such a request reaches the durable relay.

In the controller, add to `mj-controller/src/worker_client/relay.rs`:

    pub async fn cpu_usage(&mut self) -> Result<Option<SessionCpuUsage>>

It returns `Ok(None)` without sending anything when `!RelayRequest::CpuUsage.supported_at(self.protocol_version)`, like `history_requests`. A protocol error from the worker becomes an `Err` carrying the worker's message.

Tests: in `mj-worker/src/relay/tests.rs`, add `CpuUsage` to the table near the existing `("credential-state", RelayRequest::CredentialState)` entry, so the durable relay rejects it. In `mj-worker/src/worker_runtime/relay_tests.rs`, add a test that a served worker answers `CpuUsage` and that the journal's latest ordinal is unchanged afterwards. Copy the structure of the existing credential-request test near line 191. In the controller's worker-client tests (`mj-controller/src/worker_client/tests.rs`), add a test that a connection negotiated at protocol 28 returns `Ok(None)` from `cpu_usage` and writes no request.

### Milestone 3: daemon sampling and publication

At the end of this milestone the daemon reads every live session's CPU figures about every 10 seconds and publishes them in the runtime feed, at most once per 10 seconds for all sessions together.

Add the feed type in `mj-client/src/runtime_feed.rs`:

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "state", rename_all = "snake_case")]
    pub enum SessionCpuView {
        Measured { usage: mj_core::cpu_usage::SessionCpuUsage },
        Unavailable { reason: String },
    }

Add `#[serde(default)] pub session_cpu: SnapshotMap<String, SessionCpuView>` to `RuntimeProjection` and `#[serde(default)] pub session_cpu: KeyChanges<SessionCpuView>` to `RuntimeDelta`, and include the new field in `between`, `is_empty` and `apply`. `serde(default)` keeps an older daemon's frames readable by a newer TUI. An older TUI ignores the unknown field, because these types do not use `deny_unknown_fields`. Check that this is still true before relying on it.

The shared table: create a `tokio::sync::watch::channel(SnapshotMap::<String, SessionCpuView>::new())` where the session manager's channels are created (`SessionManagerChannels` in `mj-controller/src/session_manager/channels.rs`). Give the sender to every session actor and the receiver to the daemon. The watch is the daemon's only copy of these facts. Actors only write their own key, and the feed only reads.

In the session actor (`mj-controller/src/session_manager/actor.rs`), keep a `next_cpu_read: tokio::time::Instant`. After a successful `sync_actor_connection` on an `Event::Tick`, if the actor is not leased, a connection is present and `now >= next_cpu_read`, call `connection.client.cpu_usage()` (expose a small method on `StandaloneSession` in `standalone.rs` if the client is private), then set `next_cpu_read = now + 10 s`. Map `Ok(Some(usage))` to `Measured`, `Ok(None)` to removing the key, and a worker-reported error to `Unavailable { reason }`. A transport error must take the same path as a failed sync (drop the connection, count a failure), because the next sync on that connection would fail the same way. Write the key with `send_if_modified`, so an unchanged value wakes nobody. Remove the session's key on every exit path of the actor loop with a small guard value whose `Drop` removes it. This covers retirement, `break`, and panic.

Publication: start one daemon task, next to where other daemon background tasks are started, that every 10 seconds checks `receiver.has_changed()`, marks the value seen, and calls `publish_revision()`. Each tick does a bounded amount of work, and the task owns no work a daemon handoff must wait for, so it takes no admission label. When the daemon stops, the task ends with it. Document that in a comment.

Capture: in `mj-controller/src/daemon/feed.rs`, where the full `RuntimeProjection` is built from `owner.sessions.clone()` (around line 256), also set `session_cpu` from `receiver.borrow().clone()`. In `RuntimeHistory::live_projection`, set `live.session_cpu` to `full.session_cpu` filtered to keys present in `live.records`.

Tests: in `mj-client/src/runtime_feed.rs` tests, a delta between projections that differ only in `session_cpu` is not empty and, when applied, reproduces the target. A projection serialized without the `session_cpu` field deserializes with an empty map. In the session-manager tests, an actor whose fake worker reports a usage value writes it to the table, and the key disappears after the actor is retired. Use the existing hand-written fake worker setup in that module's tests, not a mocking framework.

### Milestone 4: TUI display

At the end of this milestone a person sees the figures.

Plumbing: add `session_cpu` to `RuntimeStateUpdate` in `mj-controller/src/pollers/runtime_feed.rs` and carry it the same way `native_agents` is carried. In `mj-cli/src/dashboard/drains.rs`, pass it to a new `DashboardState::set_session_cpu(SnapshotMap<String, SessionCpuView>)` in `mj-tui/src/ingest.rs`, which stores it in a new `session_cpu` field on `DashboardState` and marks the sessions list and any open report dialog for redraw. Use the existing change-tracking in `mj-tui/src/render_changes.rs` rather than redrawing everything.

Row: give `session_activity_line` in `mj-tui/src/render/sessions.rs` one more argument, `cpu: Option<u16>` (the `recent_permille` of a `Measured` view, or `None`). Update its callers. When the line is not compact, the value is at least 10 (1.0%), and the formatted text plus one space fits after the status and queue, insert it as a muted span directly after the queue count and before the identity. The identity then shrinks by that width. Unavailable or missing values show nothing in the row. The dialog explains them.

Dialog: add `CommandId::SessionCpuReport` in `mj-tui/src/actions.rs`, labelled "CPU by session", with the description "Show live sessions grouped by machine, with each session's CPU share over about the last hour, highest first." Add `("CPU by session…", CommandId::SessionCpuReport)` as the last entry of the Targets list in `begin_support_pane_menu` (`mj-tui/src/pane_controls.rs`), and list the command in `mj-tui/src/help.rs` with the other Targets commands. Its handler sets `Mode::SessionCpuReport(SessionCpuReportDialog::default())`. Build the dialog by copying `NoticeLogDialog`'s modal, rendering and event handling: a scrollable read-only body and one Close control (`DialogControl::SessionCpuReportClose`) that is both the default and the dismiss action.

The dialog body is computed at render time from `DashboardState`, so it updates while open. For each live session, find its machine label: the `target.host` of the `capacity_details` entry whose `target.target_ids` contains the session's `target_template_id`. If none matches, use `session_target_label`. Search for an existing helper that maps a template ID to a host before writing a new one. Group sessions by machine. Sort sessions in each group by `hourly_permille`, highest first, and sort groups by the sum of their sessions' hourly figures, highest first. Show a header line per machine with that sum. Each session line shows the session title, its profile, the hourly figure, the recent figure, and, when `hourly_covered_secs < 3600`, a muted note such as `(14m)` showing how much time the average covers. Sessions whose view is `Unavailable` are listed after the measured ones in their group with the reason in muted text. Sessions with no entry are listed as "no CPU data yet". That covers both workers still on protocol 28 and workers in their first 10 seconds.

Tests in `mj-tui/src/render/tests.rs` (behavior, not layout snapshots): a row with `recent_permille = 230` shows `23%`, and one with `5` shows no percentage. The dialog lists two machines in descending order of their summed hourly figures and the sessions in each in descending order. An `Unavailable` session shows its reason. Choosing "CPU by session…" from the Targets menu opens the dialog. Drive this through the menu's command dispatch, not by constructing the mode directly.

### Milestone 5: end-to-end check and commit

Run the full checks, then exercise the feature in an isolated instance (see Concrete Steps). Commit with a message that states the design decisions above, including that the hourly average lives in the worker so it survives daemon restarts. Update this plan's `Progress`, `Surprises & Discoveries` and `Outcomes & Retrospective`.


## Concrete Steps

Run all commands from the repository root, `/home/jonathan/Projects/mjolnir`. Run `cargo test` outside any restricted sandbox, because the suite uses loopback TCP and Unix sockets. Use the dev profile (no `--release`), which keeps `debug_assert!` and overflow checks. Do not redirect Cargo's target directory. Builds go through `mbx` caching as configured.

Focused tests while working on a milestone:

    cargo test -p mj-worker cpu_
    cargo test -p mj-core relay::protocol
    cargo test -p mj-client runtime_feed
    cargo test -p mj-controller worker_client
    cargo test -p mj-tui render

Before committing:

    cargo test
    cargo clippy --all-targets -- -D warnings

Expected result: all tests pass and clippy prints no warnings.

End-to-end check in an isolated instance. Never point a test build at the default instance. Binaries built by Cargo refuse to touch it, and that refusal must not be worked around.

    cargo build -p mj-cli
    target/debug/mj --instance cpu-test

(If `mbx` places build output elsewhere, use the `mj` binary path that `cargo build` reports.)

In that TUI, start a session on a local target. In the session's user shell, run a CPU-bound command, for example `timeout 120 sh -c 'while :; do :; done'`. Within about 20 seconds the session's row shows a percentage close to 100 ÷ (the machine's logical CPU count), for example `1.0%` on a 96-CPU machine or `6.3%` on a 16-CPU machine. Open the Targets menu, choose "CPU by session…", and check that the session is first in its machine's group, with an hourly figure that rises over successive refreshes and a coverage note such as `(1m)`. After the command ends, the row's percentage disappears within about 20 seconds. Stop the instance's daemon when done (`target/debug/mj --instance cpu-test daemon stop`).


## Validation and Acceptance

The feature is accepted when all of the following hold:

1. In an isolated instance, a session running a CPU-bound loop shows a row percentage within 20 seconds that matches one busy core's share of the machine to within a factor of 1.5, and the figure disappears within 20 seconds of the loop ending.
2. "CPU by session…" appears in the Targets pane menu, opens a dialog listing live sessions grouped by machine, sorted by the hourly figure in descending order, and updates while open.
3. A session whose worker is still on relay protocol 28 shows no row figure and appears in the dialog as "no CPU data yet". No error is logged for it.
4. While the instance runs, `ps` on the host shows no `podman`, `docker` or `ssh` processes started for CPU sampling.
5. `cargo test` and `cargo clippy --all-targets -- -D warnings` pass on the dev profile. The new test `process_tree_cpu_time_counts_a_busy_child_after_it_is_reaped` passes on Linux and in macOS CI.


## Idempotence and Recovery

Every step is additive and can be repeated. There is no database migration and no change to stored data, so a rollback is a plain revert of the commits. The protocol bump to 29 makes running workers appear older than the daemon. They keep serving and are replaced by the existing automatic worker upgrade once idle. Until then they report no CPU data, which is expected. Keep the `cpu-test` instance separate from the default instance. If the instance gets into a bad state, stop its daemon and delete only that instance's data directory.


## Artifacts and Notes

Evidence gathered while writing this plan:

    $ cat /proc/pressure/cpu        # not used by this plan; checked while choosing the metric
    some avg10=14.68 avg60=8.35 avg300=9.63 total=11135073539

    $ ls /proc/self/task/*/children   # WSL2
    ls: cannot access '/proc/self/task/1693897/children': No such file or directory

    $ ssh morannon 'ls -d /proc/[0-9]* | wc -l; nproc'
    2742
    96

The removed poller's commit message (`git show 5a7ec3f3`) explains why the earlier podman-based approach was too expensive. It is worth reading before changing the sampling design.


## Interfaces and Dependencies

No new crate dependencies. `libc` is already a dependency of `mj-worker`, `mj-core` and `mj-controller`. Do not use the `sysinfo` crate for per-session figures: its per-process data leaves out the CPU time of children that have exited, so it would undercount sessions that run many short build processes.

At the end of the work these must exist:

In `mj-core/src/cpu_usage.rs`: `pub struct SessionCpuUsage { pub recent_permille: u16, pub hourly_permille: u16, pub hourly_covered_secs: u32, pub online_cpus: u32 }`.

In `mj-core/src/relay.rs`: `RELAY_PROTOCOL_VERSION = 29` and `RELAY_CPU_USAGE_PROTOCOL = 29`.

In `mj-core/src/relay/protocol.rs`: `RelayRequest::CpuUsage` and `RelayResponsePayload::CpuUsage { usage: Option<SessionCpuUsage> }`.

In `mj-worker/src/cpu_usage.rs`: `pub fn process_tree_cpu_time(root: u32) -> anyhow::Result<Duration>`, `pub fn online_cpus() -> anyhow::Result<u32>`, and `pub struct CpuSampler` with `observe` and `latest`.

In `mj-controller/src/worker_client/relay.rs`: `impl RelayClient { pub async fn cpu_usage(&mut self) -> Result<Option<SessionCpuUsage>> }`.

In `mj-client/src/runtime_feed.rs`: `pub enum SessionCpuView`, `RuntimeProjection::session_cpu` and `RuntimeDelta::session_cpu`.

In `mj-client/src/usage_format.rs`: `pub fn format_cpu_permille(permille: u16) -> String`.

In `mj-tui`: `CommandId::SessionCpuReport`, `Mode::SessionCpuReport(SessionCpuReportDialog)`, `DialogControl::SessionCpuReportClose`, and `DashboardState::set_session_cpu`.
