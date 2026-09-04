# Stop Active Sessions Without Waiting for the Current Turn

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan is maintained in accordance with `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

Stopping a session while its agent is working currently waits as long as 30 seconds for the agent to reach a checkpoint barrier before Mjolnir interrupts it. The dashboard therefore appears stuck on `Stopping` even when exporting the recovery archive itself takes well under a second. After this change, choosing Stop on an active session presents a dialog titled `Stop active session?`; confirmation interrupts the active worker immediately, lets its journal recover to an idle state, creates and verifies a fresh recovery archive, and destroys the target. Stopping an idle session continues directly to the same fresh recovery workflow without restarting its worker.

## Progress

- [x] (2026-09-04 15:20Z) Traced the slow stop to the fixed 30-second checkpoint-barrier wait and verified that the existing timeout recovery restarts the worker before successfully taking a fresh archive.
- [x] (2026-09-04 15:20Z) Chose immediate worker preemption for an active close instead of reusing `force_stop`, because `force_stop` discards work newer than the latest installed archive and refuses sessions that do not yet have one.
- [x] (2026-09-04 15:30Z) Added a close-time barrier policy that turns the first `Running` observation into an immediate worker restart while ordinary background checkpoints still defer.
- [x] (2026-09-04 15:32Z) Added active/idle Stop confirmation wording to the terminal and browser, including the exact active title and interruption warning.
- [x] (2026-09-04 15:35Z) Added focused controller, terminal-dialog, and embedded-browser behavior tests.
- [x] (2026-09-04 15:41Z) Added and passed the isolated scheduled active-stop process-chaos scenario; a 60-second prompt stopped in 6.37 seconds and resumed from its fresh archive.
- [x] (2026-09-04 15:42Z) Passed formatting, Python and shell syntax checks, the focused suites, the full workspace suite, and clippy with warnings denied.
- [x] (2026-09-04 15:44Z) Reviewed the final diff, updated this living plan with validation evidence, and prepared only the ten plan-owned files for the required commit.

## Surprises & Discoveries

- Observation: Archive creation was not responsible for the observed `Stopping 18s` delay. The log entered `wait_for_checkpoint_barrier` and had not yet reached the 30-second restart fallback; a completed retry spent about 161 ms on the worker archive and about 27 ms on transfer/checksum work.
  Evidence: The stop log reported `ACP did not admit the checkpoint barrier; restarting the worker and retrying` only after the fixed `CHECKPOINT_BARRIER_TIMEOUT`, while the subsequent capture timing was subsecond.

- Observation: The existing `Controller::force_stop` is not an acceptable implementation for ordinary Stop. It deliberately takes no fresh checkpoint, requires an existing verified recovery archive, and resumes from that older archive.
  Evidence: `mj-controller/src/hel_controller/lifecycle.rs` calls `verify_installed_checkpoint_gate` on `session.checkpoint` and its tests assert that a missing checkpoint leaves the running target untouched.

- Observation: The first active-stop chaos attempt proved a 4.82-second stop and produced a fresh archive, but its intended controller-route diagnostic was absent because the reliability fixture's `RUST_LOG` still named the repository's old `hel_cli` crate target and omitted `mj_controller`.
  Evidence: `trace.json` recorded `elapsed_seconds: 4.821385388961062` and the preserved runtime held a new `.hel.zip`; the controller log contained only `hel::hel_targets` records. The fixture filter is now `hel=debug,mj=debug,mj_controller=debug`.

## Decision Log

- Decision: Replace the close path's wait-on-active behavior with immediate use of its already-tested worker restart recovery, but retain the short checkpoint latch after the restarted worker is idle.
  Rationale: The user-visible barrier wait is the source of the latency. The latch itself is still needed to establish an exact recovery cut and normally completes immediately once the worker is idle. Reusing the timeout recovery preserves current filesystem and native-session state, unlike force-stop rollback.
  Date/Author: 2026-09-04 / Codex

- Decision: Show `Stop active session?` only when the latest dashboard projection says the agent is running; retain `Stop session?` for an idle session.
  Rationale: The destructive extra fact is interruption of an in-flight turn. The controller remains authoritative and will preempt a turn that begins during the confirmation race, so correctness does not depend on the projection being perfectly current.
  Date/Author: 2026-09-04 / Codex

- Decision: Add a scheduled active-stop process-chaos scenario rather than treating the pure barrier-policy unit test as sufficient lifecycle coverage.
  Rationale: The regression involved the interaction among a live ACP process, relay restart, archive publication, target teardown, and resume. The deterministic fake ACP can hold a prompt longer than the old 30-second timeout, making a sub-15-second stop and successful resume direct evidence that the new route works end to end.
  Date/Author: 2026-09-04 / Codex

## Outcomes & Retrospective

Active Stop now interrupts the running worker on the first barrier-state observation instead of waiting 30 seconds, then reuses the established journal recovery and verified fresh-archive workflow. Both terminal and browser surfaces name the interruption before submitting it. The new scheduled chaos scenario demonstrated a 6.37-second stop against a deliberately 60-second turn, successful resume, exact-once interrupted prompt recovery, valid SQLite state, and zero leaked processes. Ordinary periodic checkpoints retain their non-preemptive defer behavior.

## Context and Orientation

Mjolnir's terminal dashboard lives in `mj-tui`, its browser viewer is embedded from `mj-controller/src/web/viewer.js`, and lifecycle orchestration runs through the daemon in `mj-cli/src/daemon.rs`. The daemon calls `Controller::close_session_managed_controlled` in `mj-controller/src/hel_controller/lifecycle.rs`. That method asks `checkpoint_session_latched` in `mj-controller/src/hel_controller/checkpoint.rs` for an exact recovery cut, then closes the relay and destroys the target.

A checkpoint barrier is a relay command that waits until the agent protocol runtime is idle and prevents another agent turn from mutating the workspace while its recovery state is captured. Ordinary periodic checkpoints use `LatchExclusivity::ReleaseAfterLatch`; they defer when the agent is working. Close uses `LatchExclusivity::HoldThroughClose`. Before this change it waited up to `CHECKPOINT_BARRIER_TIMEOUT`, currently 30 seconds, before deciding an active turn was wedged. Its fallback calls `restart_worker_for_checkpoint`, which stops the worker process, installs the current worker binary, starts it again, replays its durable journal, waits until the recovered protocol session is idle, and retries the checkpoint barrier.

The terminal receives live execution state as `SessionDetail.current_turn_started_at` in `mj-tui/src/ingest.rs`. The browser receives the corresponding `ViewerSession.chat_phase` value and already derives a `running` boolean when rendering session controls. Both currently show a generic confirmation before the `close` action.

## Plan of Work

First, extend the close checkpoint call in `mj-controller/src/hel_controller/checkpoint.rs` with a narrowly scoped policy that means an active relay may be interrupted. After submitting the close checkpoint barrier, inspect the first synchronized relay snapshot. If close supplied the policy and execution is `Running`, return the same typed restart condition used by a barrier timeout. The existing caller then invokes `restart_worker_for_checkpoint`, replaces the leased connection with the recovered idle connection, and retries. Do not alter the policy for ordinary periodic checkpoints. Keep requesting a checkpoint latch after restart because it proves the fresh archive and relay projection describe the same durable point.

Second, update `mj-tui/src/dialogs.rs` and `mj-tui/src/actions.rs` so `Confirmation::Close` carries whether the selected session currently has a turn in flight. Render the exact title `Stop active session?` for that case and say confirmation interrupts the current turn before saving a fresh recovery copy. Keep the idle title and recovery explanation. Extend colocated dialog tests to prove both variants and their resulting `DashboardAction::Close` behavior.

Third, update `mj-controller/src/web/viewer.js` to locate the current session before confirming a close and choose the active-session wording when `chat_phase` is `running`. The request remains the existing `close` controller action; preemption is a close policy enforced by the controller, not a second unsafe endpoint.

Fourth, add an `active-stop` case to `tests/e2e/reliability_lab.py` and expose it through `tests/e2e/run-reliability.sh`. Its fake ACP holds a prompt for 60 seconds, while the scenario requires Stop to settle in less than 15 seconds, checks the controller's typed preemption diagnostic, resumes the stopped session, and proves the interrupted prompt exists exactly once. Schedule that scenario in `.github/workflows/reliability.yml` beside the existing process-chaos jobs.

Finally, format and validate the affected crates, run the active-stop chaos scenario, then run the repository-required full tests and clippy outside the restricted sandbox. Record exact outcomes here and commit the plan and implementation on the current branch.

## Concrete Steps

All commands run from `/home/jonathan/Projects/hel2`.

Inspect and edit the close/checkpoint path and the two confirmation surfaces. Then run:

    cargo fmt --all -- --check
    cargo test -p brokk-mj-controller -p brokk-mj-tui
    cargo test
    cargo clippy --all-targets -- -D warnings
    tests/e2e/run-reliability.sh --scenario active-stop --seed 700004 ./target/debug/mj

The test commands require elevated execution because the suite opens loopback TCP and Unix sockets. Expected results are exit status zero, all tests passing, and no clippy warnings.

Before committing, inspect:

    git status --short
    git diff --check
    git diff -- .agents/plans/stop-active-session-without-barrier-wait.md .github/workflows/reliability.yml mj-controller/src/hel_controller/checkpoint.rs mj-controller/src/hel_server.rs mj-controller/src/web/viewer.js mj-tui/src/actions.rs mj-tui/src/dialogs.rs tests/e2e/prepare-luna-lab.py tests/e2e/reliability_lab.py tests/e2e/run-reliability.sh

Then stage only files changed by this plan and commit them on the current branch.

## Validation and Acceptance

The controller behavior test must start from a running relay snapshot, invoke the close-specific checkpoint policy, and prove the restart path is selected without waiting for `CHECKPOINT_BARRIER_TIMEOUT`. Existing checkpoint tests must continue proving that ordinary periodic checkpoints defer rather than interrupt an active turn and that a close on an idle worker obtains its latch without a restart.

The terminal test must render or inspect an active close confirmation and observe the exact title `Stop active session?`, an interruption warning, Cancel and Stop buttons, and `DashboardAction::Close` after confirmation. An idle session must still say `Stop session?`.

The browser asset must remain valid JavaScript and its close confirmation must choose `Stop active session?` for `chat_phase === 'running'`. The full Rust test suite and clippy command must pass.

The isolated active-stop chaos scenario must observe `chat_phase: running`, issue Stop, reach `stopped` in less than 15 seconds despite a fake 60-second prompt, find the active-turn interruption marker in controller logs, resume successfully from the new archive, and find the interrupted prompt exactly once in the restored transcript. It must then stop every owned process, report no leaks, and pass SQLite integrity checks.

For a manual end-to-end check, start a long agent turn, choose Stop, confirm `Stop active session?`, and observe that the current turn ends promptly, the operation advances through worker restart and recovery archive stages instead of sitting at `Stopping` for 30 seconds, and the session appears in Resume. Resuming it should restore workspace changes captured after the interruption.

## Idempotence and Recovery

The source edits and validation commands are safe to repeat. Close remains fail-safe: a restart or checkpoint failure leaves the target and prior verified archive recorded rather than deleting them. If validation is interrupted, rerun the focused test first and then the full commands. Do not delete archives or targets to recover a failed test or close; terminate the owning process group before removing any process-owned files.

## Artifacts and Notes

The motivating timeline was:

    ACP did not admit the checkpoint barrier; restarting the worker and retrying
    worker archive/capture total: approximately 161 ms
    transfer/checksum: approximately 27 ms

The implementation should remove the first 30-second wait while retaining the fast fresh archive that follows it.

Final validation evidence:

    cargo test
    test result: ok (all workspace test binaries; 736 core tests with 4 ignored, the full controller and TUI suites, and all remaining suites passed)

    cargo clippy --all-targets -- -D warnings
    Finished `dev` profile ... exit status 0

    tests/e2e/run-reliability.sh --scenario active-stop --seed 700004 ./target/debug/mj
    reliability: passed scenario=active-stop seed=700004 leaks=0
    active Stop elapsed_seconds: 6.366729085915722

## Interfaces and Dependencies

No new crate or external dependency is needed. `Controller::close_session_managed_controlled` remains the daemon-facing interface, and browser/terminal actions remain the existing `Close` request. The internal checkpoint interface gains an explicit close-time preemption policy rather than inferring behavior from timeout or changing ordinary checkpoint semantics. `Controller::restart_worker_for_checkpoint` remains the sole implementation of worker replacement and recovery.

Revision note (2026-09-04 15:20Z): Created the plan after tracing the delay and rejecting stale-archive force stop as the normal Stop implementation.

Revision note (2026-09-04 15:32Z): Updated the internal implementation detail to match the simpler completed design: the first barrier-state poll returns the existing restart marker immediately for a confirmed active close, so the tested restart loop remains the single recovery mechanism.

Revision note (2026-09-04 15:42Z): Extended the plan at the user's request to include a scheduled, isolated active-stop process-chaos scenario with timing, log-route, resume, transcript, database, and process-leak invariants, then recorded its successful validation.

Revision note (2026-09-04 15:44Z): Completed the final diff review and recorded the full validation outcome before committing the implementation and plan together.
