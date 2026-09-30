# Diagnose daemon stalls and failed suspension recovery

This ExecPlan follows `.agents/PLANS.md` and is maintained as work proceeds.

## Purpose / Big Picture

Issues #1199 and #1112 describe failures without enough evidence to identify
their cause safely. A daemon is the process that manages sessions; a worker is
the process that keeps each agent running. The next daemon stall must report
whether the async runtime and the serving loop are progressing and which
synchronous target operations are still running. A failed suspension must
record whether its worker exists and where startup or shutdown stopped, while
retaining the target and checkpoint for recovery.

## Progress

- [x] (2026-09-30) Claimed both issues, read current recovery code and the retained #1199 log.
- [x] Confirmed relay SSH leases and capacity probes already run on blocking threads; the historical cause is unproved.
- [x] (2026-09-30) Added scoped synchronous-operation diagnostics and a default 10-second SSH handshake timeout, retaining explicit user settings.
- [x] (2026-09-30) Added an independent daemon progress monitor with bounded cleanup, separate runtime/serving-loop observations and stall/recovery reports.
- [x] (2026-09-30) Added failure-time worker probes to suspension recovery without mutating retained data or replacing the original error.
- [x] (2026-09-30) Focused operation-registry, runtime/serving-loop, real SSH handshake and isolated missing-worker suspension regressions passed.
- [x] (2026-09-30 15:23Z) `cargo clippy --all-targets -- -D warnings` passed in the dev profile.
- [x] (2026-09-30 15:24Z) Final Rust formatting and whitespace checks passed.
- [x] (2026-09-30 15:31Z) Full dev-profile `cargo test` rerun passed, including all 2,066 controller tests, the real SSH handshake regression and documentation tests. Validated the complete change for its required commit on hel2.

## Surprises & Discoveries

The retained daemon log ends at 22:18:35 and resumes at 22:38:45 with SSH master
opens. Current `mj-controller/src/worker_client/connect.rs` and
`mj-controller/src/pollers/capacity.rs` already use `spawn_blocking` and a
60-second executor budget for these opens. There is no stack from the failed
process, so this timing cannot prove SSH caused the stall.

`Controller::recover_interrupted_close_managed` in
`mj-controller/src/controller/lifecycle.rs` must query the worker before deciding
whether to take a fresh checkpoint or finish a sealed close. A missing socket
does not establish whether the worker died, is starting, or holds a sealed
checkpoint. Historical #1112 workers were removed after their work was saved;
they cannot now be examined. Existing code intentionally retains `Closing`
state and target on an ambiguous failure.

A real loopback TCP server that withholds its SSH banner demonstrates that
OpenSSH's handshake timeout applies before authentication. Testing the entire
admission path initially hit its deliberate transport retries instead: banner
timeouts are classified as unaccepted requests and retried with backoff. The
regression now exercises one master-open attempt and verifies that the user's
one-second ConnectTimeout beats the executor's three-second deadline.

The first full workspace run passed 2,065 controller tests but failed the new
blocked-runtime regression at `Option::unwrap`: another test held the operation
registry mutex when the reporter sampled it. Returning `None` is intentional so
the reporter never blocks on that mutex. The regression now waits for a later
nonblocking snapshot that names its guard. It also keeps the runtime running
while awaiting recovery, rather than relying on a short sleep being observed
by the OS thread under concurrent load.

## Decision Log

- Decision: Instrument ambiguous failures instead of guessing a new destructive recovery transition.
  Rationale: A timeout cannot establish that a worker is safe to replace, and an old checkpoint may omit newer work.
  Date/Author: 2026-09-30, Codex.
- Decision: Report stalls from a plain OS thread, with separate runtime and serving-loop progress.
  Rationale: An async-only watchdog cannot report when the runtime itself is stalled. Separating the two identifies a stuck serving loop even when other async tasks run.
  Date/Author: 2026-09-30, Codex.
- Decision: Disable serving-loop observation during shutdown while keeping the runtime heartbeat.
  Rationale: Draining accepted operations after serving ends is expected and must not produce a false serving-loop stall report.
  Date/Author: 2026-09-30, Codex.

## Outcomes & Retrospective

Implementation and full validation are complete. Neither historical cause is
claimed to be fixed. A recurrence now has
independent progress observations and synchronous-operation identities for
#1199, and connection-phase/worker-state evidence for #1112. SSH connection
establishment also gains a default handshake deadline.

The monitor warns after ten seconds without runtime or serving-loop progress,
repeats at most every thirty seconds, and reports recovery. During shutdown it
observes runtime progress alone. Suspension probes run on the blocking pool
with a five-second executor deadline; probe failures retain the original
suspension error. No live session was changed to investigate either incident,
and both issues remain open until recurrence evidence establishes their cause.

## Context and Orientation

`mj-core/src/targets.rs` owns synchronous subprocess execution and
`mj-core/src/targets/ssh.rs` owns SSH admission and shared-connection leases.
These can wait before starting a child. Diagnostics must register operations
before those waits and remove them on every exit, without recording argument
vectors, environment values or stdin. `mj-controller/src/daemon/process.rs`
runs the async serving loop. The monitor must use monotonic time and atomics,
avoid the daemon's state mutex, and stop when its owning scope ends.
`mj-controller/src/controller/worker_binary/process.rs` already provides
`probe_worker`, a read-only probe of startup/exit records and process IDs.
Reuse it for suspension failure rather than adding another interpretation of
worker liveness.

## Plan of Work

First add a scoped registry in the existing target module for active blocking
operations, including their safe descriptions, originating thread and elapsed
time. Register executor calls and SSH lease preparation so the monitor sees
admission waits as well as child execution. Add a default SSH ConnectTimeout to
master establishment while respecting explicit user options.

Then add `mj-controller/src/daemon/diagnostics.rs`. Its async heartbeat records
runtime progress; its serving-loop tick records loop progress. An OS thread
reports a stall after a short grace period and periodically while it persists,
with active synchronous operations. It reports recovery once progress returns.
Dropping the monitor stops its thread and aborts its heartbeat without waiting
on a stuck runtime. Tests must prove the thread reports while a single-thread
runtime is blocked and stops promptly on drop.

Finally wrap each connection phase in interrupted-close recovery with useful
context and, on failure, run the existing worker probe on the blocking pool
with a short deadline. Log session state, checkpoint frontier, failed phase,
worker processes, startup step and exit reason, or the diagnostic probe error.
Keep the original error and all retained session data. Tests must drive a
missing worker in an isolated store and demonstrate that diagnostics do not
stop or remove it or replace its original failure.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir2`, use the existing mbx Cargo shim without
changing build storage:

    cargo test -p brokk-mj-core diagnostics
    cargo test -p brokk-mj-controller diagnostics
    cargo test -p brokk-mj-controller suspension_intent_survives_restart
    cargo test -p brokk-mj-core a_master_open_times_out_during_handshake_and_honors_the_users_shorter_budget
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

Run every test command outside the restricted sandbox. Tests that access the
store use the repository's isolated-test subprocess helper and their own
MJ_DATA_DIR. Never run a new build against the default instance. Existing
unrelated edits in move_session.rs and daemon/session_move.rs remain outside
this change and its commit.

## Validation and Acceptance

A deliberately blocked test runtime must produce a stall report from the OS
thread before the runtime is released, naming the active synchronous operation.
Advancing runtime progress without loop progress must still identify a stuck
serving loop. Resuming progress produces one recovery notice, and stopping the
monitor produces no later reports. Failed suspension preserves its target and
checkpoint and records a read-only worker probe result with the connection
phase; an unavailable probe never hides the original failure. Required Cargo
tests and clippy pass in the dev profile.

## Idempotence and Recovery

Diagnostics introduce no database migration, protocol change or live-session
mutation. Probe commands are bounded and read-only. Monitor ownership is
scoped and cleanup is bounded. Do not stop, resume, destroy or migrate any
production session to reproduce these reports.

## Artifacts and Notes

The historical log inspected is
`~/.local/share/mjolnir/logs/mj-daemon-20260929T215103.456Z-2216742.log`.
The older #1112 daemon log no longer exists here. Record final commands and
results here after validation.

Final checks passed:

    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

The final full-test log is `/tmp/mj-1199-1112-cargo-test-final.log` and the
Clippy log is `/tmp/mj-1199-1112-clippy.log`. These temporary logs are local
validation evidence, not repository artifacts. To identify recurrence evidence,
search daemon logs for `daemon progress stalled` or
`suspension failure worker diagnostic`, then correlate the session ID and
connection phase with the original lifecycle failure reference.

## Interfaces and Dependencies

Use existing std synchronization, Tokio, tracing and target executor helpers.
No new crate or dependency is needed. Active-operation snapshots are diagnostic
facts only and must never be used to decide session activity or restart safety.
The existing worker probe remains the single reader of worker startup, exit
and process state.

Revision note: Initial plan records the evidence gap and the safe diagnostic
scope explicitly authorized by the user.

Revision note: Implementation and focused validation establish the diagnostic
behavior. Record the handshake-retry finding and avoid false serving-loop
warnings during deliberate shutdown; full validation remains in progress.

Revision note: The full-suite contention failure exposed a test assumption,
not a production stall. Preserve the monitor's nonblocking registry read and
make the regression tolerate contention and observe recovery asynchronously.

Revision note: Final full tests, Clippy and formatting passed. Record the
observable report cadence, diagnostic deadline and remaining evidence gap;
retain both issues as unresolved while committing the validated diagnostics.
