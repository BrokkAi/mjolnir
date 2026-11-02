# Stop repeated session recovery warnings

This ExecPlan is maintained according to `.agents/PLANS.md`. It describes the daemon (the background process that owns session records), the worker (the process that runs a session), and the terminal dashboard (TUI).

## Purpose / Big Picture

Repeated failed worker polls currently restart footer warnings for the same sessions. A missing working directory can also be recorded independently by the daemon and TUI. After this fix, a worker failure is reported once per outage, definitive missing targets settle under daemon ownership, and later recovery permits a new outage to be reported. Existing session history and recovery archives must survive cleanup.

## Progress

- [x] (2026-11-02 18:32Z) Trace warning generation and display through independent investigations; inspect current live daemon and session records read-only.
- [x] (2026-11-02 18:32Z) Update the clean current master checkout to upstream 9a045fd13.
- [x] (2026-11-02 18:49Z) Implement daemon-owned definitive missing-target transitions, preserve write results through reload retries, and park repeated recovery of the same missing target.
- [x] (2026-11-02 18:49Z) Remove TUI persistence and make unreachable notices follow outage transitions without resetting dismissal timers.
- [x] (2026-11-02 19:15Z) Repair child operator-away replay and verify checkpoint-only close with a real worker, over 64 KiB of final journal output, missing cwd, and intact parent files.
- [x] (2026-11-02 19:18Z) Independently review the combined changes and validate affected crates, corrected fixtures, exact late Provisioning race, real-worker acceptance, formatting, and workspace Clippy in the dev profile.
- [x] (2026-11-02 19:45Z) Commit and push ae5b18abb plus upstream merge dc4dbe4b6, install normally, and close all 12 diagnosed children while preserving histories and parent targets. Fresh runtime snapshot: 59 live worker views, zero errors, none of the 12 old IDs present.

## Surprises & Discoveries

The initial live daemon was 2.64.1+aa6200c0. Its database had no unreachable or missing-directory `last_error` values even though the user saw these warnings. This supported tracing live poll warnings rather than deleting historical errors. The initial listing contained 7,875 sessions: 7,295 suspended, 504 parked, 64 running, and 12 suspending. Existing suspension/checkpoint failures were separate and were not erased just to quiet the dashboard.

The TUI calls `report_session_unreachable` for each failed poll. `mj-tui/src/notify.rs` unconditionally restarts notice display. `mj-cli/src/pollers.rs` also persists `TargetMissing` despite a revision-guarded daemon path already doing so. The store permits repeated Error-to-Error transitions.

A filtered read of the live `RuntimeChanges` snapshot found exactly 12 error views. Every one belongs to a child session stuck in durable Closing after suspension failure; all 64 Running session views were healthy. Eight Closing views fail projection with an operator-away resolution lacking its captured request; the others report refused worker sockets. All 12 borrow parent workspaces and have no independent checkpoint. Each parent is Running and has an existing recovery archive. None of the local project directories checked is currently missing. This is accumulated lifecycle state plus current warning behavior, not evidence that current Running sessions are losing directories. Investigate and finish child closure through the owning lifecycle rather than changing records directly.

The retained child `launch.json` working directories reveal three removed local scratch directories beneath otherwise existing parent projects. These are the source of the missing-directory reports. The normal child close currently requires a relay connection; closing a dead child must first make its retained journal readable without restarting a harness or recreating the scratch checkout. Existing checkpoint-only worker support is being evaluated for that purpose.

## Decision Log

Decision: Keep missing-target facts under daemon ownership and remove the duplicate TUI writer. Rationale: one serialized owner must settle durable state, even when no dashboard is attached. Date: 2026-11-02.

Decision: Track notice episodes, including diagnosis updates and recovery. Rationale: comparing only currently displayed text cannot suppress alternating sessions or late asynchronous diagnoses, and must not hide a genuinely new outage after recovery. Date: 2026-11-02.

Decision: Preserve all histories and archives. Rationale: cleaning warning noise does not authorize destroying session work. Date: 2026-11-02.

Decision: Diagnose failed child closure separately from missing-target detection. Rationale: current live errors are Closing children, and definitive target absence cannot be assumed from a refused socket or a projection failure. Date: 2026-11-02.

Decision: Repair operator-away child routing at the projection entry point for both request and resolution events. Rationale: captures are intentionally absent for children because their parent handles their questions, so requiring a capture only for child resolutions makes replay fail permanently. Preserve the top-level missing-capture error. Date: 2026-11-02.

Decision: Let an unreachable child close restart a worker in checkpoint-only mode only after the existing liveness probe proves the worker dead. Rationale: this mode reads the retained journal without starting the harness, then uses the existing close barrier, final synchronization, and private-root teardown sequence. Failed recovery must preserve the journal and Closing record. Date: 2026-11-02.

Decision: Keep unreachable footer wording stable and log completed diagnostics rather than replacing the footer. Rationale: replacing it resets the minimum display period and can resurrect an old outage over newer information. Date: 2026-11-02.

Decision: Keep the old internal missing-target action's wire shape but acknowledge it without mutation. Rationale: an already-running old TUI must not retain a second unguarded writer; the daemon's own worker view now owns the transition. No protocol change is necessary. Date: 2026-11-02.

## Outcomes & Retrospective

The warning ownership/display bugs and blocked child-close paths are fixed and installed. All 12 diagnosed children have been closed without losing their histories or changing their eight parents' Running states and target locators. The live runtime no longer publishes their failing views. The requested cleanup is complete.

The controller full suite completed with 2,231 passing tests, 19 ignored cases, and one new fixture failure (`missing_target_retry_completes_a_committed_transition` saved its session before creating the referenced workspace). Creating the fixture workspace corrected it and the focused rerun passed. The full CLI/TUI suites passed: TUI unit tests 565 passed/2 ignored, CLI unit tests 231 passed, and all CLI integration suites passed (four explicitly ignored tests in one suite). `cargo fmt --all -- --check`, `git diff --check`, and default-members `cargo clippy --all-targets -- -D warnings` passed in the dev profile. Final stale-client action and idempotent database-transition regressions passed separately. The exact late Provisioning race case `a_late_missing_workspace_report_cannot_invalidate_a_newer_session` passed on final source (1/1).

The real-worker acceptance passed: `MJ_WORKER_BINARY=/mnt/optane/tmp/mj-child-close-0aa688df/mj-worker cargo test -p brokk-mj-controller dead_child_close_recovers_journal_without_recreating_its_cwd -- --ignored --nocapture` imported the final 106,496-byte event into materialized history, reached Stopped, cleared the child target, removed only its private root, left the missing cwd absent, and preserved the parent record and files. The companion `unreachable_child_keeps_its_private_journal_until_worker_death_is_proven` also passed. Both ran elevated in isolated stores. An initial acceptance fixture lacked the local target template; adding that template to its isolated config corrected it without production changes.

The normal complete installation finished, and an ordinary installed `mj sessions` initiated daemon handoff. New daemon build dc4dbe4b6 automatically closed 11 of the 12 old children through their normal lifecycle. The final Morannon child had a separate stale socket entry in a live fuse-overlayfs container: all path components were directories, the socket had no listener, worker unlink returned ENOTDIR, and a guarded operator unlink returned ENOENT while the entry remained visible. No host upper-layer inode or mount was changed. Holding the child's existing exclusive worker.lock, verifying a socket inode and ECONNREFUSED, and renaming only that socket to a unique backup succeeded. The retained journal and state metadata were unchanged. A normal installed `mj suspend --session ea6ea7905f724d79c2a22fec2aa46138 --acknowledge-unpublished-work --json` then finished the child close; the flag satisfied the CLI's generic publication preflight, while the ownership-aware child close left the parent workspace alone.

Final live acceptance verified all 12 records are Stopped, have no target or last_error, retain their original native session IDs, and have Closed materialized histories at event frontiers strictly newer than the baseline. All eight parent records retain their Running state and original target locators. A fresh read of the daemon runtime feed reports 59 worker views with zero errors and none of the old failing child IDs. The pushed merge's CI passed, and the upstream worker-image publication included by that merge passed. Cleanup required no session destruction, direct SQL update, parent stop, or mount change.

## Context and Orientation

`mj-controller/src/session_manager/actor.rs::run_session_actor` synchronizes worker views and requests recovery. Its recovery results distinguish transient unreachability from a definitively absent worker target or working directory. `mj-controller/src/daemon/snapshot.rs::persist_missing_target` records definitive failures using the observed record revision, reloads the controller, and emits a runtime notice. The database transition lives in `mj-controller/src/database/sessions.rs`. `mj-cli/src/pollers.rs::apply_worker_poll_update` consumes worker views; it currently duplicates persistence through dashboard background I/O. `mj-tui/src/notify.rs` owns local reachability notices, while `mj-cli/src/dashboard/io.rs` applies asynchronous worker diagnoses. Diagnosis from an earlier outage must never overwrite a later definitive warning or a user dismissal.

## Plan of Work

The controller implementation slice makes definitive missing-target transitions idempotent and prevents recovery views from oscillating while the same desired target is absent. Keep persistence failures visible and ensure a changed target or explicit lifecycle restoration can recover. The terminal implementation slice removes missing-target persistence tasks and their completion messages, then reports unreachable state once per outage. Diagnosis logs details and leaves the footer unchanged. Child closure proves worker death before checkpoint-only restart and does not remove its private files until the final journal events are imported and the worker reports Closed. Add regression coverage beside existing tests for state transitions, stale observations, and notice timing; avoid tests that merely restate implementation.

Independently review the combined diff for lifecycle races and lost failure reporting. Reuse passing focused tests. Run the complete suites for the touched crates and workspace Clippy once after integration. Commit only this task's files on the current branch and push to origin/master. If upstream moves, fetch and merge it without rebasing.

### Milestone 1: Settle and display worker failures once

The daemon becomes the sole writer of definitive missing-target failures. Its worker actor stops polling an unchanged absent target while remaining available for explicit lifecycle recovery. The TUI tracks each unreachable outage and clears tracking when the session recovers, stops, or disappears. The existing internal action sent by older clients cannot change session state. The timing regression must fail if repeated polls restart the original dismissal deadline; store and actor regressions must show stable state and renewed recovery when the desired target changes.

### Milestone 2: Close retained child journals safely

Operator-away projection excludes child requests and resolutions consistently, allowing their retained journal events to import. An ordinary child close that encounters a dead worker restores a checkpoint-only worker on the same private root, without launching the harness. The integration regression uses over 64 KiB of final journal content, a missing child working directory, and an intact parent workspace. It must prove final content is durable before the child root is removed, and that an unproven-dead worker is left intact.

### Milestone 3: Validate and clean the running records

Run the complete touched-crate suites and default workspace Clippy after the implementation settles. Run the journal recovery regression with a matching built worker and isolated store, naming any skipped external requirements explicitly. After commit and push, install through the normal script and initiate ordinary bounded daemon handoff. Retry normal suspension only for the 12 diagnosed children if startup recovery has not already finished them. Compare the final live snapshot and parent records with the baseline; no failed child view should keep producing warnings, and no parent workspace or recovery archive should be removed.

## Concrete Steps

Work from the repository root. Use normal mbx-managed `cargo` commands without changing target directories. Run every test command with elevated sandbox permissions. The focused commands will use `cargo test -p <touched-package> <regression-filter>`. Final validation uses `cargo fmt --all -- --check`, full `cargo test` suites for touched packages, and `cargo clippy --all-targets -- -D warnings` in the dev profile. Record exact completed commands and results below as they become available.

Read the running instance only for diagnosis. Any invocation of a newly built binary for acceptance must use a separate named `--instance recovery-warning-noise` with isolated configuration and data directories. Installing an accepted build uses `scripts/install.sh` without concurrent commits, and an ordinary installed CLI command initiates normal bounded daemon handoff. Never restart or mutate the live store with a test binary.

## Validation and Acceptance

Repeated identical unreachable updates must leave the original notice timestamp unchanged. A completed diagnosis after dismissal or a different warning must not reshow the outage. A successful worker update resets the episode, and a subsequent failure reports again. A definitive missing target must settle once with no dashboard attached, preserve its original reason and timestamp on duplicate observations, and reject observations from an older session revision. If the target changes, recovery can resume.

Inspect the live dashboard and daemon after safe application: the same settled sessions must no longer cycle through footer warnings. Preserve actionable recovery errors in session records and report any unresolved current failures separately.

## Idempotence and Recovery

Do not destroy sessions, remove working directories, or clear arbitrary database error fields. Tests use isolated stores and existing fixture cleanup. A failed durable write remains visible and can be retried by its daemon owner; it must not require a TUI to succeed. Normal installation/handoff preserves active workers.

## Artifacts and Notes

The read-only diagnostic handbacks live in `.mj/agents/97763fe39580bb654d19ba324e9b9a42/diagnosis.md` and the child report for `518a81a4c7aadb60303883af501fe78f`. These are temporary evidence; this plan retains the causal conclusions needed to resume.

## Interfaces and Dependencies

No new crate, dependency, public API, or database schema is planned. Reuse existing `ViewError`, `WorkerRecoveryOutcome`, runtime notice publication, revision guards, and `DashboardState` reachability tracking. Remove obsolete private TUI persistence types once no callers remain.

Revision note (2026-11-02): Created from the two independent diagnoses and live read-only evidence to coordinate daemon and terminal ownership slices.

Revision note (2026-11-02 18:49Z): Recorded the live Closing-child causes, checkpoint-only close design, stable footer wording, legacy-client write removal, and completed implementation slices. Focused projection and DB idempotence regressions have passed; combined validation is pending the close recovery integration fixture.

Revision note (2026-11-02 19:04Z): Recorded the full controller result and targeted fixture correction; reassigned real-worker acceptance after a provider-capacity failure. Retained only filtered session status evidence from the live snapshot, removing its full configuration copy.

Revision note (2026-11-02 19:33Z): Code and normal installation are delivered; normal startup recovery closed 11 children. Recorded the final remote filesystem-path blocker and verified all child histories and parent targets before investigating it.

Revision note (2026-11-02 19:45Z): Recorded successful guarded socket rename, ordinary final child close, complete live acceptance, and successful upstream CI/image checks. The requested warning cleanup is complete.
