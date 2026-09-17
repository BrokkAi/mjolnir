# Replace only the harness when a session moves to the same target

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It must be maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Today, moving a session to another profile (for example from a Claude profile to a Codex profile, or between two Claude accounts) on the same target rebuilds everything: the container is destroyed, a new one is created, every repository is cloned again, the worker binary and the checkpoint archive are uploaded again, and the checkpoint is restored into the fresh clone. The person waits minutes for what is, from their point of view, "run a different agent in the same place".

After this change, a move whose destination target, attached mounts, and resource allocation are unchanged replaces only the harness: the session is checkpointed and sealed, the old worker daemon and profile home are removed inside the existing environment, the new profile is staged there, the harness state is restored from the checkpoint, and the new worker starts. The container (or bare worker root), the workspace, untracked files, and any build caches survive untouched. The conversation shows "Switched from A to B in place …; the workspace and environment were kept."

A move that changes the target keeps today's behaviour. Failure or a daemon restart during the swap falls back to today's recovery contract: the session ends `Stopped` with a verified checkpoint and the usual "Retry move or Resume" affordances.

## Progress

- [x] (2026-09-17 14:05Z) Milestone 1: durable intent (`in_place` on `MoveOperation` and `MovePreparation`, `in_place_move_eligible`, database round-trip tests).
- [x] (2026-09-17 15:10Z) Milestone 2: `SourceTargetDisposition` through `close_session_for_move`; test that Retain leaves `Closing` + target + checkpoint and issues no cleanup command. As committed, `execute_move` still passes `Destroy` and still cleans the source for every move; the branch on `operation.in_place` is wired in Milestone 5, because with the retained close and no in-place restore an eligible profile-only move would fail at `resume_session_controlled` ("session is not stopped, lost, or retryable"). See `Idempotence and Recovery`.
- [x] (2026-09-17 14:40Z) Milestone 3: resume tail extraction (`verify_resume_checkpoint`, `restore_into_target`), pure refactor, existing tests green (`cargo test -p brokk-mj-controller`: 1337 passed, 0 failed).
- [x] (2026-09-17 14:55Z) Milestone 4: `targets::in_place_worker_reset_plan` and `removable_profile_root` + tests.
- [ ] Milestone 5: `restore_session_in_place` for same-harness moves, `execute_move` branch, rollback, notice text, controller tests.
- [ ] Milestone 6: cross-harness in place (joined handoff lane) + test.
- [ ] Milestone 7: recovery arms and the two restart tests.
- [ ] Milestone 8: TUI/CLI/web wording, docs, e2e assertions, live Podman timing check.

## Surprises & Discoveries

- Observation: after `RelayCommand::Close` the worker daemon stays alive and answers `sync` with `Closed`; it only dies today because teardown runs `podman stop` or `close_plan`.
  Evidence: `mj-controller/src/controller/checkpoint/staging.rs:3-16` (`wait_for_relay_closed`), `mj-controller/src/targets/cleanup.rs:41-50`.
- Observation: `clear_relay_state_plan` returns `None` for container locators, so it cannot be the in-place reset.
  Evidence: `mj-controller/src/targets/worker_daemon.rs:141-175`.
- Observation: there are two distinct `TargetLocator` types. `mj_core::state::TargetLocator` is what a session record stores and spells a local bare worker root as a `PathBuf`; `mj_core::targets::TargetLocator` is what commands are built against and spells it as a `String`. `backend_locator` converts between them, so `in_place_worker_reset_plan` takes the `targets` one and `removable_profile_root` does too.
  Evidence: `mj-core/src/state.rs:637-640` versus `mj-core/src/targets.rs:1598-1601`.
- Observation: `target_profile_home` was already computing the per-session profile root and then, for Muse only, appending `muse`. Extracting `removable_profile_root` therefore removed a duplicate match rather than adding one: `target_profile_home` is now that root plus the Muse suffix.
  Evidence: `mj-controller/src/controller.rs`, `target_profile_home`; `cargo test -p brokk-mj-controller` stayed at 1337 passing.
- Observation: the cheapest live-relay fixture for a sealed close is the checkpoint latch harness in `mj-controller/src/controller/checkpoint/tests.rs` (`latch_relay_target` plus `latch_relay_child_serves_stdio`). In checkpoint-only mode it advances `Close` itself through `dispatch_checkpoint_only`, and `CheckpointExportPolicy::ReuseUnchangedArchive` on an already-archived session issues no command at all, which is exactly what a "no teardown happened" assertion needs.
  Evidence: `mj-worker/src/relay/commands.rs:564-624`; the existing `a_close_latch_reuses_an_unchanged_archive_and_exports_after_new_content` asserts `executor.purposes().is_empty()`.
- Observation: `validate_move_checkpoint` re-reads the configuration with `Config::load()` and compares its fingerprint, so any controller test that reaches it must persist its config (and set `MJ_CONFIG_DIR`), not just hold it in memory.
  Evidence: `mj-controller/src/controller/move_session.rs`, `validate_move_checkpoint`.

## Decision Log

- Decision: in-place applies only when target template, attached mounts, and resource allocation are unchanged and `clear_resource_allocation` is false; retries and sub-agents always use the full path.
  Rationale: a target change means a different container or host; retries start from a torn-down or unknown source.
  Date/Author: 2026-09-17, Fable (design).
- Decision: record the choice as `#[serde(default)] in_place: bool` on `MoveOperation` and `MovePreparation` rather than overloading `destination_target`.
  Rationale: `destination_target` is the readiness boundary used by `admit_move_queue` and `current_worker_launch_config`. Classification: compatible-additive JSON, same class as `source_checkpoint_only`. An older daemon reading a newer in-flight row skips it with a warning (`database/session_move.rs:62-76`); the session record alone (`Closing` + checkpoint or `Provisioning` + retained target) lets generic recovery reach `Stopped`. `MovePreparation` crosses IPC with `deny_unknown_fields`; the CLI and daemon ship in one binary, so an old client against a new daemon is not a supported pairing.
  Date/Author: 2026-09-17, Fable.
- Decision: after the sealed close, leave the record `Closing` with checkpoint and target retained; do not introduce a new `SessionState` and never persist `Stopped` with a retained target.
  Rationale: `Stopped` + target is read as "remove the retained Podman container" by `cleanup_stopped_target` (`lifecycle.rs:456`) and deferred cleanup; `Closing` is already `locator_in_flight` for the recovery scan and already recovers through `recover_interrupted_close_managed`.
  Date/Author: 2026-09-17, Fable.
- Decision: never retry in place after a failure or daemon restart; tear down to `Stopped` with the verified checkpoint.
  Rationale: after a failure the worker root and profile home may be half-written; the fresh path is the existing recovery contract and needs no new ownership protocol.
  Date/Author: 2026-09-17, Fable.
- Decision: `WorkerRootReset::InPlace` runs `in_place_worker_reset_plan` itself, inside `restore_into_target`, rather than the caller running the reset as a separate step before the call.
  Rationale: the fresh-target arm already prepares the worker root at exactly that point, immediately before the worker binary is installed and while a surviving daemon would still hold the old binary open. One `match` at one place decides how the worker root is prepared, the `previous_profile_root` the variant carries is live, and Milestone 5 loses a step. Milestone 5 therefore holds `recovery_gate::worker_target_mutex(session_id)` around the `restore_into_target` call instead of around a separate reset command.
  Date/Author: 2026-09-17, implementation.
- Decision: `RestoreIntoTarget` takes `resume_notices: Vec<String>` by value and `retire_after_ready: Option<&ManagedWorktree>` by reference, and derives the two CheckpointRestoreSpec decisions that used to read `conversion` and `plan` from two plain booleans (`restore_repositories`, `primary_repository_root_from_conversion`).
  Rationale: the tail is the only consumer of the notices, so owning them avoids a borrow that would outlive the head's `conversion`; the conversion plan itself is head-only knowledge, and reducing it to the two answers the tail needs keeps `ResumePlan` out of the in-place path, which has no conversion at all.
  Date/Author: 2026-09-17, implementation.
- Decision: `close_session_for_move` from `recover_move_source_stop` always passes `SourceTargetDisposition::Destroy`, even for an `in_place` operation.
  Rationale: it matches the existing decision never to retry in place. A recovered source stop follows a failure or a restart, where the worker root and profile home may be half-written, so it tears down to `Stopped` with the verified checkpoint.
  Date/Author: 2026-09-17, implementation.
- Decision: `WorkerRootReset::InPlace` and `removable_profile_root` land before their production caller does; the enum variant carries a scoped `#[allow(dead_code, reason = ...)]` and `removable_profile_root` is made live by `target_profile_home` calling it.
  Rationale: the workspace builds with `-D warnings`, so a genuinely unconstructed variant would fail clippy between milestones. `removable_profile_root` needed no allow because folding `target_profile_home` onto it is a real simplification. Milestone 5 removes the remaining allow.
  Date/Author: 2026-09-17, implementation.
- Decision: skip `install_attached_resources` in the in-place restore.
  Rationale: it is EC2-only and eligibility requires unchanged mounts, so the resources are already on the instance.
  Date/Author: 2026-09-17, Fable.

## Outcomes & Retrospective

Milestones 1 to 4 are implemented and validated; Milestones 5 to 8 remain.

What exists now that did not before: a move records durably whether it can keep
its environment (`in_place` on `MovePreparation` and `MoveOperation`, decided by
`in_place_move_eligible`); a verified close can stop at the sealed relay and keep
its target (`SourceTargetDisposition::RetainForInPlaceSwap`); the part of a resume
that runs once a destination is ready is a separate, callable unit
(`Controller::restore_into_target`, fed by `RestoreIntoTarget`), as is archive
verification (`verify_resume_checkpoint`); and a target can be emptied of one
harness without being destroyed (`targets::in_place_worker_reset_plan`, with
`removable_profile_root` deciding which profile directory belongs to the session).

Nothing user-visible changed yet: no caller sets `in_place` to anything the move
path acts on beyond skipping source cleanup, and `WorkerRootReset::InPlace` is not
constructed until Milestone 5. The refactor was proved neutral by the existing
suite rather than by new tests, which is the right bar for a pure extraction.

Lesson: the expensive part was not the extraction but finding a live-relay fixture
for a sealed close. The checkpoint latch harness already had one, and reusing it in
checkpoint-only mode with an already-archived session gives a close that runs no
command at all, which is exactly the shape a "nothing was torn down" assertion
needs.

## Context and Orientation

Mjolnir (`mj`) runs a coding agent ("harness": Claude Code, Codex, Kimi, Grok, or Muse) inside a "target" (a local Podman/Docker/Apple container, a bare local directory, or an SSH host with or without a container). A "profile" (`HarnessProfile`, `mj-core/src/config/harness.rs:514`) names a harness kind plus its home directory and settings; the session record stores `last_profile` and `harness_kind`.

Inside a target lives a detached worker daemon: `hel worker run --root <worker_root> --config <worker_root>/launch.json`, started by `start_worker` (`mj-controller/src/controller/worker_binary/process.rs:48-153`) through `podman exec --detach` or ssh. `launch.json` is a `WorkerLaunchConfig` (`mj-core/src/worker_launch.rs:97`). The worker starts the harness process and relays to the controller over `control.sock`.

The profile home is copied into the target per session by `prepare_worker_files` (`mj-controller/src/controller/worker_binary/launch.rs:25-127`) → `stage_profile` → `install_worker_files` (`worker_binary/install.rs:322`). Its location is `target_profile_home` (`mj-controller/src/controller.rs:1187-1225`): `/var/lib/hel/profiles/<session_id>` in containers, `.local/share/hel/profiles/<session_id>` on SSH bare and EC2, `<worker_root>/profile` on LocalBare for Claude or private-home profiles, and the user's real `profile.home` on LocalBare for other profiles. Muse appends `/muse` (on LocalBare the root is `data_dir()/profiles/<session_id>`). `install_worker_files` copies over the existing directory and never removes anything. `prepare_worker_files` also writes `ownership.json` carrying `profile_id`, and `session_launch_config` requires `profile.kind == record.harness_kind` (`mj-core/src/state.rs:1069-1101`).

Move today: `Controller::execute_move` (`mj-controller/src/controller/move_session.rs:826-926`) runs `close_session_for_move` (`lifecycle.rs:73` → `close_session_controlled_with_manager`, `lifecycle.rs:92-232`: persist `Closing`, latched checkpoint, `validate_move_checkpoint`, `RelayCommand::Close`, `CompleteCheckpoint`, `wait_for_relay_closed`, then `destroy_after_verified_checkpoint` at `lifecycle.rs:225`), then `cleanup_stopped_target` for retained Podman containers, then `resume_session_controlled` (`resume.rs:693/707`), which requires `Stopped|Lost|Error`, sets `record.target = None` (`resume.rs:1018`), provisions a fresh target, and runs the restore tail from `resume.rs:1198` (`worker_placement` … `mark_worker_connected` at `resume.rs:1459`). Cross-harness moves run `provision_with_cross_harness_handoff` (`resume.rs:1833`), which runs provisioning and `utility_handoff_while_cancellable` as two joined lanes. Failure calls `rollback_failed_resume` (`resume.rs:1538`), which runs `close_plan` on the current target, optionally retires the managed worktree, restores the previous record, and sets `Stopped` (or `Error` when cleanup fails).

Recovery: `recover_move_managed_controlled` (`move_session.rs:724-784`) branches on `MovePhase` and session state. Daemon startup (`daemon/process.rs:107-135`) runs `recover_moves` before generic interrupted-close and retained-cleanup recovery. `recovery_scan::locator_in_flight` (`recovery_scan.rs:312-320`) treats `Provisioning`, `Checkpointing`, `Closing`, `Destroying` as controller-owned.

`worker_restart.rs:245-336` (`restart_worker_with_installed_binary`) already stops a worker, rewrites its binary and `launch.json`, starts it, and reconnects, inside a live target. That is the precedent for this work.

Earlier plan: `.agents/plans/session-move.md` deferred container reuse on 2026-09-06. This plan lifts that deferral at the user's request (2026-09-17).

## Plan of Work

### Milestone 1: durable intent

In `mj-core/src/state/session_move.rs` add to `MoveOperation` and to `MovePreparation`:

    /// The destination is the source target: only the harness is replaced;
    /// the container or worker root and the workspace are kept.
    #[serde(default)]
    pub in_place: bool,

In `mj-controller/src/controller/move_session.rs` add next to `outcome()`:

    pub(super) fn in_place_move_eligible(
        source: &SessionRecord,
        selection: &MoveSelection,
        is_subagent: bool,
        retry: bool,
    ) -> bool

returning true only when: `!retry`, `!is_subagent`, `source.target.is_some()`, `matches!(source.state, Running | Disconnected)`, `Some(&source.target_template_id) == selection.target_template_id.as_ref()`, `Some(&source.additional_mounts) == selection.additional_mounts.as_ref()`, `source.resource_allocation == selection.resource_allocation`, `!selection.clear_resource_allocation`. Set `preparation.in_place` in `prepare_move_session_controlled` (after selection defaults are filled at lines 410-427) and `operation.in_place` in the `None =>` arm of `move_session_managed_controlled` (line 632), using the same predicate with the `retry` computed there.

Tests: `in_place_eligibility_requires_same_target_mounts_and_allocation` (table); in `mj-controller/src/database/session_move.rs` tests, a legacy row without `in_place` decodes as `false` and a row with `"in_place":true` round-trips (next to `bulk_load_skips_a_move_intent_whose_harness_no_longer_decodes`).

### Milestone 2: stop without teardown

In `mj-controller/src/controller/lifecycle.rs`:

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum SourceTargetDisposition {
        /// Verified checkpoint, sealed relay, then destroy the exact target.
        Destroy,
        /// Verified checkpoint, sealed relay; keep the target and its worker
        /// daemon alive for an in-place harness replacement.
        RetainForInPlaceSwap,
    }

Add the parameter to `close_session_for_move` and `close_session_controlled_with_manager`. Existing callers pass `Destroy`. With `RetainForInPlaceSwap`, after `latched.relay.release()` (line 223) return `Ok(false)` without `destroy_after_verified_checkpoint`. The record stays `Closing` with `checkpoint` installed and `target` retained. Do not stop the worker daemon here: a crash between this point and the restore recovers through `recover_interrupted_close_managed`, which needs the daemon to answer `Closed`.

In `execute_move`: pass `RetainForInPlaceSwap` when `operation.in_place`; guard the `cleanup_stopped_target` block (lines 872-878) with `!operation.in_place`.

Test: a controller test with a fake relay proving `RetainForInPlaceSwap` leaves `Closing`, checkpoint set, target set, and that the recording executor saw no cleanup or stop command.

### Milestone 3: extract the resume tail (pure refactor, own commit)

From `resume_session_controlled_with_repository_preflight` in `mj-controller/src/controller/resume.rs` extract:

    pub(super) struct VerifiedResumeArchive {
        pub archive_path: PathBuf,
        pub manifest: mj_checkpoint::archive::ArchiveManifest,   // whatever type lines 784-791 yield
        pub canonical_session: Arc<CanonicalSessionSnapshot>,
    }
    pub(super) fn verify_resume_checkpoint(session_id: &str, checkpoint: &CheckpointMetadata) -> Result<VerifiedResumeArchive>

covering lines 763-802 (canonicalize, `verify_archive_streaming`, sha and session-id check, legacy-origin check), and

    pub(super) enum WorkerRootReset {
        /// Today's behaviour: clear relay state on bare targets, then mkdir.
        FreshTarget,
        /// Stop the live daemon, clear relay state, unlink worker files, remove
        /// the previous per-session profile home; runs on every locator.
        InPlace { previous_profile_root: Option<String> },
    }
    pub(super) struct RestoreIntoTarget<'a> {
        pub profile: &'a HarnessProfile,
        pub archive: &'a VerifiedResumeArchive,
        pub restored_archive: &'a Path,
        pub resumed_project_directory: Option<PathBuf>,
        pub resumed_container_workspace: Option<PathBuf>,
        pub restore_repositories: bool,
        pub primary_repository_root_from_conversion: bool,
        pub native_continuity: bool,
        pub discard_queued_prompts: bool,
        pub replay_queue: bool,
        pub utility_handoff: Option<String>,
        pub projection_build: Option<tokio::task::JoinHandle<Result<MaterializedSession>>>,
        pub resume_notices: Vec<String>,
        pub install_attached_resources: bool,
        pub worker_root_reset: WorkerRootReset,
        pub retire_after_ready: Option<&'a ManagedWorktree>,
    }
    pub(super) async fn restore_into_target(&mut self, session_id: &str, restore: RestoreIntoTarget<'_>, executor: &(impl CommandExecutor + Sync)) -> Result<MaterializedSession>

covering the restore tail (`worker_placement` through `mark_worker_connected` and the closing `sync`). As implemented, the struct also carries `install_attached_resources: bool` and the two booleans above in place of `conversion`/`plan`, `resume_notices` by value, and `utility_handoff: Option<String>`; `harness_home` is not a field because the tail derives it from `profile`. The head of the old function (state check, preflight, profile/target lookup, compatibility plan, conversion, record transition including `record.target = None`, worktree work, provisioning) stays and then calls `restore_into_target(.., WorkerRootReset::FreshTarget)`. The error arm at 1475-1534 stays in the old function. Adjust field names to whatever the extracted code needs; the rule is that the existing resume path's executed commands and persisted writes are byte-for-byte what they were. Run the full mj-controller suite before committing.

### Milestone 4: target reset plan

In `mj-controller/src/targets/worker_daemon.rs`, next to `clear_relay_state_plan`:

    /// Everything an in-place harness replacement must remove before the new
    /// profile is staged: the running daemon, relay state, the installed worker
    /// files, and the previous per-session profile root. Runs inside the
    /// target on every locator.
    pub fn in_place_worker_reset_plan(
        locator: &TargetLocator,
        session_id: &str,
        previous_profile_root: Option<&str>,
    ) -> Result<CommandSpec>

Script, built with `posix_quote`/`join_remote_command` and wrapped by `locator_command` (so containers run it through `podman exec` and SSH hosts through ssh): `stop_worker_daemon_script(worker_root)`; `rm -rf --` the relay state file and journal directory that `clear_relay_state_plan` names; `rm -f -- <worker_root>/hel <worker_root>/launch.json <worker_root>/ownership.json` (unlink rather than overwrite: a mapped `hel` cannot be overwritten and a stale `ownership.json` must not survive a crash); `rm -rf -- <previous_profile_root>` when given; `mkdir -p <worker_root>`. Guard with `verify_locator`. Purpose text: "reset the worker root for an in-place harness replacement". Give it `.stage(ProvisionStage::Syncing)`.

Add `pub(super) fn removable_profile_root(locator, session_id, profile: &HarnessProfile) -> Option<String>` in `controller.rs` next to `target_profile_home`: returns the per-session root that is safe to delete (for Muse the `profiles/<session_id>` root, not `…/muse`), and `None` when the home is the user's `profile.home` (LocalBare non-private case).

Tests in `mj-controller/src/targets/tests.rs`: `in_place_worker_reset_plan_stops_daemon_clears_relay_state_and_old_profile_home` for LocalBare, SshBare, LocalPodman (assert `podman exec` wrapping and that a `None` root produces no `rm -rf` of a profile path). Test for `removable_profile_root` covering Claude/Codex/Muse on LocalBare and on a container locator.

### Milestone 5: in-place restore, same harness

Note from the Milestone 3 implementation: step 3 below is already done by
`restore_into_target` when it is given `WorkerRootReset::InPlace`, so
`restore_session_in_place` passes the variant instead of executing the reset
itself, and holds `recovery_gate::worker_target_mutex(session_id)` around the
`restore_into_target` call. Milestone 5 also removes the
`#[allow(dead_code, reason = ...)]` on `WorkerRootReset::InPlace`.

New file `mj-controller/src/controller/resume/in_place.rs` (declare in `resume.rs`):

    impl Controller {
        pub(in crate::controller) async fn restore_session_in_place(
            &mut self,
            session_id: &str,
            profile_id: &str,
            executor: &(impl CommandExecutor + Sync),
        ) -> Result<MaterializedSession>
    }

Steps:

1. Read `previous = record.clone()`. `ensure!` state is `Closing`, `target.is_some()`, checkpoint present. `verify_resume_checkpoint`. Look up the destination profile; `ensure!(profile.enabled)`; `validate_muse_resume_destination`; the Muse mounts check; `preflight_worker_binary(target_template)`. Compute `native_continuity = native_continuity_preserved(profile.kind, manifest.session.harness_kind)`, `discard_queued_prompts = true`, `stored_frontier`/`projection_build` exactly as `resume.rs:935-966`. Compute `previous_profile_root = removable_profile_root(&backend, id, source_profile)` from the *source* profile before mutating the record (`backend` from `backend_locator(record.target, record, config)`).
2. Persist one record transition: `harness_kind = profile.kind`, `last_profile = profile_id`, `native_session_id = native_continuity.then(|| manifest.session.native_session_id)`, `state = Provisioning`, `last_error = None`, `target` unchanged. Use `save_session` (whole row). This is the crash boundary that switches recovery from "finish the close" to "roll back the resume".
3. Under `recovery_gate::worker_target_mutex(session_id)`, `execute_checked(StagedExecutor::new(executor, ProvisionStage::Syncing), in_place_worker_reset_plan(...))`.
4. Call `restore_into_target` with `restore_repositories: false`, `native_continuity`, `discard_queued_prompts: true`, `replay_queue: false`, `install_attached_resources: false` (comment: EC2-only and mounts are unchanged), `worker_root_reset: WorkerRootReset::InPlace { .. }` (Milestone 3's tail must skip its own mkdir/clear when `InPlace`, since the reset already did both), `retire_after_ready: None`, `utility_handoff: None` (Milestone 6 fills it), `harness_home = target_profile_home(&backend, id, &profile)`.
5. On error: mirror the projection restore in `resume.rs:1492-1526`, then return `Err(self.rollback_failed_resume(session_id, &previous, /*recreated_managed_worktree*/ true, error, executor)?)`. Passing `true` makes the rollback retire the still-present managed worktree so `Stopped` keeps its "checkout retired" invariant.

In `execute_move`, after the checkpoint/recovery_session bookkeeping (lines 879-895), branch:

    if operation.in_place {
        self.restore_session_in_place(&id, profile_id, executor).await?;
    } else { /* existing clear_resource_allocation block and resume_session_controlled */ }

Then the existing code records `destination_target`, `destination_native_session_id`, `StartingQueue`, and `admit_move_queue` runs unchanged. In `admit_move_queue` (lines 1001-1006) branch the notice on `operation.in_place`:

    "Switched from {src_profile} / {src_target} to {dst_profile} / {dst_target} in place; the workspace and environment were kept. {queue sentence} The interrupted prompt was not replayed."

Tests (`move_session/tests.rs`), all LocalBare with a recording executor and the fake worker pattern from `move_queue_replay_survives_accept_then_relay_crash_and_rejects_replaced_store`:
- `in_place_move_reinstalls_the_harness_without_removing_the_worker_root`: no executed purpose matches "remove exact" / "stop the local Mjolnir worker and remove" / "podman rm" / "podman run" / "create"; exactly one reset command; `ownership.json` names the new profile; the old `<worker_root>/profile` contents are gone and the new profile's files are present; an untracked file beside the project survives; the persisted operation has `in_place: true` and `destination_target == source_target`; the record ends `Running` with the new `last_profile`.
- `in_place_move_never_removes_a_shared_local_profile_home` (Codex non-private profile on LocalBare: no `rm` names `profile.home`).
- `in_place_move_failure_tears_down_and_leaves_stopped_with_checkpoint` (make `start_worker` fail): `Stopped`, `target == None`, checkpoint retained, managed worktree retired, old profile/harness restored, recovery text "Session is stopped with a verified checkpoint. Retry move or Resume with previous settings."
- Extend `move_source_recovery_retains_data_on_cancellation_or_failed_stop_and_keeps_its_mode` with an `in_place` operation.

### Milestone 6: cross-harness in place

In `restore_session_in_place`, when `!native_continuity`: run the worker-files/upload lanes as the first closure of `execute_joined_cross_harness_work` and `utility_handoff_while_cancellable(session_id, &config, &canonical_session, context_bytes, executor, cancellation)` as the second, exactly like `provision_with_cross_harness_handoff` (`resume.rs:1833-1877`) minus provisioning, then pass the handoff into `restore_into_target` so it calls `install_prompt_context`. Milestone 3 must therefore expose the lanes as a step `restore_into_target` can take pre-run, or `restore_into_target` gains an `Option<CancellationToken>`; choose the smaller change and record it in the Decision Log. `prepare_move_session_controlled` already requires an available utility model for cross-harness moves (lines 480-493), so no new preflight.

Test: `in_place_cross_harness_move_installs_the_handoff_without_provisioning`, using the channel-handshake pattern already in `resume/tests.rs` for cross-harness resumes.

### Milestone 7: recovery

In `recover_move_managed_controlled`:
- The `Closing`/`ClosingSource` arms are unchanged in behaviour (full teardown through `recover_interrupted_close_managed`). When `operation.in_place`, append to the bail text: "the in-place swap was interrupted; the environment was released".
- The `ResumingDestination` non-Running arm (lines 764-769) passes `operation.in_place` as `recreated_managed_worktree` to `rollback_failed_resume`.

Tests modelled on `terminal_move_recovery_finishes_interrupted_close_before_phase_retry`: `in_place_move_recovery_after_restart_during_swap_rolls_back_to_stopped` (persist `ResumingDestination`, `in_place`, session `Provisioning` with a retained LocalBare target; reopen; assert worker root removed, `Stopped`, `target == None`) and `in_place_move_recovery_after_restart_before_swap_finishes_the_close` (session `Closing` with retained target and a fake worker reporting `Closed`; assert teardown, `Stopped`).

### Milestone 8: surfaces, docs, e2e

- `mj-tui/src/wizards/render.rs:614`, `mj-cli/src/main.rs:753`, `mj-controller/src/web/viewer.js:2565`: when `preparation.in_place`, say "Only the harness and profile are replaced; the environment and workspace are kept." instead of "restored into a fresh environment".
- Docs: `docs/src/content/docs/durability.md:147-151,186`, `docs/src/content/docs/web-viewer.md:65`, and the Move paragraph in `docs/src/content/docs/sessions.md`.
- `tests/e2e/session_move.py`: before the profile-only move (`move("fake", "destination", "discard", "profile")`), capture the session's locator and write `checkout/in-place-survivor.txt`; after it assert the locator is identical, the file is intact, `native_session_id` unchanged, `<worker_root>/ownership.json` names `"fake"`, and the daemon log has no "remove exact local Mjolnir worker state" line for the session.
- Live check (needs a Podman host; not available in the lab): `podman ps --format '{{.ID}}'` before and after a profile-only move onto a local Podman target is identical; an untracked file under `/workspace` survives; record the `move phase finished` timings from the daemon log against a target-changing move.

## Concrete Steps

Working directory: `/home/jonathan/Projects/hel4`.

    cargo test -p brokk-mj-controller -- move_session # after each milestone
    cargo test                                        # full suite, outside the sandbox, dev profile
    cargo clippy --all-targets -- -D warnings
    python3 tests/e2e/session_move.py                 # see the file header for prerequisites

Note: the workspace packages are published under `brokk-` names, so the package
selector is `-p brokk-mj-controller`, not `-p mj-controller`.

Commit each validated milestone on the current branch with only the files it changed.

## Validation and Acceptance

- `cargo test` passes with the new tests listed above; `in_place_move_reinstalls_the_harness_without_removing_the_worker_root` fails before Milestone 5 and passes after.
- `tests/e2e/session_move.py` passes with the new assertions.
- A profile-only `mj move --session <id> --profile <other>` on a running session shows "Stopping source", "Preparing destination", then the conversation line "Switched from … in place; the workspace and environment were kept." and no "Cleaning up source" notice. The session's target locator is unchanged.
- A target-changing move behaves exactly as before.

## Idempotence and Recovery

Milestones 1, 3, and 4 are additive. Milestone 2 is not: once `execute_move`
passes `RetainForInPlaceSwap`, an eligible profile-only move seals its source and
leaves the record `Closing` with its target, and the destination half that knows
what to do with that state does not exist until Milestone 5. Between those two
commits a profile-only move on an unchanged target fails at
`resume_session_controlled` with "session is not stopped, lost, or retryable",
and the session is left `Closing` with a verified checkpoint and a live worker
daemon; `recover_interrupted_close_managed` takes it to `Stopped` from there.
Milestones 2 and 5 should therefore be released together, or Milestone 2 committed
with `execute_move` still passing `Destroy`. This ExecPlan previously claimed the
whole sequence was additive until Milestone 5, which was wrong. If the swap fails at any point, `rollback_failed_resume` tears the target down and the session is `Stopped` with its verified checkpoint; `mj resume` or "Retry move" then use the existing fresh path. A daemon restart during the swap resolves the same way through `recover_move_managed_controlled`.

## Artifacts and Notes

After Milestones 1 to 4 (working directory `/home/jonathan/Projects/hel4`; the
workspace packages are named `brokk-mj-*`, so `-p brokk-mj-controller`):

    cargo test -p brokk-mj-controller -- move_session
    test result: ok. 13 passed; 0 failed; 0 ignored; 1331 filtered out

    cargo test -p brokk-mj-core -p brokk-mj-controller
    test result: ok. 1341 passed; 0 failed; 7 ignored     # mj-controller
    test result: ok. 341 passed; 0 failed; 0 ignored      # mj-core

    cargo clippy --all-targets -- -D warnings
    (no output)

The Milestone 2 test discriminates: swapping its disposition to `Destroy` fails it
with "a retained target has no deferred storage cleanup".

New test names, by file:

    mj-controller/src/controller/move_session/tests.rs
        in_place_eligibility_requires_same_target_mounts_and_allocation
    mj-controller/src/database/session_move.rs
        in_place_intent_round_trips_and_a_legacy_row_without_it_reads_as_a_fresh_environment
    mj-controller/src/controller/checkpoint/tests.rs
        an_in_place_move_close_seals_the_source_and_keeps_its_target
    mj-controller/src/targets/tests.rs
        in_place_worker_reset_plan_stops_daemon_clears_relay_state_and_old_profile_home
        removable_profile_root_names_only_per_session_profile_directories

## Interfaces and Dependencies

No new crates. New or changed signatures are listed in the milestones: `SourceTargetDisposition` (lifecycle.rs), `in_place_move_eligible` (move_session.rs), `verify_resume_checkpoint`, `restore_into_target`, `RestoreIntoTarget`, `WorkerRootReset` (resume.rs), `restore_session_in_place` (resume/in_place.rs), `in_place_worker_reset_plan` (targets/worker_daemon.rs), `removable_profile_root` (controller.rs), and the `in_place` fields in `mj-core/src/state/session_move.rs`.

Revision (2026-09-17, Fable): Milestones 1 to 4 are committed with `execute_move` unchanged in behaviour (`Destroy`, unconditional source cleanup) so that no commit on the branch breaks a profile-only move; Milestone 5 wires the `in_place` branch together with `restore_session_in_place`. The `Started` dashboard notice for image downloads in the sibling plan was likewise limited to hosts with no copy of the image.
