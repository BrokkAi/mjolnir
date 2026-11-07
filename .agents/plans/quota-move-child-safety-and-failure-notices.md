# Restore quota Moves and report their failures

This ExecPlan is a living document maintained under `.agents/PLANS.md`. The work began as a small flag correction. Review exposed child-stop and failure-delivery ownership problems, so this plan records the expanded design before final validation and delivery.

## Purpose / Big Picture

When a subscription reaches its weekly limit, Mjolnir should switch an idle session to a compatible profile without interrupting accepted child work. A session using native delegation must not be rejected merely because it lacks Mjolnir parent tools. If the switch fails, the conversation must explain the failure and preserve the public recovery instructions, including when the original worker has stopped. Resuming successfully must retire the failure display. Private parser diagnostics must remain out of conversation journals and browser responses.

## Progress

- [x] (As of 2026-11-07 19:22Z) Diagnosed #1470: native delegation made the Move warning flag true even without active managed children. The reported session has only stopped reviewer children.
- [x] (As of 2026-11-07 19:22Z) Shared the existing close operation's active-child selection and corrected automatic Move preparation.
- [x] (As of 2026-11-07 19:22Z) Added the failed-Move conversation view, safe public recovery text, retirement after Resume, and routing without a readable transcript.
- [x] (As of 2026-11-07 19:22Z) Restricted quota journal messages to explicitly approved safety refusals or daemon-log references; validated real TOML credential canaries at the outgoing command boundary.
- [x] (As of 2026-11-07 19:22Z) Removed the automatic child-stop hook and moved asynchronous failure recording inside lifecycle admission.
- [x] (2026-11-07 19:28Z) Validated the final child-stop ownership and asynchronous notice changes: 71 affected tests, workspace clippy, formatting, and the diff checks passed. Changes are prepared for the current branch's required review and push.

## Surprises & Discoveries

The worker request drain does not serialize every source of child creation. A worker is the process running one session; the daemon is the server managing session records and lifecycle operations. Closing the worker's child-request queue does not prevent a daemon HTTP request from registering another child. An empty-child check followed by a hook that reads all children again is therefore unsafe. Automatic Moves must never invoke that stop hook.

Failure can occur before a durable Move record is created. A browser card based only on Move records cannot report those failures. The existing worker journal, an append-only conversation record, can record them through `RelayCommand::RecordNotice` while the source worker remains available.

General `Refusal` errors are not sufficient proof that text can enter the quota journal: configuration-loading refusals can wrap a TOML diagnostic containing source lines. Only the static Move-safety refusal code certifies text for this boundary. Real parser diagnostics containing a fake credential now exercise this condition.

## Decision Log

Decision: use `State::active_child_session_ids` for the set a close would stop. Rationale: parked children and oracles count, but stopped reviewers do not; Move must use the same interpretation as close. Date/Author: 2026-11-07, Codex.

Decision: automatic in-place Moves never invoke `before_move_source_stop` after their empty-child check. Manual Moves without a child-stop confirmation skip it too. Rationale: a newly registered daemon-side child must survive rather than be picked up by a second scan. Older workers that cannot freeze child requests remain ineligible for automatic Move; a manual Move can present the existing warning. Date/Author: 2026-11-07, Codex.

Decision: preserve the existing public lifecycle diagnostic, correlated with the Move reference, rather than reconstructing recovery instructions in the browser. Rationale: it includes cleanup charges and retained-checkout instructions, excludes raw errors, and clears after successful recovery or new work. Date/Author: 2026-11-07, Codex.

Decision: record asynchronous quota failures before the admitted lifecycle completes. Admission is the daemon's hold preventing an upgrade from cancelling accepted control work. Rationale: failures before Move persistence still need a durable conversation line, and its delivery belongs to the bounded operation rather than an unowned watcher. Date/Author: 2026-11-07, Codex.

Decision: failure notices identify the attempted destination without asserting a current profile. Rationale: the worker journal is historical, so a later Resume or Move would make that assertion stale; the session header supplies the current profile. Date/Author: 2026-11-07, Codex.

Decision: retain the existing Move panic boundary instead of adding a second notification watcher. Rationale: `run_move_controller_work` awaits its blocking task and converts its panic `JoinError` into the error handled by the admitted notice reporter. The lifecycle admission prevents automatic handoff from cancelling that work. Date/Author: 2026-11-07, Codex.

## Context and Orientation

`mj-controller/src/daemon/continuation/quota.rs` selects a destination after a quota stop and stores the retry plan in the worker. `mj-controller/src/daemon/session_move.rs` admits and supervises the resulting Move. `mj-controller/src/controller/move_session.rs` prepares and executes it. An in-place Move replaces the harness and profile inside the existing environment; it does not recreate that environment.

`mj-core/src/state.rs` owns active-child selection. `mj-controller/src/daemon/close.rs` uses that selection when actually stopping children. The Move executor's `before_move_source_stop` hook invokes that close behavior, so checking an empty set is insufficient unless automatic Move skips the hook itself.

`mj-controller/src/server_runtime/snapshot.rs` joins the current public session error to `ViewerMoveRecovery` in `mj-controller/src/server/viewer_types.rs`. The public error must name the same Move reference. The resulting optional `failure_notice` owns both the conversation card's text and its visibility. `mj-controller/src/web/viewer.js` permits that status view without enabling transcript reads or prompting a stopped worker.

## Plan of Work

### Child ownership

Keep full environment-replacement warnings because native children may also be affected. For an in-place Move without parent tools, derive the warning from active managed children. Preserve the warning the user accepted through preparation. After draining worker requests, refuse automatic Move if children would be stopped; regardless of subsequent daemon registrations, never call the stop hook for an automatic Move. Extend the existing Native-source Move integration to execute a quota Move and assert zero stop-hook calls.

### Failure delivery and privacy

Use the existing `Refusal` mechanism with `QUOTA_MOVE_SAFETY_REFUSAL` for static safety messages. `public_quota_failure` must never copy another error's display text. Preparation and admission failures receive a log reference, with the full diagnostic logged only by the daemon.

Inside the admitted Move closure, inspect the `Result<MoveOutcome>` before completing the lifecycle. Use `quota_move_failure_notice` to select safe text and public recovery guidance. Send `RecordNotice` through `record_quota_move_failure` with an operation-derived command ID. This writes a historical failure after the earlier switching notice without replacing a newer turn's quota record. If no worker can accept the notice, report the recording error; failures after source suspension remain represented by the durable Move record.

### Conversation view

Publish the full current public diagnostic only when its prefix and final reference identify this Move. Do not publish the raw Move error. Hide the failure card when the public diagnostic retires, even if Resume leaves a cancelled Move record. Keep the existing conversation route while Resume is waiting for the worker's transcript, and provide a Resume-page link to a failed Move's status. Wrap long checkout paths so recovery instructions remain readable on phones.

## Concrete Steps

Run Rust commands from the checkout root, outside the restricted sandbox. Preserve the existing Cargo/mbx build layout. Existing tests use their own configuration and data directories; any manual invocation of a new `mj` build must use `--instance quota-move-validation`.

    cargo test -p brokk-mj-controller -p brokk-mj-core --lib -- controller::move_session::tests daemon::continuation::tests a_lifecycle_failure_carries_a_refusal_across_its_result_channel
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

For the view and public-diagnostic projection, use the affected integrations:

    cargo test -p brokk-mj-controller -p brokk-mj-core --lib -- server_runtime::tests server::tests

From `tests/e2e/web`, with dependencies installed by `npm ci` or borrowed from an existing checkout, run:

    node node_modules/@playwright/test/cli.js test resume.spec.js boot-state.spec.js --project=deterministic

The browser tests use local fixtures, not a live daemon. Only use `MJ_UPDATE_GOLDEN=1` when intentionally updating the checked-in expected output, then review the diff and rerun without it. Remove borrowed dependency links and `test-results` after the browser processes finish. Commit only changed files on the current branch and use the repository's review and push workflow; do not create a branch or install the application as part of this task.

## Validation and Acceptance

The Native-source integration must complete its automatic Move with zero stop-hook calls. It still closes and reopens the worker request queue. The late-child integration must reject an unconfirmed stop without invoking the hook.

The quota coordinator integration must capture a durable `RecordNotice` after a failure occurring after the initial switching message. Test both an early error with no Move record and a failed `MoveOutcome`. Real TOML errors and configuration refusals containing the credential canary must not place the token, private profile path, or SSH host in any outgoing recovery or failure-notice command. The explicitly coded safety sentence must remain visible.

The browser golden must show a stopped worker's failure despite `open: false`, preserve the view through reload, expose the full missing-checkpoint instructions, and retire the card after successful Resume while its cancelled Move record remains. Recovery paths must fit the phone viewport. Existing Move recovery integrations verify that their actual stored public messages reach the view unchanged.

## Idempotence and Recovery

No schema migration or worker protocol change is required. Failure recording uses the existing journal command and a deterministic operation-derived ID, so lost acknowledgements cannot create duplicate history. It does not replay user prompts or clear another turn's quota recovery. The environment remains intact for an in-place Move. Retry only the failing tests after a validation failure; do not repeat whole suites for each review round.

## Outcomes & Retrospective

Earlier validation completed the controller and core suites, with one unrelated worker-installer timing test passing alone after failing under host load. The view/publication integrations passed 141 tests; 26 browser routing/resume cases and the updated golden passed. The final ownership and asynchronous-notification changes passed all 71 affected tests and workspace clippy. Implementation and validation are complete; review and remote delivery follow the repository workflow. The live installation has not been changed.

The central lesson is to retain one owner for each fact: close owns active-child selection, the accepted operation owns permission to stop children, the public lifecycle diagnostic owns current failure guidance, and the worker journal owns conversation history.

## Artifacts and Notes

The final affected-module run produced:

    test result: ok. 71 passed; 0 failed; 0 ignored

That run includes a complete automatic Native-source Move with zero child-stop calls and the quota coordinator's recovery-command and asynchronous-notice cases. The browser golden is `tests/e2e/web/golden/viewer_resume.txt`; it includes both the stopped-worker failure and missing-checkpoint recovery instructions.

## Interfaces and Dependencies

Reuse `mj_core::refusal::Refusal`, `SessionManagerControl`, and `RelayCommand::RecordNotice`. The daemon helpers `public_quota_failure`, `quota_move_failure_notice`, and `record_quota_move_failure` remain internal to the daemon. `ViewerMoveRecovery::with_public_failure` joins the existing public diagnostic instead of interpreting raw error chains. Add no crate, alternative transport, global child gate, timeout, or retry budget.

Plan created during follow-up review when the initially small fix grew to include lifecycle admission and child-stop ownership; it records the implemented design and its validation without treating earlier discarded approaches as separate supported paths.

Updated at 2026-11-07 19:28Z after the final ownership and failure-delivery checks passed; Progress and Outcomes now record the completed validation rather than pending runs.

Follow-up review found one remaining Resume routing race: a session-detail request begun in the failed Move view could return after Resume retired the failure, but before the worker became openable. The detail callback now uses the same loading predicate as snapshot routing. The existing browser golden holds that request across Resume; it reproduced the redirect before the fix and passed afterward, without changing expected output. This round changes JavaScript and planning notes only; the completed Rust checks still apply.
