# Recreate containers when Restart must apply changed settings

This living ExecPlan follows `.agents/PLANS.md`. It extends the already committed web creation feature without changing branches. The user asked that Restart stop reusing a container after its mount settings change, and invited a recommendation about reuse in general. The user chose to reuse a proven unchanged container, preserving files and caches, and recreate changed or unverifiable container configurations only after a fresh verified checkpoint.

## Purpose / Big Picture

Saving container mounts followed by Restart must result in a container with those mounts. Today the session record holds the desired list and Restart always tries to keep a reachable container. That makes the edited record an incorrect description of the container's actual configuration. Record the settings applied when the container was created separately from editable desired settings, and let the durable Restart operation choose restoration or recreation from that comparison.

## Progress

- [x] (2026-10-15) Reproduced the defect on d8443f17cb24506c5ee26277fcf319f4 and repaired that session through suspend/resume.
- [x] (2026-10-15) Committed the creation feature as a0102eac2 after browser checks, 2000 controller tests, formatting, and Clippy passed.
- [x] (2026-10-15) Located the missing applied-settings snapshot and the durable Restart phase machine.
- [x] (2026-10-15) Record applied per-session container settings during provisioning and preserve them during worker-only restores.
- [x] (2026-10-15) Add a checkpoint-bound recreation phase, breaking forward migration 83, and serialized settings admission.
- [x] (2026-10-15) Add isolated migration, applied-settings, checkpoint and cancellation regressions; update UI and documentation guidance.
- [x] (2026-10-15) Add daemon-owned recreation projections and explicit warnings with Cancel selected in TUI and web confirmation.
- [x] (2026-10-15) Focused regressions, full touched-crate suites, all 30 browser checks, 56 web unit checks, formatting, and workspace Clippy passed.
- [x] (2026-10-15) Committed the main correction as b0bc92951; final recovery fixes are validated.
- [x] (2026-10-15) Committed final fixes as 96fea4dd3 and merged the upstream version-only 2.46.0 release; publish the validated commits to origin/master.

## Surprises & Discoveries

`Restart` uses `suspend_session_for_restart`, which seals a fresh checkpoint while retaining the container, then `restore_session_in_place_for_restart`. Its existing `Fallback` phase permits cleanup only for a confirmed missing/unreachable target. Reusing that absence-only permission for a reachable container would weaken an intentional safety boundary. A separate recreation phase must require the checkpoint bound to this Restart intent before destroying any existing container.

`TargetRuntimeSettings` already stores creation-owned settings such as the fixed tool-cache placement, but does not record the applied per-session mounts or size. Older writers reconstruct this JSON and would discard a new field. A new persisted phase also changes the table's allowed values. Therefore the migration is breaking and must advance both schema revision and minimum compatible writer/reader revision in the same transaction. Keep the live default store untouched.

## Decision Log

- Decision: Preserve reuse for containers whose applied settings match the requested settings; bare-worker restarts continue retaining their environment.
  Rationale: Reuse preserves workspace, untracked files, and caches; a fresh container is needed only when immutable creation settings must change. The user explicitly chose this behavior.
  Date/Author: 2026-10-15, Codex.
- Decision: Compare desired database settings with an immutable applied snapshot owned by target provisioning, rather than comparing the desired record against itself.
  Rationale: The reported defect occurs precisely because editing the record does not edit the container. Unknown legacy applied settings cannot prove reuse safe.
  Date/Author: 2026-10-15, Codex.
- Decision: Protect settings updates and Restart admission through the database writer's serialized transactions.
  Rationale: An accepted Restart must own its settings selection; an edit racing admission must either precede its loaded snapshot or be refused until completion.
  Date/Author: 2026-10-15, Codex.

## Outcomes & Retrospective

The live session is repaired. Creation Review supports directory attachments. Restart reuses containers with known unchanged settings, checkpoints and recreates changed or unknown containers, and requires acknowledgment of the recovery limits before recreation. Both confirmation surfaces and recovery regressions are complete. Final controller validation passed: 2004 tests, 12 ignored. All other touched Rust suites, 30 browser tests, 56 web unit tests, formatting, and workspace Clippy passed. Implementation and validation are complete; the final publication command is git push to origin/master.

## Context and Orientation

`mj-core/src/state/target_runtime.rs` defines settings captured for an actual selected target; `SessionRecord` in `mj-core/src/state.rs` stores editable `additional_mounts`, resource allocation, and CPU/memory overrides. `mj-controller/src/controller/provisioning.rs` creates the target. `controller/resume/in_place.rs` replaces a worker inside the retained target. `daemon/restart.rs` owns durable Restart admission and its checkpoint/restore sequence. `database/session_restart.rs` stores its phase and sealed checkpoint identity. `database/sessions.rs` stores settings edits. `database/schema.rs` must retain every existing migration and add the next revision. New tests use existing isolated data-directory helpers; no test binary may operate on the default instance.

## Plan of Work

First add a snapshot type for the editable per-session container settings and an optional applied snapshot to target runtime metadata. Capture it during actual target provisioning, preserve it when replacing only the worker, and centralize the recreation decision. Existing records with no applied snapshot are explicitly unknown, not empty.

Next add a durable container-recreation phase to Restart and a forward migration that preserves every existing intent while extending the SQL phase constraint. Retain the old absence-only fallback guard. Reachable-container recreation must follow the fresh checkpoint/seal path, bind that archive's checksum to the intent, enter the durable recreation phase, then use verified-checkpoint teardown and ordinary resume. On cancellation, crash, or daemon replacement, recovery must resume from this durable phase without destroying an uncheckpointed target. No configuration inspection or filesystem work belongs on UI event loops.

Make settings edits refuse an active Restart intent inside the same database writer transaction that would update the settings. Load Restart's controller snapshot after the intent is admitted, so the accepted operation sees every earlier settings update. Preserve native conversation identity and queued work through the existing restore machinery.

Finally add regressions at the store/runtime boundary for changed sources, destinations, access and removals, unchanged reuse, unknown legacy state, checkpoint-bound recreation, and mutation ordering. Update the web settings guidance and user docs so Restart is again the normal way to apply edits. Record the final behavior in project memory. Commit only task changes and push the current master branch; if rejected, fetch and merge origin/master, resolve conflicts and validate affected areas before pushing.

## Concrete Steps

From the repository root, run focused tests outside the sandbox as code changes settle. Finish with:

    cargo fmt --all -- --check
    cargo test -p brokk-mj-core -p brokk-mj-client -p brokk-mj-controller -p brokk-mj-tui -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings

Use existing isolated test helpers and named test instances for any daemon or CLI exercise. Preserve the mbx wrapper and target storage. For changed web guidance, rerun the affected creation golden with `PLAYWRIGHT_BROWSERS_PATH=$HOME/.cache/ms-playwright npx playwright test new-session.spec.js --project deterministic --grep golden_viewer_new_session` from `tests/e2e/web`; inspect any golden update before accepting it.

## Validation and Acceptance

After editing a session's mounts, Restart checkpoints successfully and creates a replacement container whose applied settings match the requested list. A subsequent unchanged Restart can retain that container. Failure to checkpoint does not authorize teardown. A recovered recreation intent requires its own verified archive and never treats a stale checkpoint as permission to remove the target. Settings writes racing an accepted Restart are refused at the database owner. Migration tests use isolated stores and prove old intents and session data survive while older incompatible readers/writers are refused.

## Idempotence and Recovery

Do not install the new build into the live default instance or migrate its store. Failed tests retain their isolated setup. Runtime recreation must remain restartable from the durable intent after every side-effect boundary; retries reuse that intent rather than inventing another cleanup authorization. Existing missing-target fallback behavior stays guarded by confirmed absence/unreachability.

## Interfaces and Dependencies

Use existing `AdditionalMount`, `SessionResourceAllocation`, `TargetRuntimeSettings`, `SessionRestartIntent`, worker lifecycle ownership, verified checkpoint gates, target teardown helpers, and serialized database writer. Add no crate and no dependencies. Keep terminal handoff formats backward compatible; extend database revision compatibility explicitly for stored changes.

## Artifacts and Notes

Completed creation validation: 2000 controller tests passed, 12 ignored; 30 creation browser tests passed, both updated goldens passed, the final mount-keyboard golden passed, the web unit suite passed, and workspace Clippy passed with warnings denied.

Revision note (2026-10-15): Created this follow-up plan after the user requested the Restart correction and asked about unchanged-container reuse.

Revision note (2026-10-15): Implemented applied per-session settings snapshots, recreation phase and breaking migration 83, database-serialized settings admission, and an irreversible teardown reservation in the existing lifecycle state machine. Existing verified-checkpoint teardown performs removal; no separate destructive fallback was added. Regression checks and final validation remain.

Revision note (2026-10-15): The user chose reuse when unchanged and requested a warning before container recreation. Publish the same centralized comparison through the runtime feed and browser summary, and confirm potential loss of files outside the recovery copy. Extend the existing container-editor golden and browser creation/settings golden with cancel and confirm behavior.

Revision note (2026-10-15): A stale screen must not bypass the new warning. Restart admission now checks acknowledgment in the same database transaction that excludes settings edits and persists it with the recovery intent. Both UI confirmations send acknowledgment only after displaying recreation risk. Legacy unfinished intents retain their original in-place restore choice. Incrementally publish the daemon verdict with changed records rather than rescanning all sessions.

Revision note (2026-10-15): Keep the durable Restart intent while cancellation unwinds. The lifecycle owner enters a noncancellable completion phase, then releases a cancelled intent only after its task has stopped. This prevents a settings edit from racing an older controller's final persistence. Existing cancellation coverage now checks that edits remain excluded until completion.

Validation checkpoint (2026-10-15): Recreation/settings and migration regressions passed; the final corrected sealed-checkpoint/cancellation regression passed. The terminal container-details golden and browser creation/settings/recreation golden passed, with Cancel preventing dispatch and acknowledgment reaching the request. Web unit tests passed (56). Formatting and diff checks passed. Commit this coherent checkpoint, merge upstream, then run all touched-crate suites and workspace Clippy before pushing.

Revision note (2026-10-15): Full validation caught migration-replay fixtures selecting seven columns into six. Migration 83 now copies the six legacy columns explicitly. Two revision-81 turn fixtures now rewind all later ledger rows and assert the current revision after a full upgrade. Sixteen affected tests, including the unrelated process-deadline test, passed on focused retry; the two ledger fixtures are included in the final recovery checks. Core (503), TUI (525 plus two ignored), CLI (212 unit tests plus integration suites, including all 14 isolated startup/upgrade tests), client (40), 30 browser tests, and 56 web unit tests passed.

Revision note (2026-10-15): Bind each admitted Restart's exact durable operation id to the existing lifecycle owner. Preserve a failed completion while it still owns an intent, and have a superseding lifecycle release that exact intent before starting its work. A failed Restart followed by Suspend and a completed Restart superseded directly by Resume both have regression coverage. Explicit user cancellation is a durable cancel_requested phase, retaining the settings lock until the task stops. Startup settles a cancelled Closing session as Error with its environment retained, rather than resuming it or interpreting it as an ordinary suspension. Keep internal shutdown cancellation distinct so accepted Restart work can still recover after daemon replacement.

Validation completion (2026-10-15): The final controller suite passed all 2004 tests, with 12 ignored, using the already-built test binary and disposable fixture data under runtime TMPDIR=/tmp. Inherited /mnt/optane/tmp fixture fsync stalled on the busy build pool; no Cargo output, target directory, mbx configuration, or host mount was changed. Final focused cancellation/recovery and migration checks also passed, and workspace Clippy passed with warnings denied. The user requested that any separately identified unrelated Restart bugs be filed as tickets instead of expanding this change. No additional unresolved Restart defect was identified for a ticket at completion.

Publication checkpoint (2026-10-15): Final ownership fixes are committed as 96fea4dd3. Merged origin/master release 09b776ecc, whose only changes are workspace and lockfile versions. Locked offline workspace metadata confirms 2.46.0; no semantic source changed after final validation. Publish directly to the configured upstream as requested.
