# Suspend settles a Move it supersedes, and failed lifecycle work says why

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root. Part of BrokkAi/mjolnir#1542 (cases 2 and 4).


## Purpose / Big Picture

On 2026-11-15 session f01b2eefc22385245286892277497036 was being moved to the EC2 Podman host s50-max-a. A Suspend was accepted while the Move was still cloning the destination checkout, before any worker was installed there. The Move was cancelled ("close superseded the operation"), and Suspend recovery then tried to lease and sync a worker that never existed (`mj-controller/src/controller/lifecycle.rs` ~625-648). Each failure only replaced the session's error text (`record_failed_close`, ~844), so the session stayed in `Closing` indefinitely while the relay actor kept retrying its hello with backoff. Separately, every failed lifecycle operation tells the user only "the daemon log records the reason under reference …" (built at `daemon/state.rs` ~693), so the user cannot see what went wrong or what to do.

After this change, a Suspend or Close that supersedes a Move ends in a terminal state the user can act on. That is Stopped when the session can be restored from a sealed source checkpoint, or a visible Error that Resume's repair can take. A session never stays in `Closing`. Every lifecycle failure the user sees states a cause, the next action, and the diagnostic reference.


## Progress


- [x] (2026-11-15) Initial implementation and review fixes committed at 59b3ff06f and 4c3c932a6; prior checkpoint validation completed (2,660 passing cases, 27 ignored; workspace clippy clean).
- [x] (2026-11-15) Final rereview found cancellation alone cannot grant Suspend authority. Parent approved an explicit nullable SQL intent with compatible revision 133 and unchanged floor 132 after writer audit.
- [x] (2026-11-15) Revision-132 writers audited: explicit column lists only; retry invalidation trigger covers reused operation IDs. Isolated migration rollback/old-writer preservation test passed (0.47 s).
- [x] (2026-11-15) Intent, readiness and Closing publication implemented atomically. Startup selects exclusive Move/close owner from one Move/intent snapshot; terminal outcomes consume intent; committed Destroying retains it.
- [x] (2026-11-15) Parent approved reusing normal Move rollback for cancelled, unresolved, sealed workspace transfers in Closing without readiness/boot ownership; this keeps source in Error for Move retry rather than granting suspension.
- [x] (2026-11-15) Strict paired workspace-transfer regression passed (20.45 s): NULL/stale intent keeps source under Move recovery; explicit Suspend follows close recovery.
- [x] (2026-11-15) Parent approved sharing startup recovery ownership with GC so a missing archive cannot delete a pending close owner. Regression extended; targeted retry passed (18.46 s).
- [x] (2026-11-15) Full pre-GC controller run: 2,660 passed, 27 ignored, one unrelated Keychain lock-timing failure. Named isolated retry passed (0.25 s).
- [x] (2026-11-15) Final full controller suite passed: 2,661 passed, 27 ignored, zero failures. Workspace clippy with all targets and denied warnings passed (2m21s); formatting and diff checks passed.
- [x] (2026-11-15) Doctests passed (zero cases).
- [ ] Obtain a clean independent review of the final origin and GC fixes.


## Surprises & Discoveries

- Full controller validation found one expected-output correction: the existing HTTP guidance case expected lowercase `reference`, while the new safe failure sentence uses `Reference`. The existing case now checks the full cause/action/reference message. Evidence: `/tmp/suspend-controller-suite.log`; only this failing case is rerun.

- The ready-close regression's first run passed its terminal Stopped/checkpoint/Move-resolution assertions, then failed when it reset the fixture onto an already reclaimed placement. The reset now uses a fresh destination locator, respecting the environment ledger's ownership boundary; the focused retry is running. Evidence: `/tmp/suspend-ready-close-regression.log`.

- The first test command used the directory name instead of the Cargo package name; corrected `mj-controller` to `brokk-mj-controller` before any tests ran.
- Dev dependency compilation encountered many mbx shared C-object coordination waits; normal build storage and cargo wrappers were preserved.
- A worker boot intent protects the pre-Running launch window; `restore_into_target` removes it after publishing Running, before Move saves StartingQueue. The existing destination identity must survive the subsequent Closing transition.

- (child, 2026-11-15) A retained-source handoff Move has no portable full checkpoint, so settlement must preserve its source for Error and repair rather than delete it.


## Decision Log

- Decision: revision-132 daemons retain their pre-existing reaper behavior, including possible destructive fallback to an older checkpoint after losing a Move row. Compatibility means no worse than before; the fix applies only to upgraded daemons. Parent explicitly approved retaining minimum revision 132 rather than authorizing a breaking upgrade, 2026-11-15.

- Decision: compatible revision 133 adds nullable `session_moves.close_requested_operation_id`; minimum compatibility stays 132. Parent approved after audit of eligible revision-132 writers, which name columns and preserve the new column. NULL means conservative Move recovery; only an exact current Move-ID match grants close ownership. A trigger clears old intent when an operation ID changes or explicit retry resets cancellation, including writes by older binaries. No JSON shape or wire protocol changes.
- Decision: only `persist_suspension_intent` writes close authority, together with readiness and Closing. Successful/terminal-failed outcomes clear it in their publication transactions; Destroying is nonterminal and retains accepted teardown authority. Parent approved, 2026-11-15.
- Decision: plain cancellation of a proven sealed workspace transfer reuses `rollback_move_destination` under Move recovery, with ordinary keep-for-retry persistence and no Move resolution. This is the paired cancellation case, not Suspend, and must preserve source data without leasing an uninstalled destination. Parent approved, 2026-11-15.

- Decision: the same startup ownership predicate also routes a cancelled pre-queue Move with a recorded ready destination in Destroying to interrupted suspension recovery. Rationale: a crash in the newly added ordinary destination teardown must resume that teardown rather than treating it as a retained source stop. Parent approved this completion-path correction, 2026-11-15.

- Decision: ordinary verified close resolves a cancelled pre-queue Move in the same transaction that publishes its restored destination as Stopped, retaining the destination checkpoint. Rationale: preserving readiness must not leave Resume blocked after successful close. This is the final parent-approved scope extension, 2026-11-15.

- Decision: persist restored source identity/environment binding and Move resolution in the same resumed-session transaction, reading the latest Move intent inside that transaction. Rationale: Error must never commit with an unresolved source-owning Move, and cleanup updates must not be overwritten by stale snapshots. Review findings accepted, 2026-11-15.
- Decision: retire prepared destinations before either source settlement branch, including Created and CleanupPending states without target locators. Rationale: successful physical cleanup must precede release of durable ownership. Review finding accepted, 2026-11-15.

- Decision: no proven source seal (`recovery_session` absent) means Error with every placement kept; Resume repairs in place before retrying Suspend. Do not ask a possibly sealed relay and then destroy from an older full checkpoint. Rationale: an early-cancelled Move requiring an extra step is preferable to losing newer source work. Approved by parent, 2026-11-15.

- Decision: preserve the already-Running destination identity in the same `save_suspension_intent` transaction that publishes Closing. A matching worker boot intent also prevents source settlement before Running publication. Rationale: destination startup can precede Move's StartingQueue write, so placement alone cannot be readiness. Approved by parent, 2026-11-15.
- Decision: startup assigns a cancelled Move to close only with an exact persisted Suspend intent. NULL or stale intent keeps interrupted state under Move recovery. The same recovery-owner predicate prevents GC from deleting either interrupted owner. Approved by parent, 2026-11-15.

- Decision: the daemon's existing `wait_before_close` stays the single serialized point of supersession. It cancels the Move, awaits the Move's actual completion, reloads its durable record, then settles under the existing lifecycle owner. Settlement lives in `controller/move_session.rs`, called by close recovery before any worker lease is requested. There is no independent recovery task and no worker-upgrade fallback. Date: 2026-11-15.
- Decision: the Move record is the authority for source seal, destination readiness, checkpoints, placements and queued admissions. `Cancelled` alone is not evidence of a cut, so classify from the durable boundaries the record retains. Date: 2026-11-15.
- Decision: source sealed and destination not ready means stop and remove each exact partial destination placement with the existing retirement and cleanup helpers, keep the record and the source checkpoint, and publish Stopped only after cleanup succeeds. Destination ready (queue admission started) is irrevocable: ordinary close checkpoints the destination, and the source is never restored over accepted destination work. Date: 2026-11-15.
- Decision: for a handoff-only Move that retained its source workspace, settlement must capture or preserve the full source workspace before it removes the source. If that cannot finish safely, the terminal result is Error with the source retained, never Stopped from a non-portable artifact. Date: 2026-11-15.
- Decision: every failed close completion transitions `Closing` to Error while keeping recoverable data and placements, instead of only replacing `last_error`. Startup recovery of an interrupted close uses the same publication. Date: 2026-11-15.
- Decision: there is one safe lifecycle-failure rendering path: a trusted cause sentence, a concrete next action, and the reference. Typed refusals keep their trusted public reason. Other failures use a handful of stage names (worker contact, checkpoint, cleanup), not a taxonomy. Raw paths, hosts and error chains stay in the log. Example: "Suspension could not contact the worker. Resume the session to repair its runtime, then retry suspension. Reference …". Date: 2026-11-15.
- Decision: `mj suspend` keeps returning on acceptance. The issue needs the failure reason to be visible, and the stored safe message shown by `mj sessions` and the dashboard provides it. Making the CLI wait for the outcome is out of scope. Date: 2026-11-15.
- Decision: no schema, relay protocol or persisted format change is expected. If one turns out to be needed, classify any migration per AGENTS.md. Date: 2026-11-15.


## Outcomes & Retrospective


Cases 2 and 4 are implemented and validated. A superseded Move settles from its durable source cut without leasing an uninstalled destination worker. Source restoration and Move resolution commit together; prepared cleanup state survives resolution; verified destination teardown resolves its Move while retaining the destination checkpoint. Unproven seals keep every placement and report actionable Error. Stored failure guidance carries a safe cause, next action, and reference. CLI wait behavior, Move JSON shape and relay protocol are unchanged. Revision 133 adds a compatible nullable close-intent column while retaining compatibility floor 132.

The initial review exposed real atomicity and resource-ownership defects, all corrected in the existing owners. The expanded regression protects those failures and the readiness completion path. Validation used isolated data and real LocalBare cleanup with fake AWS commands, not a live remote clone or harness repair smoke test. The final review verdict is reported by the review tool and in the sub-agent handback.


## Validation and Acceptance

One Hard-won test of the Tricky-condition kind, at the real Move/close ownership boundary. It holds destination clone preparation until a Suspend cancels it, then asserts the terminal state, the preserved checkpoint and record, and the correct cleanup of placements. Mark it `// Hard-won: #1542: Suspend superseding a Move mid-clone left the session in Closing indefinitely`. Add boundary coverage for "destination ready" versus "unsealed source" only where no existing test protects it. Extend an existing lifecycle error golden if one exists. Run targeted tests per round, then the full suites of the touched crates and workspace clippy once, on the dev profile, outside the sandbox. Isolate the known hook flakes.

Plan update (2026-11-15): recorded approved readiness/startup/no-seal decisions and successful focused regressions; deferred full validation until the final review round.

Plan update (2026-11-15): incorporated all initial review findings; expanded the existing regression with SQLite abort rollback and isolated fake AWS cleanup checks.

Plan update (2026-11-15): final focused checks passed; full validation runs before the next review so its results are included in the same fix commit.


## Repository Orientation and Verification


`mj-controller/src/daemon/close.rs` waits for the superseded Move and invokes settlement before choosing the ordinary close route. `mj-controller/src/controller/move_session.rs` decides from the Move's durable seal and readiness fields. The existing rollback owners in `controller/move_session/transfer.rs` and `controller/resume/in_place.rs` accept the close's persistence continuation. `database/sessions.rs` restores source/session ownership or publishes terminal destination teardown, and `database/session_move.rs` resolves only the cancelled Move's restore authority in those same transactions. `daemon/session_move.rs` assigns interrupted work to exactly one startup owner. Trusted failure reasons originate at lifecycle boundaries and cross both daemon and phone result channels.

Run verification from this worktree with its normal mbx cargo wrapper and normal build storage. Run every Cargo test and clippy invocation outside the restricted sandbox. The tests supply named isolated stores; do not start a test binary against the default instance.

    cargo test -p brokk-mj-controller suspend_superseding_move_before_worker_install_settles_the_source
    cargo test -p brokk-mj-controller in_place_move_failure_retains_environment_for_retry
    cargo test -p brokk-mj-controller
    cargo test -p brokk-mj-controller each_rejected_action_keeps_its_own_status_and_guidance
    cargo test -p brokk-mj-controller --doc
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all -- --check

The full unit run completed with 2,659 passing tests, 27 ignored tests, and one old error-text expectation. Only that expectation was updated, and its targeted retry passed, giving 2,660 passing cases across the full run and correction. Doctests completed with zero cases. Do not repeat the full suite to reconfirm this one expectation correction. Detailed logs are in the assigned agent report directory; the final report names them.

Plan update (2026-11-15): all required validation completed; the full run's sole old text expectation was corrected with a targeted retry, and workspace clippy passed without warnings.

Plan update (2026-11-15): rereview origin distinction and the approved compatible intent contract supersede the earlier no-schema expectation; final validation is pending this last round.

Plan update (2026-11-15): final post-GC suite passed (2,661 cases, 27 ignored, no failures), including the previously flaky Keychain test. Workspace clippy and formatting passed. The earlier Keychain failure was unrelated lock-timing flakiness; its named isolated retry also passed. No hook tests are present in this touched crate (zero filtered cases). Compatible revision 133, its rollback and reader-132 preservation all passed under isolated data.
