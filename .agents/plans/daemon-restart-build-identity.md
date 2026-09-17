# Make a daemon restart land on the build the caller asked for

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan is maintained in accordance with `.agents/PLANS.md` at the repository root. Read that file before changing this one.

This plan addresses GitHub issue BrokkAi/mjolnir#1039, "Daemon restart from a rebuilt binary can bring back the old build when an older client is attached".

## Purpose / Big Picture

Today, after you rebuild `mj` and run `mj daemon restart`, the daemon that comes back can be the previous build. The command still prints "Mjolnir daemon restarted as PID N" and exits zero, so nothing tells you that your new code is not running. On a development machine this silently wastes a debugging session; it also disables automatic worker recovery for every session, because a daemon started from a deleted executable cannot find the portable Linux worker any more.

After this change, three things are true. First, `mj daemon restart` either produces a daemon running the caller's own executable or fails with a message that names the executable that won instead, so "restarted" means what it says. Second, `mj daemon status` and `mj doctor` show whether the running daemon is the same build as the client you typed, and which worker binary the daemon froze at startup, so a mismatch that cannot be prevented is at least visible. Third, a long-lived client's background keep-alive tick no longer creates daemons behind your back; only a command you actually ran can start one.

You can see it working: build `mj`, start the TUI, rebuild `mj`, run `mj daemon restart` from the new binary, and then run `mj daemon status`. Before this change the status line is identical whichever build won. After it, restart either succeeds against the new build or tells you exactly which other executable took the daemon.

## Progress

- [x] (2026-09-17 23:35Z) Read issue #1039, the related issues #1060 and #1078, and the daemon startup, worker-pinning and worker-upgrade code paths.
- [x] (2026-09-17 23:47Z) Reproduced the failure live in a private instance `plan1039`: six consecutive `mj daemon restart` runs from the new build all reported success and all brought back the old, unlinked inode.
- [x] (2026-09-17 23:47Z) Reproduced the second variant: after `mj daemon stop`, the attached old client revived the daemon from its own deleted inode within six seconds.
- [x] (2026-09-17 23:55Z) Proved the key assumption behind the recommended fix: a client that predates any fix already blocks on `daemon-start.lock`, so holding that lock across stop-and-start wins deterministically without changing old clients.
- [x] (2026-09-17 23:52Z) Established what the daemon pins at startup for the worker binary and confirmed the pinned copy is the SHA-256 of the worker file as it stood when the daemon started.
- [x] (2026-09-18 00:20Z) Wrote this plan. No product code changed.
- [ ] Maintainer decides the open questions in `Decisions For The Maintainer`.
- [ ] Milestone 1: restart holds the startup lock across stop and start, and verifies the resulting daemon's executable identity.
- [ ] Milestone 2: build identity and pinned worker identity become visible in `mj daemon status` and `mj doctor`.
- [ ] Milestone 3: the attachment keep-alive stops creating daemons.
- [ ] Milestone 4 (only if the maintainer wants it): a sidecar intent file that records which build asked for the current daemon.

## Surprises & Discoveries

- Observation: the failure is not rare. With one old client attached, six of six restarts from the new build lost the race.

  Evidence (private instance `plan1039`, worktree `target/debug/mj`; old build inode 808807, new build inode 1208707942):

      run1 exit=0 out='Mjolnir daemon restarted as PID 2611442.' pid=2611442 inode=808807 deleted=1
      run2 exit=0 out='Mjolnir daemon restarted as PID 2612201.' pid=2612201 inode=808807 deleted=1
      run3 exit=0 out='Mjolnir daemon restarted as PID 2614738.' pid=2614738 inode=808807 deleted=1
      run4 exit=0 out='Mjolnir daemon restarted as PID 2617777.' pid=2617777 inode=808807 deleted=1
      run5 exit=0 out='Mjolnir daemon restarted as PID 2618396.' pid=2618396 inode=808807 deleted=1
      run6 exit=0 out='Mjolnir daemon restarted as PID 2619392.' pid=2619392 inode=808807 deleted=1

  The first restart of the session did win, so the race is real rather than a fixed ordering; once the old client has been deprived of its daemon it retries every two seconds and wins almost every time.

- Observation: `stat` on `/proc/<pid>/exe` without `-L` returns the procfs symlink's own inode, not the executable's. An implementation or a diagnostic that compares those numbers compares nothing. The product code is correct here (`executable_file_identity` in `mj-cli/src/daemon.rs` calls `fs::metadata`, which follows the link), but a plan reader debugging by hand will be misled.

  Evidence: for the same process, `stat -c %i /proc/2574343/exe` printed 33911992 while `stat -L -c %i /proc/2574343/exe` printed 808807, the real old-build inode.

- Observation: a client that predates any fix already waits on the daemon startup lock, for up to 38 seconds. That is what makes a lock-holding restart work against clients that cannot be changed.

  Evidence: with an external `flock` held on `~/.local/share/mjolnir/instances/plan1039/daemon-start.lock` for 25 seconds, the old client could not revive the stopped daemon at all, and started one the moment the lock was released:

      t=3s none
      ...
      t=24s none
      t=27s pid=2699708 inode=2660262

- Observation: the worker binary is frozen at daemon start into a content-addressed cache, so a worker rebuild does not reach a running daemon even when the daemon is the current build.

  Evidence: after the daemon started, `~/.local/share/mjolnir/instances/plan1039/workers/pinned/` contained exactly one directory, `5fbc7669171e2f9c78adbc23467b64e3e55b0c3e30b7173bf3b7bfd49dfed5f1/hel`, and `sha256sum target/debug/mj-worker` printed the same digest.

- Observation: nothing a user can run today distinguishes the two builds. `mj daemon status` prints `version 2.10.0` for both, and `daemon.json` carries the same `build_version`, because both are `env!("CARGO_PKG_VERSION")`.

## Decision Log

- Decision: treat the startup lock, not a new protocol message, as the mechanism that makes a restart deterministic.

  Rationale: the clients that cause the problem are, by definition, older than any fix. They cannot be taught a new rule. They already honour `daemon-start.lock`, proved live above, so a restart that holds that lock from before the stop until after the replacement answers `Ping` cannot be overtaken. Nothing else in the design reaches clients that predate the fix.

  Date/Author: 2026-09-18 / Opus (plan only)

- Decision: do not add any field to `daemon.json`.

  Rationale: `DaemonMetadata` in `mj-client/src/daemon.rs` is declared with `#[serde(deny_unknown_fields)]`. A new field makes `read_metadata_any` fail in every client built before the change. Those clients would then be unable to run `mj daemon status`, `mj daemon stop` or `mj daemon restart` against the daemon, and would conclude no daemon exists and try to start their own, which fails later on the controller store lock with a confusing message. The same conclusion was reached and recorded in `.agents/plans/auto-restart-stale-dev-daemon.md`. If build intent must be recorded on disk, it goes in a separate file that old clients never read.

  Date/Author: 2026-09-18 / Opus (plan only)

- Decision: do not add a field to `DaemonStatus` either.

  Rationale: `DaemonStatus` is part of the frozen management subset documented at `mj-client/src/daemon.rs` around the `DaemonAction` definition, and it also denies unknown fields. Status is how clients and daemons of any protocol version identify each other. The client can determine the daemon's executable identity by itself from the PID that status already returns, so no wire change is needed for the visibility work.

  Date/Author: 2026-09-18 / Opus (plan only)

- Decision: recommend against making the `MJ_DEV_RESTART_STALE_DAEMON` behaviour the default in development builds.

  Rationale: this repository is routinely checked out into many worktrees at once, each with its own `target/debug/mj`, all sharing one instance's daemon by default. A default-on staleness check means every command from worktree A stops the daemon worktree B just started, and back again, indefinitely. The check is correct as an explicit statement by `scripts/run.sh` that its freshly built binary is authoritative; it is wrong as a standing rule. The real gap the issue found is not that the check is off by default, but that `mj daemon restart` does not verify its own result.

  Date/Author: 2026-09-18 / Opus (plan only)

## Outcomes & Retrospective

Not started. Fill this in at the end of each milestone: what the reader can now do that they could not, which evidence proves it, and what remains.

## Context and Orientation

This section assumes no prior knowledge of the repository.

### The pieces

Mjolnir ships one main executable, `mj`, built from the crate `brokk-mjolnir` in `mj-cli/`. The same file plays two roles. Run with the `daemon-run` argument it becomes the **daemon**: a long-lived background process that owns the database, runs sessions and serves a local TCP endpoint. Run any other way it is a **client**: the terminal user interface (TUI), or a one-shot command such as `mj sessions` or `mj daemon status`.

A second executable, `mj-worker`, is the **worker**. One worker process runs per session, possibly on another machine or inside a container, and talks back to the daemon. It is built separately; `cargo build --bin mj` does not rebuild it.

The daemon publishes where it can be reached in a small JSON file, `daemon.json`, under the instance's data directory (`mj_core::config::data_dir()`; for an instance named `plan1039` on Linux that is `~/.local/share/mjolnir/instances/plan1039/`). Its shape is `DaemonMetadata` in `mj-client/src/daemon.rs`: protocol version, process id, address, token, start time and build version.

### How a daemon gets started

Every client that needs the daemon calls `connect_or_start` in `mj-cli/src/daemon.rs`. That function, in order:

1. Takes an exclusive file lock on `data_dir()/daemon-start.lock` (`acquire_start_guard`). Any other client doing the same waits here, for at most `STOP_TIMEOUT + START_TIMEOUT`, which is 30 + 8 = 38 seconds, then gives up with an error.
2. Reads `daemon.json` and replaces the daemon if its protocol version differs from this client's.
3. Optionally replaces a daemon that is running a different executable, but only when the environment variable `MJ_DEV_RESTART_STALE_DAEMON` is set, and only once per client process (`maybe_replace_stale_development_daemon`, guarded by a `OnceCell`). `scripts/run.sh` sets that variable.
4. Connects to the daemon and returns it if it answers `Ping`.
5. Otherwise spawns a new daemon and waits up to `START_TIMEOUT` (8 seconds) for it to answer.
6. Releases the lock when the returned guard is dropped, which happens as `connect_or_start` returns.

Step 5 spawns `daemon_launch_executable()`, which on Linux is the literal path `/proc/self/exe`. That is a kernel-provided reference to the exact file this client is running, even if that file has been deleted from disk. The comment in the code explains why: executing the running image guarantees the daemon speaks the same protocol as the client that started it.

The complete list of things that may start a daemon, all through `connect_or_start` and therefore all from the caller's own executable:

- any CLI subcommand that needs the daemon (`mj sessions`, `mj move`, `mj import`, the `mj api ...` family, `mj desktop`, and others; see the call sites in `mj-cli/src/main.rs`, `mj-cli/src/api_client.rs`, `mj-cli/src/desktop.rs`, `mj-cli/src/import.rs`);
- the TUI when it opens a workspace (`run_workspace_dashboard` in `mj-cli/src/main.rs`);
- the TUI's background keep-alive, `maintain_attachment` in `mj-cli/src/daemon.rs`, which calls `connect_or_start` **every two seconds** for as long as the TUI runs;
- the TUI's action, poller and draft paths (`mj-cli/src/dashboard/actions.rs`, `mj-cli/src/pollers.rs`, `mj-cli/src/dashboard/drafts.rs`, `mj-cli/src/dashboard/io/spawn.rs`);
- the desktop shell, which is the `mj desktop` subcommand wrapping the `mj-desktop` webview crate; it is a client like any other.

`mj daemon restart` (in `daemon_command`, `mj-cli/src/main.rs`) is three steps: connect to the daemon over the management subset, `stop_and_wait()`, then `connect_or_start()`. The lock is **not** held across the stop. It then prints the PID from `status()`.

### What goes wrong

While a client runs, Cargo can replace `target/debug/mj` with a new file. The old client keeps running the old file, which is now unlinked; Linux shows it as `/proc/<pid>/exe -> /path/to/mj (deleted)`.

Now run `mj daemon restart` from the new build. The stop succeeds. In the gap between the stop and the new client's `acquire_start_guard`, the old TUI's two-second tick calls `connect_or_start`, takes the lock first, and spawns a daemon from its own deleted image. The restarting client then blocks on the lock, and when it gets in it finds a healthy daemon at step 4 and returns it. Restart prints success. The daemon is the old build. This is confirmed by the live runs quoted in `Surprises & Discoveries`.

Two consequences follow, both confirmed:

1. Nothing reports the mismatch. `build_version` is `CARGO_PKG_VERSION` for both builds, so status, `daemon.json` and every log line are identical.

2. Portable worker resolution breaks entirely in the new daemon. At startup, `run_daemon_runtime` (`mj-controller/src/daemon/process.rs`) calls `pin_worker_binary_sources`, which resolves a worker for the host architecture and for portable Linux x86\_64 and aarch64 and copies each local one into a content-addressed cache under `data_dir()/workers/pinned/<sha256>/hel`. The resolver, `worker_binary_prerequisite_for_current` in `mj-controller/src/controller/worker_binary/binary_source.rs`, computes `controller_replaced = !is_file(current)` where `current` is `std::env::current_exe()`. For a daemon started from a deleted image that path ends in ` (deleted)` and is not a file, so `controller_replaced` is true, the sibling lookup is skipped, and the function returns "the running mj binary was replaced or removed on disk ...; restart the Mjolnir daemon so it runs the current build, then retry". That is exactly the message in the issue, and it is stored in the pinned snapshot, so every later worker upgrade for every session repeats it. Automatic worker recovery is off until the daemon really changes build. (Confirmed by code; the same text is asserted by the existing test at `mj-controller/src/controller/worker_binary/tests.rs:611`. Not re-observed live here, because this worktree had no musl worker built.)

Note which part of worker resolution is *not* broken by a deleted image: `select_native_worker` is consulted before the `controller_replaced` guard and only uses the executable's parent directory, which still exists. So a `local-bare` session still gets a native worker. Only the portable Linux worker, used by container, ssh and EC2 targets, is lost.

### The worker's own staleness, which is a separate problem in the same family

Even with a perfectly current daemon, the worker a new session runs is the worker as it stood when the daemon started. `WorkerBinarySourceSnapshot::capture` copies it into the immutable cache once; nothing re-reads `target/debug/mj-worker` afterwards. Confirmed live: the pinned directory's name equalled `sha256sum target/debug/mj-worker` at daemon start, and the daemon serves that copy thereafter.

The daemon does know about worker staleness internally. A connected worker reports a content digest of its own executable in its `hello` (`mj_core::worker_launch::running_executable_digest`, sent as `worker_build`), and `mj-controller/src/worker_upgrade.rs` compares it against the build the controller would install and replaces the worker in place when the session is quiet. But that comparison is invisible: no command prints either digest. A user or an agent who rebuilt the worker and started a session has no way to ask which worker is actually running, which is how the trap recorded in the maintainer's notes (an agent A/B-testing worker changes against a worker built hours earlier) happened.

### Related issues

- **#1060** — `mj daemon restart` sometimes reports a healthy restart as a failure, because `START_TIMEOUT` (8 seconds, `mj-cli/src/daemon.rs`) expires before the new daemon accepts a request. Another agent is changing that readiness wait in the same file right now. This plan must not introduce a second readiness loop. Milestone 1 performs its identity check **after** whatever readiness wait exists at merge time, using its result, and changes no timeout. If the readiness wait becomes process-aware or longer, the identity check simply runs later; the only coupling is the total time spent holding the startup lock, discussed in Milestone 1.
- **#1078** — an older CLI against a newer daemon. `ensure_supported_daemon_protocol` already produces a clear message for that direction. This plan makes the message more likely to be *correct* rather than more likely to appear, because it stops a stale client from silently installing a daemon of its own protocol under a newer client.

## Options Considered

Each option is judged on three things: what it guarantees, how it behaves against clients built before the fix, and what it does to an installed upgrade, where an old and a new `mj` legitimately coexist for a while.

**A. Hold the startup lock across stop and start, then verify identity (recommended).** `mj daemon restart` takes `daemon-start.lock` first, stops the daemon, starts the replacement and waits for it, all without releasing the lock, then checks that the new daemon's `/proc/<pid>/exe` is the caller's own file. Guarantees the caller's build wins whenever every other daemon starter honours the lock. Pre-fix clients honour it already, proved live, so this works today against the exact clients that cause the issue. Installed upgrades are unaffected: the lock is already on every path, and no message, file or timeout changes. The cost is that other clients can be blocked for the duration of a restart; the existing 38-second waiter deadline bounds that, and a waiter that does time out already retries.

**B. Record the intended build in `daemon.json` and have other clients defer to it.** Guarantees nothing against pre-fix clients, which do not read the field; worse, it actively breaks them, because `DaemonMetadata` denies unknown fields, so they can no longer read the file at all and lose `daemon status`, `daemon stop` and `daemon restart` against the daemon. A sidecar file avoids the breakage but still does nothing for pre-fix clients. For installed upgrades it is a genuine improvement in principle — the newest installed build wins — but it needs a rule for what "newer" means when two installations legitimately differ (two checkouts, a package and a local build), and that rule is what makes the development experience bad. Rejected as the primary mechanism; offered as optional Milestone 4.

**C. Older clients never auto-start a daemon when a newer one was recorded.** Same defect as B in a stronger form: it is a rule that only the clients which already have the fix can obey, and the offender never does. It also cannot distinguish "older" from "different" without a build ordering that does not exist. Rejected.

**D. The daemon exits only when a newer client asks, and only that client starts the replacement.** This needs a new daemon action, so it needs a `PROTOCOL_VERSION` bump, and old daemons reject unknown actions. It does not help in the reported scenario, because the daemon being replaced is a *new* build being displaced by an *old* client; the old client never asks. Option A achieves the same exclusivity with a file lock that already exists and that old clients already respect. Rejected.

**E. Make the mismatch loudly visible without preventing it (recommended, as a complement).** `mj daemon status` reports whether the daemon runs the same executable as the client, and `mj doctor` gains a check for both that and the pinned worker. Guarantees nothing about which build runs, but it converts a silent wrong answer into a visible one, which is the only thing that helps when the other party is a client that cannot be changed. It needs no protocol or file-format change, because the client can inspect `/proc/<pid>/exe` for the PID that status already returns. Cheap and independently useful.

**F. Stop the keep-alive tick from creating daemons (recommended, as a complement).** `maintain_attachment` calls `connect_or_start` every two seconds; that is the process that actually spawns the stale daemon in this incident. A two-second background timer is not a statement of user intent and should use `connect_existing`, reporting when no daemon answers rather than creating one. This removes the cause rather than winning a race against it — but only in clients that carry the fix, so it is the durable fix, not today's fix. For installed upgrades it is strictly good: an old attached TUI stops resurrecting an old daemon after the user upgrades.

**G. Launch the daemon from the current file on disk when the running image has been unlinked.** Tempting and wrong for installed upgrades. Today an old client spawns an old daemon it can definitely talk to. Under G, an old TUI attached across a package upgrade would spawn the *new* daemon and then refuse to speak to it ("the daemon uses a newer protocol"), every two seconds, forever. It trades a silent staleness for a loud permanent breakage of a running session. Rejected. (Note that F removes the same spawn without this side effect.)

**H. Default `MJ_DEV_RESTART_STALE_DAEMON` on in development builds.** Rejected; see the Decision Log. It would make every pair of worktrees fight over the shared daemon.

**I. Do nothing, or only part.** Doing nothing leaves a documented trap that costs a debugging session each time it is hit and silently disables worker recovery for container and remote targets. Doing only Milestone 2 (visibility) is a serious partial option: it is the smallest change, it cannot regress anything, and it turns the failure into a question the user can answer. If the maintainer wants only one milestone, take Milestone 1; if only one *safe* milestone, take Milestone 2.

## Recommended Design

Do A, E and F, in that order, as three separately testable milestones. A is the development-loop fix and lands first because it is the one that works against clients that predate it. E is the part that survives any client that cannot be fixed. F is the durable fix that removes the spawn entirely for clients that carry it, and is also the right behaviour for installed upgrades.

No protocol change. No `daemon.json` change. No database migration. `PROTOCOL_VERSION` stays where it is, unless the maintainer chooses Milestone 4 with a wire-based variant, which this plan does not recommend.

## Plan of Work

### Milestone 1 — restart lands on the caller's build, or says why not

Scope: `mj daemon restart` becomes exclusive and self-checking. At the end of this milestone, restarting from a rebuilt binary while an old client is attached either produces a daemon running the new build or fails with a message naming the executable that won.

The work is in `mj-cli/src/daemon.rs` and `mj-cli/src/main.rs`.

First, split the body of `connect_or_start` so that the part after the lock can be reused by a caller that already holds the lock. Introduce a private `async fn connect_or_start_holding(guard: &DaemonStartGuard) -> Result<DaemonClient>` containing everything `connect_or_start` does today after `acquire_start_guard`, and reduce `connect_or_start` to acquiring the guard and delegating. The guard parameter exists to make the requirement visible in the type; it is not otherwise used. This split is required, not cosmetic: the lock is not reentrant within one process. The existing test `cancelled_startup_wait_does_not_retain_the_lock` in the same file demonstrates that a second `acquire_start_guard` in the same process blocks, so a restart that already holds the guard must not call `connect_or_start` again.

Second, add `pub async fn restart_daemon() -> Result<RestartedDaemon>` to `mj-cli/src/daemon.rs`, where

    pub struct RestartedDaemon {
        pub pid: u32,
        /// Whether the new daemon runs this client's own executable.
        /// `None` on platforms where that cannot be determined.
        pub runs_this_build: Option<bool>,
    }

It acquires the startup guard once, then, still holding it: reads `daemon.json`; if a daemon is recorded, stops it with the existing `replace_daemon(&metadata)` helper, which already tries a graceful management stop and falls back to `SIGTERM` with the existing safety checks; then calls `connect_or_start_holding`; then asks the daemon for its status to learn the PID; then calls `daemon_uses_current_executable(pid)`.

Third, handle the case where the check says `Some(false)` — another client won despite the lock, which can happen if it had already started its daemon before the restart took the lock. Retry the stop-and-start sequence exactly once, still under the same guard. If the second attempt also reports `Some(false)`, return an error that names both executables, for example:

    Mjolnir daemon 2611442 is running /path/to/mj (deleted), not this build
    (/path/to/mj). Another attached client started it. Close clients from the
    previous build, then retry `mj daemon restart`.

Resolve the daemon's path for that message with `std::fs::read_link("/proc/<pid>/exe")` on Linux and `sysinfo`'s process `exe()` elsewhere, mirroring the two existing `daemon_uses_current_executable` implementations. On a platform where identity cannot be determined, `runs_this_build` is `None` and the command reports success without the guarantee.

Bound the time under the lock. One stop plus one start is already bounded by `STOP_TIMEOUT` and the readiness wait; a single retry doubles that and can exceed the 38-second deadline that waiting clients use. Accept that: a waiting client fails one attempt with the existing "timed out waiting for another client to finish starting the Mjolnir daemon" message and retries on its next tick. Do not raise the waiter deadline, and do not add a second readiness loop — reuse whatever `connect_or_start_holding` does, including whatever #1060 changes it to.

Fourth, rewrite `DaemonCommand::Restart` in `mj-cli/src/main.rs` to call `daemon::restart_daemon()` and print one of:

    Mjolnir daemon restarted as PID 2663145, running this build.
    Mjolnir daemon restarted as PID 2663145.        (identity not checkable)

and to propagate the error otherwise.

Tests for this milestone belong beside the code in `mj-cli/src/daemon.rs`, in the existing `#[cfg(test)] mod tests`. Two are worth writing, and neither duplicates the implementation:

- A behaviour test that a second `acquire_start_guard` in the same process blocks while the first is held and succeeds after it is dropped — this already exists as `cancelled_startup_wait_does_not_retain_the_lock`; extend or reference it so the non-reentrancy the new structure depends on stays proved.
- A test that the identity verdict drives the outcome: factor the decision into a small pure function, for example `fn restart_verdict(pid: u32, identity: Option<bool>, daemon_path: Option<&Path>) -> Result<RestartedDaemon>`, and test that `Some(true)` yields success, `None` yields success without the guarantee, and `Some(false)` yields an error whose text contains both paths. Do not mock the filesystem or spawn daemons for this.

Do not write a test that merely asserts the sequence of calls.

### Milestone 2 — the mismatch becomes visible

Scope: after this milestone, a user or an agent can ask which build the daemon is and which worker it will hand to a new session, and get an answer, without any protocol change.

In `mj-cli/src/main.rs`, `DaemonCommand::Status` already prints the PID. Add one line after the existing status line, computed entirely in the client from that PID using `daemon_uses_current_executable`:

    This daemon runs this build.

or

    This daemon runs a different executable (/path/to/mj (deleted)) than this
    client (/path/to/mj). Commands still work; rebuilt code is not running.

or nothing at all when identity cannot be determined. `daemon_uses_current_executable` is currently private to `mj-cli/src/daemon.rs` and compiled only for Linux and macOS; make it visible to `main.rs` and give it a stub returning `Ok(None)` elsewhere.

For the worker, add a check to `mj-controller/src/doctor.rs` alongside `worker_binary_checks`. It runs in the client process (`run_current` is called directly by `mj doctor`), so it must not assume it is the daemon. The check should report, for each architecture the config needs: the worker source that would be resolved *now* by `worker_binary_prerequisite_for_arch`, its SHA-256, and — if a daemon is running — the digests present in `data_dir()/workers/pinned/`, which are exactly the digests that daemon froze. When the current source's digest is absent from the pinned set, report it as a warning with the remediation "restart the Mjolnir daemon so new sessions use the rebuilt worker". This is a filesystem comparison only; it adds no wire traffic and no daemon-side code.

Also state the daemon's build identity in the same `mj doctor` run, so one command answers both questions.

A behaviour test for the doctor check belongs in `mj-controller/src/doctor.rs` next to the existing worker checks: build a temporary pinned directory and a temporary worker file and assert the check is `Ready` when the digest matches and a warning when it does not.

### Milestone 3 — the keep-alive stops creating daemons

Scope: after this milestone, a running TUI never creates a daemon from its own image behind the user's back. The user-visible change is that if the daemon is stopped while a TUI is open, the TUI reports that the daemon is gone instead of silently starting one.

In `mj-cli/src/daemon.rs`, change `maintain_attachment` to call `connect_existing()` rather than `connect_or_start()`. `connect_existing` re-reads `daemon.json` on every call, so a TUI still follows a daemon that was legitimately replaced by someone else — that behaviour must be preserved and is the reason this change is safe. When `connect_existing` fails, keep the existing `tracing::warn!` and, in addition, surface a notice in the dashboard's existing notice channel so the user learns the daemon is gone rather than finding a frozen view. Identify the right channel by following how `mj-cli/src/dashboard/actions.rs` reports background failures today; do not invent a second mechanism.

Decide deliberately what the other short-lived TUI paths do. `mj-cli/src/pollers.rs`, `mj-cli/src/dashboard/actions.rs`, `mj-cli/src/dashboard/drafts.rs` and `mj-cli/src/dashboard/io/spawn.rs` act on a user action, so they may keep `connect_or_start`. Only the unattended timer changes.

The test is a behaviour test of the keep-alive: with no daemon metadata present, one tick must report a failure and must not spawn a process. Structure `maintain_attachment` so the connection function is a parameter or a small trait object that a hand-written test fake can supply, rather than introducing a mocking framework.

### Milestone 4 — optional, only on the maintainer's instruction

If the maintainer wants a recorded intent despite the analysis above, write it to a **new** file, `data_dir()/daemon-build.json`, never to `daemon.json`. Contents: the SHA-256 of the executable that asked for the current daemon, and its path and start time. A client that has the fix reads it before starting a daemon; if it names a different build and that build's process is still alive, the client defers instead of starting its own. Pre-fix clients ignore the file entirely, so nothing they do changes and nothing they do is prevented. Format compatibility: additive, new file, no reader in any existing build; the file must be treated as advisory, and every code path must work when it is missing or unparseable.

## Concrete Steps

All commands run from the repository root of your worktree.

Build and validate:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings

Run every `cargo test` outside the restricted sandbox with elevated permissions; the suite uses loopback TCP and Unix sockets and fails or hangs under the sandbox. Use the dev profile, which is where `debug_assert!` and overflow checks run.

Be aware that a known flaky test (#1036, `codex_usage`, "Text file busy") can fail when other agents build in parallel. Rerun that test alone before treating a failure as yours.

## Validation and Acceptance

### The live test, which fails on the unfixed build

This is the reproduction from `Surprises & Discoveries`, written out so a novice can run it. It uses a private instance named `plan1039` and TCP port 4139 so it cannot touch the default instance. Never stop or restart a daemon belonging to any other instance.

Set up the instance once:

    mkdir -p ~/.config/mjolnir/instances/plan1039
    cp ~/.config/mjolnir/instances/campaign0916/config.toml \
       ~/.config/mjolnir/instances/plan1039/config.toml

Edit that copy: set `bind` under `[phone]` to `127.0.0.1:4139`, and append a local target if you need one:

    [targets.localhost]
    kind = "local-bare"

The copied file contains an API key. Never print, paste or commit it.

Then:

1. Build the first version and note its inode:

       cargo build --bin mj
       stat -c '%i %n' target/debug/mj

2. Start the TUI from that build inside tmux and leave it running:

       tmux new-session -d -s plan1039 -x 140 -y 40
       tmux send-keys -t plan1039 "$PWD/target/debug/mj -i plan1039" Enter

   Wait for the dashboard to draw (`tmux capture-pane -p -t plan1039`).

3. Rebuild, which replaces the file and leaves the TUI holding the old, now unlinked inode:

       touch mj-cli/src/main.rs && cargo build --bin mj
       stat -c '%i %n' target/debug/mj          # a different inode

4. Restart from the new build six times, printing which executable the daemon actually runs. Note the `-L`: without it you get the procfs link's inode, not the executable's.

       for i in $(seq 1 6); do
         ./target/debug/mj -i plan1039 daemon restart
         sleep 1
         for p in $(pgrep -f daemon-run); do
           inst=$(tr '\0' '\n' < /proc/$p/environ | grep '^MJ_INSTANCE=')
           [ "$inst" = "MJ_INSTANCE=plan1039" ] &&
             echo "pid=$p inode=$(stat -L -c %i /proc/$p/exe) link=$(readlink /proc/$p/exe)"
         done
       done

   **Before the change**, every line reports success and the old inode with a ` (deleted)` link — this is the observed failure. **After Milestone 1**, every line reports the new inode with no ` (deleted)` suffix, or the command fails with the message naming the other executable. Either is a pass; a silent success on the old inode is a fail.

5. The second variant, which is deterministic. Stop the daemon from the new build and wait:

       ./target/debug/mj -i plan1039 daemon stop
       sleep 6
       # then repeat the pgrep loop above

   **Before the change**, a daemon is back within a few seconds, on the old inode. **After Milestone 3**, no daemon comes back on its own, and the TUI reports that the daemon is gone.

6. Worker visibility, after Milestone 2:

       cargo build --bin mj-worker
       ./target/debug/mj -i plan1039 daemon restart
       ls ~/.local/share/mjolnir/instances/plan1039/workers/pinned/
       sha256sum target/debug/mj-worker
       ./target/debug/mj -i plan1039 doctor

   The pinned directory name equals the worker's digest, and `mj doctor` reports the worker as current. Now rebuild the worker so its content changes, rerun `mj doctor`, and it must report the pinned worker as out of date with the remediation to restart the daemon.

Clean up completely, in this order — deleting files is not a substitute for stopping processes:

    pkill -f "mj -i plan1039"          # clients first, so none revives the daemon
    ./target/debug/mj -i plan1039 daemon stop
    # confirm nothing survives: a daemon shows only as `/proc/self/exe daemon-run`,
    # so identify it by MJ_INSTANCE in /proc/<pid>/environ
    for p in $(pgrep -f daemon-run); do
      tr '\0' '\n' < /proc/$p/environ | grep -q '^MJ_INSTANCE=plan1039$' && echo "survivor $p"
    done
    tmux kill-session -t plan1039
    rm -rf ~/.config/mjolnir/instances/plan1039 ~/.local/share/mjolnir/instances/plan1039

Do not run any other `mj -i plan1039` command after the stop: it starts the daemon again.

A trap worth stating explicitly: `cargo build --bin mj` and `cargo test` do **not** rebuild `mj-worker`. Any live test that claims something about worker behaviour must run a full `cargo build` first and prove the running worker is the current one, for example by comparing `sha256sum target/debug/mj-worker` with the pinned directory name.

### Automated acceptance

`cargo test` passes, including the new tests named in each milestone, and each new test fails before its milestone's change and passes after. `cargo clippy --all-targets -- -D warnings` is clean.

## Idempotence and Recovery

Every step is repeatable. The instance setup overwrites its own config. The live test can be run any number of times; each run starts from `cargo build` and ends with the cleanup block. If a daemon is left behind, the cleanup block finds it by `MJ_INSTANCE` and stops it. If the startup lock is ever left held by a dead process, the kernel releases it when that process exits; no manual repair is needed. Nothing in this plan writes to the default instance, and no database migration is involved, so there is nothing to roll back.

## Interfaces and Dependencies

In `mj-cli/src/daemon.rs`, at the end of this work:

    pub struct RestartedDaemon {
        pub pid: u32,
        pub runs_this_build: Option<bool>,
    }

    pub async fn restart_daemon() -> anyhow::Result<RestartedDaemon>;

    async fn connect_or_start_holding(guard: &DaemonStartGuard)
        -> anyhow::Result<DaemonClient>;

    // widened from private to crate-visible, with an `Ok(None)` stub off Linux and macOS
    pub(crate) fn daemon_uses_current_executable(pid: u32) -> anyhow::Result<Option<bool>>;

`pub async fn connect_or_start() -> Result<DaemonClient>` keeps its signature and meaning.

In `mj-controller/src/doctor.rs`, one new check function returning `DoctorCheck` values in the existing style, reusing `mj_controller::controller::worker_binary_prerequisite_for_arch` and `mj_core::config::data_dir`.

Dependencies already in the workspace and used as-is: `sysinfo` for non-Linux process inspection, `sha2` for digests (already used by `copy_worker_source_to_cache`), `libc` for the existing signal path. No new dependency.

Unchanged and deliberately so: `PROTOCOL_VERSION` in `mj-client/src/daemon.rs`; `DaemonMetadata`; `DaemonStatus`; `DaemonAction`; `daemon.json`; every timeout constant.

## Decisions For The Maintainer

1. **Should `mj daemon restart` fail, or warn and succeed, when another client's build won twice?** Recommendation: fail, with the message naming both executables. A restart that reports success while running other code is the whole bug, and the issue asks for exactly this. The cost is that a script which restarts the daemon while an old TUI is open now gets a non-zero exit; that is the correct signal.

2. **Milestone 3 changes what a TUI does when its daemon disappears: it reports instead of restarting.** Is that the behaviour you want? Recommendation: yes. Creating a daemon is a user intent, and a two-second timer cannot express one. If you would rather keep auto-recovery, the compromise is to let the tick restart the daemon only when the client's own executable still exists on disk, which blocks precisely the stale case.

3. **Do you want Milestone 4 (the `daemon-build.json` sidecar) at all?** Recommendation: no, for now. It helps only clients that already have the fix, and those clients are better served by Milestone 3, which removes the spawn rather than arbitrating it.

4. **Confirm the rejection of default-on `MJ_DEV_RESTART_STALE_DAEMON`.** Recommendation: keep it opt-in, set by `scripts/run.sh` only. Many worktrees share one instance here, and a standing rule makes them fight. If you disagree, the safer variant is to scope the check to "the daemon's executable no longer exists on disk", which is a fact rather than a preference and cannot make two live checkouts fight.

5. **How much should `mj doctor` say about the worker?** Recommendation: the digest of the worker it would resolve now, the digests the running daemon pinned, and a warning when they differ. This is the check that would have prevented the stale-worker A/B testing trap, and it costs nothing at run time.

## Artifacts and Notes

The failure, captured on 2026-09-17 against commit e3e1d1fc with an old client attached (old build inode 808807, new build inode 1208707942):

    run1 exit=0 out='Mjolnir daemon restarted as PID 2611442.' pid=2611442 inode=808807 deleted=1
    ...
    run6 exit=0 out='Mjolnir daemon restarted as PID 2619392.' pid=2619392 inode=808807 deleted=1

The same instance's status at that moment, showing nothing wrong:

    Mjolnir daemon 2619392 (version 2.10.0) started 2026-09-17T23:48:36Z; 1 attached client; web viewer http://127.0.0.1:4139/

and the matching `daemon.json`, with the token removed:

    {
     "protocol_version": 24,
     "pid": 2619392,
     "address": "127.0.0.1:32781",
     "token": "<redacted>",
     "started_at": "2026-09-17T23:48:36Z",
     "build_version": "2.10.0"
    }

The lock experiment that justifies Milestone 1. An external `flock` held `daemon-start.lock` for 25 seconds while the daemon was stopped; the old client could not start one until the lock was released:

    t=3s none
    t=6s none
    ...
    t=24s none
    t=27s pid=2699708 inode=2660262

The worker pin, showing the content-addressed freeze:

    $ sha256sum target/debug/mj-worker
    5fbc7669171e2f9c78adbc23467b64e3e55b0c3e30b7173bf3b7bfd49dfed5f1  target/debug/mj-worker
    $ ls ~/.local/share/mjolnir/instances/plan1039/workers/pinned/
    5fbc7669171e2f9c78adbc23467b64e3e55b0c3e30b7173bf3b7bfd49dfed5f1

## Revision Note

2026-09-18, Opus: first version of this plan, written from issue #1039 plus a live reproduction in a private instance. No product code was changed. The plan's central claim — that holding the existing startup lock across stop and start is enough to beat clients that predate any fix — rests on the `flock` experiment recorded above; if that experiment cannot be reproduced on another platform, Milestone 1 loses its guarantee there and falls back to detection only, which is Milestone 2.
