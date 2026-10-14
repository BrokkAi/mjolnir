# Keep sub-agent wakeups independent of daemon handoff

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation proceeds.

## Purpose / Big Picture


A child waking from its parked state must not hold a Mjolnir upgrade while its worker recovers a journal, opens its harness, or executes an automatic provider retry. Once launch is accepted, the worker and pending input must survive daemon replacement. A readiness timeout must not stop accepted agent work.

## Progress


- [x] (2026-10-14) Confirmed the live failure: unpark held lifecycle admission through a 300-second idle wait, then stopped a worker whose provider retry was running.
- [x] (2026-10-14) Inspected worker ownership, durable restart intents, parked-session exclusion, and the existing daemon admission release mechanism used by Move.
- [x] (2026-10-14) Implemented atomic durable wake admission, short launch ownership, native readiness without idle waits, and bounded cancellation without worker cleanup.
- [x] (2026-10-14) Added passing isolated regressions for readiness during accepted work, handoff during slow native startup, next-actor recovery, and atomic publication rollback. Extended the shared admission ownership regression for Unpark.
- [x] (2026-10-14) Ran affected unpark, shared admission, worker restart, parked-child error, and queued-input handoff tests successfully.
- [ ] Merge current upstream, run final touched-crate suites and workspace clippy, and push master.

## Surprises & Discoveries


Normal session recovery excludes `Parked`; dropping a lifecycle hold while leaving the session parked cannot recover the wake. `worker_restart_intents` already protects live workers that are still starting across daemon replacement. The API already waits for native readiness after unpark, outside the unpark lifecycle. Refusal of wake admission during draining must retain pending input; the gate marks that refusal with the existing Refusal code mechanism, and input delivery waits with the same command ID. No database schema change is needed if the existing intent and the transition to `Running` are committed together. Recovery-copy reservation used the generic daemon `blocking` helper, which silently added a database-operation admission hold around a potentially long wait; Unpark now reserves on a direct blocking task without that hold.

## Decision Log


- Decision: Reuse the existing worker restart intent and atomically change the parked child to Running before launch.
  Rationale: The next daemon can attach or recover through its ordinary actor, without a second wake state machine or a new stored enum value.
  Date/Author: 2026-10-14 / Codex.
- Decision: Release unpark admission after detached launch; retain supervised native-readiness observation without admission.
  Rationale: Startup observation is allowed to take minutes and never justifies killing a live worker. Launch remains protected by worker ownership and daemon admission. Keeping the container startup slot until native readiness preserves the existing limit on simultaneous harness starts; handoff cancels only observation.
  Date/Author: 2026-10-14 / Codex.
- Decision: Generalize Move's resumable-work admission hooks for worker preparation, preserving labels at each reacquisition site.
  Rationale: Preparation is safely repeatable before durable wake acceptance; cancellation must release its owner without changing a parked child.
  Date/Author: 2026-10-14 / Codex.

## Outcomes & Retrospective


The two live-store/relay unpark regressions pass. Shared admission, worker restart compatibility, parked-child error reporting, and queued-input handoff deferral tests pass. A validated checkpoint is ready; upstream merge, final validation and push remain. The user authorized committing and pushing the current branch; it is master. Existing Cargo is the mbx wrapper, and target is its existing symlink under `/mnt/optane/mbx-targets`; leave both unchanged.

## Context and Orientation


The daemon is the process that manages session records and launches workers. A worker is a separate process that owns the agent turn and its durable relay journal. Admission is a counted hold preventing the daemon from exiting during an operation that cannot yet restart from stored state.

`mj-controller/src/daemon/lifecycle.rs` admits each session operation. `daemon/support.rs` implements the executor hooks that release admission during resumable work. `controller/subagent_park.rs` stops idle children and wakes parked ones. `database/worker_restart.rs` stores restart ownership and phase. `session_manager/actor.rs` already observes a ready worker and clears its intent; recovery preserves a live worker with an outstanding intent. `server_runtime/api.rs::unpark_child` waits for the actor's native session after unpark finishes. `controller/worker_restart.rs` currently applies an inappropriate idle wait to worker startup.

## Plan of Work


Generalize resumable admission hooks in `mj-core/src/targets.rs` and their controller implementations and call sites. Release the unpark lifecycle hold before recovery reservation and worker preparation. Acquire the short launch hold only after binary/configuration preparation and container start admission. In `database/worker_restart.rs`, share intent insertion and add an atomic parked-child wake transition that updates only lifecycle fields. Launch under the existing worker owner, leave the durable intent for the session actor, and finish readiness observation without daemon admission, idle waits, or stop-on-error cleanup. Preserve failed launch errors while leaving recoverable accepted wake state intact. Remove startup idle observation from common restart paths; checkpoint callers acquire their own checkpoint barrier.

Extend isolated module tests and, where needed, real binary upgrade coverage. Exercise partial failures and cancellation with controlled target commands; test real store transactions and relay boundaries rather than tiny computed examples.

## Concrete Steps


Work from the repository root. Run all Cargo tests with escalated permissions; use the existing mbx Cargo configuration. New subprocess tests must retain private MJ_CONFIG_DIR and MJ_DATA_DIR, and real CLI/daemon invocations must use `--instance durable-unpark-test` or a distinct test name.

    cargo fmt --all --check
    cargo test -p brokk-mj-controller <affected-filter>
    cargo test -p brokk-mj-core
    cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings

Run any CLI suite added by the implementation as well. Stage only changed files, commit on master with an ownership explanation in the body, and push origin/master. If upstream has moved, fetch and merge origin/master, resolve conflicts, and push the merge; never rebase or switch branches.

## Validation and Acceptance


A restarted worker may report a native-ready session with a running retry and still complete startup. Preparation cancellation leaves the child parked with no accepted wake intent. Once wake acceptance is committed, the durable record and intent remain mutually consistent even if launch fails or the daemon exits. Slow native startup does not hold lifecycle admission. The next actor attaches without stopping a live boot or submitting the parent's input twice. Required formatting, touched-crate tests and workspace clippy must pass before the final commit.

## Idempotence and Recovery


Worker ownership serializes wake against close, destroy and recovery. Atomic intent admission refuses another intent. After acceptance, a failed observation is retried by ordinary actor recovery; a live worker with an intent is preserved. Test stores are private and cleanup terminates owning processes before removing files. No live daemon, worker, store, harness profile or release installation is changed for validation.

## Artifacts and Notes


Live evidence: child `aebe025f996d728efed4e263546b4de8` restarted at 20:43:31 UTC, submitted `server-retry-4464` at 20:44:31, then failed unpark's idle wait at 20:48:34 with execution Running. Handoff closed admission immediately afterward.

## Interfaces and Dependencies


Use the existing `WorkerPermit`, `WorkerRestartIntent`, `start_worker_durably`, subprocess executor, session actor and database writer. Introduce no workspace crate, dependency, wire format or migration. Resumable executor hooks must retain operation identity checks and acquire named admission before a control transition.

Revision note: Added an explicit handoff refusal code and an input-delivery regression so declined wake admission cannot permanently fail accepted queued input. Implementation retains the existing container startup slot through native readiness; that supervised observation now holds no daemon admission and never stops a live worker. This preserves startup concurrency while making handoff independent of readiness.
