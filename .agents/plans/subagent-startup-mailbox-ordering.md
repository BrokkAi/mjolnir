# Seed the worker FIFO before accepting messages

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Parents can send follow-up instructions immediately after spawn returns. The
worker must already have its configuration and original brief in its durable
first-in, first-out command queue (FIFO), so no mailbox wake can start a turn
between those inputs. Issue #1394 occurred because the daemon fed that queue
one startup step at a time and waited between steps.

## Progress

- [x] (2026-10-29) Confirm both reported failures from the live store and transcripts.
- [x] (2026-10-29) Demonstrate the defect with the real daemon and worker in instance issue-1394-red.
- [x] (2026-10-29) User selects worker FIFO initialization; remove the earlier receipt gate.
- [x] (2026-10-29) Persist startup with registration and seed ordinary worker commands before opening its socket.
- [x] (2026-10-29) Add regressions for queue ordering, interrupted initialization and required configuration failure.
- [x] (2026-10-29) Focused worker FIFO, parked receipt and legacy registration regressions pass.
- [x] (2026-10-29) Complete the real delegation scenario on the final receipt-only monitor build.
- [x] (2026-10-29) Merge origin/master and pass workspace clippy, formatting and checkpoint tests.
- [x] (2026-10-29) Diagnose a credential-lock test's fixed-delay assumption and wait for observable acquisition instead.
- [x] (2026-10-29) Full core and worker suites, worker process tests and final workspace clippy pass.
- [x] (2026-10-29) Corrected controller lock regression passes; all affected library suites are validated.
- [x] (2026-10-29) Controller documentation tests pass; final local validation is complete.
- [x] (2026-10-29) Commit and push through 02fb709cd; issue #1394 closes and its in-progress label is removed.
- [x] (2026-10-29) Image publication run 40916209742 succeeds for arm64, amd64 and the combined manifest.

## Surprises & Discoveries

Both reported children ran from an addendum without their original brief. Their
model step succeeded, but the effort and brief steps failed after a mailbox wake
started. The baseline real-daemon regression reproduced early mailbox admission.

Registration previously persisted the child before its startup input was saved.
That made worker-local initialization insufficient by itself: recovery could find
an incomplete child. The child record and full sequence now commit together.
Existing worker command receipts and the durable native_session_used fact support
idempotent initialization without a new readiness flag or mailbox disposition.

## Decision Log

The user explicitly selected: seed the worker FIFO with configuration and the
first brief before accepting messages. The earlier proposal to defer mailbox
admission on the initial prompt receipt was removed. All execution and completion
state remains in the existing worker journal and FIFO.

Keep the launch description's initial input immutable. Before exposing its
control endpoint, a fresh worker submits the entire sequence under stable IDs.
If initialization crashed before the final brief was admitted, replay completes
the missing suffix under those same IDs. The final brief receipt proves all
preceding input was journaled. Once native work ran, receipt compaction must not
cause initialization again. Required initialization failures stop the worker
before the brief or mailbox work can run; optional fast mode may be refused.

Reviewer children retain their separate plan-mode setup and explicit review
startup path. This fix initializes ordinary spawned worker children, the surface
reported in #1394. No new store schema or relay event format is introduced.

## Outcomes & Retrospective

The worker FIFO and controller receipt/registration regressions pass. The final
real delegation scenario passes startup ordering, once-only mail delivery,
daemon replacement, child parking and wake, and report delivery. Its daemon
handoff took 0.262 seconds. Receipt-only monitoring also passes the real-journal
regression for a first turn that finishes and parks before startup bookkeeping.
The checkpoint suite passes (164 tests); all but one of 2,200 controller tests
passed on the first full run. That separate credential-lock test assumed its
first acquisition completed within 150ms under suite load; it now waits for
the observable lock before checking legacy-lock exclusion. Core passes 553
library tests and its auxiliary tests; worker passes 722 library tests and all
binary/process tests. The corrected controller lock regression passes, as do
workspace clippy, formatting, Python syntax checks and controller documentation
tests. The fix is pushed to origin/master through 02fb709cd and issue #1394 is
closed. Image publication run 40916209742 succeeds for both architectures and
the combined manifest. No live instance has been upgraded.

## Context and Orientation

The daemon is the controller process; workers own agent turns and their durable
command queues. `mj-controller/src/controller/subagents.rs` builds registration
and startup input. `mj-controller/src/database/sessions.rs` saves both in one
SQLite transaction, using the existing startup insertion code in
`mj-controller/src/database/startup.rs`. Startup steps retain their existing JSON
format and command IDs.

`mj-controller/src/controller/worker_binary/launch.rs` copies initial commands into
WorkerLaunchConfig, whose wire type is in `mj-core/src/worker_launch.rs`.
`mj-worker/src/worker_runtime/unix.rs` passes that input to DurableRelay before
publishing the control socket. `mj-worker/src/relay.rs` journals the prefix;
`mj-worker/src/relay/commands.rs` prevents a required initialization failure from
releasing later queue entries. Normal mailbox delivery remains unchanged.

The daemon still monitors startup receipts so API status, failure reporting and
parent notifications survive daemon restart. Its startup drain reads the
worker's durable receipts without submitting initial commands again.
HTTP and worker-tool spawn paths no longer independently submit the same brief.

## Plan of Work

Complete the focused worker FIFO tests and adapt the recorded ACP delegation
scenario. Pause the harness's model response, enqueue a real parent message,
and verify the original brief is already queued with no prompt running. Release
configuration and verify the first turn contains the full brief and requested
effort; the follow-up reaches that turn once, either as hidden prompt context or
at a tool hook. Existing delegation scenarios cover daemon replacement and
parked-child messaging.

Review the resulting diff, validate the final state, and commit only these
changes on the current branch. Push HEAD to origin/master; if rejected, fetch
and merge origin/master, resolve conflicts and validate affected changes.

## Concrete Steps

Use the normal mbx-managed Cargo storage. Do not change target/. All cargo test
commands run outside the sandbox because tests use loopback and Unix sockets.

Run cargo test -p brokk-mj-worker initial_fifo -- --nocapture for the focused
worker regressions. Build brokk-mjolnir and brokk-mj-worker, and copy binaries to
/mnt/optane/tmp/issue-1394/bin-fixed before a live scenario, since mbx may evict
build output. Run tests/e2e/delegation.py with those --mj and --worker paths and
--instance issue-1394-green. Set MJ_RELIABILITY_ARTIFACTS outside target/ and use
an env -i launcher with separate fixture homes and data.

After the last implementation round, run cargo test for brokk-mj-core,
brokk-mj-worker, brokk-mj-controller and brokk-mj-checkpoint. Run workspace
cargo clippy --all-targets -- -D warnings and cargo fmt --all -- --check. Check
Python syntax without generating repository artifacts. Rerun only failing cases
when fixes are needed; perform the full final validation once.

## Validation and Acceptance

The real baseline scenario failed because a mailbox event reached the child
before its original brief. The fixed scenario must show that all startup input
is ahead of any mailbox wake and the first turn has both its brief and selected
effort. The worker regression must preserve accepted ordinals across a restart
that completes a partially initialized prefix. A refused required selector must
leave the brief unstarted across worker restart; checkpoint recovery must still
be able to preserve its data. Optional fast mode refusal must not prevent the
brief. Existing isolated upgrade, queue and startup recovery tests must pass.

## Idempotence and Recovery

Initial command IDs are unchanged across daemon or worker restart. The worker
journal owns their admission and completion. Startup input stays in the launch
file, so partial initialization is recoverable without daemon liveness. Existing
logical session state prevents replay after receipt compaction. No live daemon,
store, credentials or harness cache is used by tests.

## Artifacts and Notes

Evidence is under /mnt/optane/tmp/issue-1394/. Baseline build and reproduction
logs are baseline-build.log and red-e2e.log. The two original witnesses are
ba513d39f66bd8fcb15754e62fc9d6f6 and 5527f0e8b2763cb2dd039a1bbda21510, whose mailbox
wakes started at 02:36:42 and 14:59:59 UTC respectively.

## Interfaces and Dependencies

WorkerLaunchConfig adds an optional initial_commands vector of
InitialWorkerCommand { command_id, command, optional }. Commands use the existing
RelayCommand wire shape. No dependencies or crates are added. StartupDelivery
and StartupStep retain their shipped serialized formats.

Revision 2026-10-29: replace the receipt gate with the user-selected worker FIFO.

Validation update 2026-10-29: worker FIFO tests (2), parked first-turn receipt
replay, and legacy registration recovery pass. The late subtle-child report
failure in green-e2e-3.log showed why initial input must have only one producer;
the daemon now observes receipts without resubmission.

Final real scenario: final-e2e.log, instance issue-1394-final, artifacts under
final-artifacts/delegation-seed-1171-2871009. Full-suite output is final-tests.log;
the remainder and rerun use core-worker-tests.log and legacy-lock-regression.log.

Remote publication: https://github.com/BrokkAi/mjolnir/actions/runs/40916209742
completed successfully for commit 02fb709cdd5dba2471e810d37d4936a791f8bde6.
