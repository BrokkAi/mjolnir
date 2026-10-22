# Preserve session containers across daemon upgrades

This living ExecPlan follows `.agents/PLANS.md`. The user requested an implementation and commit on the current branch, without a release or push. Work is coordinated through https://github.com/BrokkAi/mjolnir/issues/1326, assigned to the author with `agent-in-progress`. Its earlier proposal for another automatic-image recheck is superseded by the user's prohibition on automatic container replacement.

## Purpose / Big Picture

A daemon upgrade must preserve an agent's stateful environment, including files outside checkpoint coverage. A compatible older worker can continue running. Supported worker-binary upgrades still use the existing atomic idle reservation and replace only that binary inside the original container. New sessions can use a new default image. Normal Restart agent always retains its container and filesystem.

A person can explicitly choose Upgrade container and restart… from the session menu or the Restart dialog. The confirmation shows the inspected current image and selected target, including cached immutable image identities, and the retention boundary. Workspace, temporary and build-cache mounts are reattached. Writable-layer files such as manually installed packages may be lost. A checkpoint does not preserve the whole environment. A busy worker finishes accepted work first. Pending requests survive daemon replacement, yield handoff admission while waiting, and can be cancelled before replacement commits.

## Progress

- [x] Trace automatic image policy, worker upgrade outcomes, scheduling, Restart admission, destruction, storage cleanup and recovery ownership.
- [x] Remove automatic container outcomes, recreation scheduling, retry state and image/settings inference from worker upgrades and normal Restart.
- [x] Add explicit preview/request types, schema 91 durable payload and a distinct container-upgrade lifecycle.
- [x] Implement retained-storage replacement without provisioning rollback, volume deletion, checkpoint reconstruction or mbx release.
- [x] Serialize child registration, wake and mutating delegation against replacement commit under the existing database writer.
- [x] Add TUI/web session-menu and Restart alternatives, CLI preview/acknowledgment/cancel and HTTP/daemon APIs.
- [x] Cancel untouched legacy automatic requests; retain checkpoints and recover already-started requests using surviving storage.
- [x] Update user documentation to remove the retired implicit replacement contract.
- [x] Browser golden passes for image confirmation, Cancel, ordinary Restart and explicit upgrade.
- [x] Busy-worker regression using a real relay and actor passes: no replacement commit, accepted prompt unchanged, handoff admission yielded, durable cancellation preserved.
- [x] Real official 2.51.0-to-development daemon replacement preserves the busy older worker, original container and uncheckpointed files; pending upgrade survives handoff and cancellation without interrupting the turn.
- [x] Complete real-container regression after entrypoint/environment handling fixes; real engine test passed and now also covers unsupported layer-only workspace refusal and historical bind-workspace retention.
- [x] Complete end-to-end normal Restart, explicit upgrade and interrupted-worker-readiness recovery. Boxing major nested lifecycle futures fixed the debug Restart stack overflow; the full real scenario passed.
- [x] Complete legacy already-removed-container recovery end-to-end, retaining actual workspace/cache mount contents.
- [x] Complete full touched-crate tests, workspace clippy, UI goldens, final engine and old-release regressions, and review all task-owned diffs.
- [x] Complete final commit preparation on current master and remove the coordination label; this plan is included in the validated implementation commit. No push or installation is authorized.

## Surprises & Discoveries

The original chain was `WorkerUpgradeOutcome::RecreateContainer` -> `spawn_container_recreation` -> Restart's `retire_restart_container` -> `destroy_after_verified_checkpoint`. That cleanup deleted the workspace and temporary volumes and released mbx targets. An idle turn never granted this storage-destruction authority. Normal Restart also inferred replacement from changed or missing applied settings, so removing the upgrade scheduler alone was insufficient.

Real Podman testing established that worker journals and staged profiles live under `/var/lib/hel`, usually in the writable layer, rather than on `/tmp`. Retaining only workspace/cache mounts would therefore lose the active worker journal. Explicit replacement now transfers this private control tree while the worker is stopped, excluding every independent mount and staying on one filesystem. It never restores repository contents from an archive.

Podman may reorder `inspect` mounts; confirmation compares a destination-sorted list. A missing container may return exit 125, so absence is recognized from explicit engine absence diagnostics, never from arbitrary failure. Engine environment must remain the host's: exporting the source container's HOME before running Podman made Podman resolve `/home/hel` on the host. A private env file now carries container variables without changing engine settings or exposing values in argv. Local engine calls read a private temporary env file that stays alive through process completion; SSH calls use the engine's `/dev/stdin` env-file input. Multiline values cannot be represented safely in the common format and are rejected before stopping the worker. Podman also limits an individual assignment to its 64 KiB line buffer: an oversized entry is rejected while the source remains running, and the real-engine regression preserves a supported environment exceeding 80 KiB in total.

`podman untag IMAGE` removes all names for that image. Temporary upgrade pin cleanup uses `podman untag IMAGE PIN_NAME`; Docker cleanup names the unique pin tag rather than the image ID. Unique pins protect prepared images against the existing dangling-image refresher while a busy upgrade waits.

A real old/new-daemon regression reached normal Restart after successfully testing old-worker reconnection, legacy unstarted-intent cancellation, busy upgrade persistence and cancellation. The debug daemon overflowed its stack during checkpoint sealing. Major nested Restart futures are now boxed; the same real path and the full interrupted-upgrade scenario now pass.

## Decision Log

Keep replacement in the existing Restart intent ownership domain, with a separate serialized `RetainedContainer` payload and distinct Runtime lifecycle kind. Historical `automatic` remains read-compatible data only; new daemons never write it. Schema 91 is breaking: old readers would treat an explicit retained-storage intent as authority for destructive recreation. Advance its revision, ledger and compatibility floor in one transaction; preserve every shipped migration.

Do not checkpoint explicit upgrades. The durable journal and private profile/control tree continue the worker, while its mounts retain actual files. Use a stopped source container as a recovery resource until a replacement worker is ready. Ordinary destruction keeps its separate authority to remove storage; replacement commands have no such capability or destructive error traps.

Capture actual engine mounts, user, environment, working directory and supported resource/network settings. Do not regenerate them from edited desired settings. Reject unsupported preservation layouts and engine overrides before accepting. Named volumes and ordinary bind mounts are supported on owned local/SSH Podman and Docker containers. Workspaces stored only in the writable layer, transient Podman overlays, borrowed child containers, Apple containers and unsupported device/privilege/namespace/port settings fail explicitly. Default Docker layer-only workspaces consequently cannot use this path.

Use a database-writer decision for child wake/creation/delegation and replacement commit. An earlier accepted child operation forces deferral. After commit, a parked child cannot wake and a new child cannot register against the storage being replaced. User cancellation and Runtime replacement commit also share the Runtime owner lock.

Keep prepared image identity separate from the consent fingerprint. A preview of an uncached versioned reference can be accepted; preparation records the immutable pulled identity before replacement. A cached mutable tag's preview includes its selected identity and is rechecked before acceptance. Respect `pull_policy=never`; an uncached target fails before acceptance.

## Outcomes & Retrospective

Implementation and validation are complete. Automatic maintenance has no container-replacement outcome or scheduler, normal Restart retains its environment, and the explicit TUI/web/CLI/API path preserves actual workspace/cache mounts. Durable ownership serializes replacement against worker recovery, child admission, cancellation and daemon handoff. Historical destructive intents retain their checkpoint and surviving storage; unknown storage absence or unsupported preservation fails visibly.

The final controller suite passed 2,034 tests with 17 explicitly ignored live-service/engine cases. Core, client, TUI and CLI suites passed, including 529 TUI and 214 CLI tests and 14 daemon-startup integration cases. Workspace clippy, formatting, platform-path classification, web unit tests (57), and TUI/browser goldens passed. The separately enabled real Podman regression passed with injected replacement failure, retained data, unsupported-layout refusal and large environment transport. The official 2.51.0-to-development daemon regression passed all busy-work, cancellation, handoff, normal Restart, interrupted replacement and legacy recovery assertions; final artifacts are `target/reliability-artifacts/container-upgrades-seed-1326-1303335`. Docker was unavailable for a live-engine run; SSH, Windows and macOS engine paths were not exercised live. No incident session, installed binary or live default daemon was changed.

The real engine and official old release found journal-location, environment-transport, mount-order and debug-stack defects that policy tests alone would miss. Validation and coordination-label cleanup are finished. This plan is included in the required current-branch implementation commit; pushing and installation remain outside this task.

## Context and Orientation

`mj-controller/src/controller/worker_restart.rs` owns worker-only replacement. `worker_upgrade.rs` and `daemon/support.rs` coordinate those outcomes and no longer schedule container replacement. `mj-core/src/state/target_runtime.rs` keeps historical applied settings but grants no replacement authority.

`mj-controller/src/targets/container_upgrade.rs` owns engine inspection, retained mounts, image preparation, private control-state transfer, source/backup/replacement identity and legacy removed-target creation. `controller/container_upgrade.rs` combines that plan with `WorkerPermit`, `IdleWorkspaceLease`, durable worker boot intents and readiness. `daemon/container_upgrade.rs` handles preview and explicit acceptance. `daemon/restart.rs` resumes the durable operation and handles ordinary Restart and historical recovery. `database/session_restart.rs` stores the payload and serializes commit/cancel; child startup/delegation writes consult the same writer.

Public preview/request types are in `mj-core/src/state/container_upgrade.rs`. Daemon transport and client methods are in `mj-client/src/daemon.rs`; supervised preview work uses the existing client-state request pipeline. HTTP routes are in `server/api/turns.rs` and `server/handlers.rs`. TUI actions/dialogs and web viewer actions use the same preview and acknowledgment. CLI `mj upgrade-container --session ID` previews, `--acknowledge-writable-layer-loss` queues, and `--cancel` cancels pending work.

## Plan of Work and Milestones

### Milestone one: remove implicit replacement

Worker upgrade outcomes contain only AlreadyCurrent, Upgraded and Deferred. Supported compatible workers reconnect without image inspection deciding container policy. Delete obsolete coordinator scheduling/retries rather than leaving a disabled second path. Normal Restart ignores old replacement-acknowledgment booleans and never falls back from a missing container to provisioning a new one. Keep bare-target historical fallback where its existing contract permits it.

Acceptance is the current-worker/older-image regression, the actual official older-worker handoff, and ordinary Restart retaining the same container ID and manually installed writable-layer marker after desired image/settings change.

### Milestone two: explicit durable retention and admission

Before acceptance, inspect ownership and actual mounts; require a separately mounted writable workspace and supported engine configuration. Store the source immutable container ID, original managed name, operation-specific backup name, target image/identity, actual mounts and runtime settings. Use schema 91's `container_upgrade_json` column in `session_restart_intents`. The initial historical phase name `stopping` means pending for a new explicit request; `recreating_container` means replacement committed. New explicit intents reach that phase only through `try_commit_container_upgrade`, never generic checkpoint-phase advancement.

Preparation and idle waiting release daemon handoff admission using `begin_resumable_work`. Immediately before taking an atomic worker reservation, reacquire bounded admission. The worker's idle barrier, not an independently computed UI busy flag, decides whether accepted work is safe to stop. After stopping, copying control state, preparing a binary and waiting for readiness release admission again. Pending relay waits poll cancellation so handoff can join promptly. Persist preparation and state-restored flags before the operation they authorize can admit newer work.

Acceptance is real busy work across queue/cancel/handoff, child-wake ordering, failed-preparation cancellation and recovery of the same durable request.

### Milestone three: replacement and recovery

After commit, stop only the original worker, transfer its private control tree to an operation-specific staging path on retained storage, stop the source container and rename it to the unique backup. Verify named volumes still exist; create the replacement under the original managed name with exact retained mounts and immutable target image. No close plan, `rm -v`, volume deletion, cache release or provisioning rollback is reachable here.

Capture uses unique temporary archive names so a disconnected capture cannot overwrite a newer retry. Restore uses a retained lock and completion marker; late extractors wait and then skip rather than overwriting a journal that has advanced. Persist `control_state_restored` before starting the worker. Positive worker death is required before starting another worker. A live recovered replacement is observed without replaying its prior archive or resetting its root. Retain the stopped source until native readiness, then remove only that container and completed staging archives. Keep zero-byte restore lock/marker files so delayed extractors cannot replay.

A committed failed operation is retried with its same durable plan. An unrelated lifecycle cannot abandon its retained backup; explicit destruction may supersede it by retiring the stopped backup before normal storage destruction. An explicit request that failed before commit remains visible and cancellable after result publication.

Historical automatic `stopping` intents on running/disconnected sessions are cancelled at startup. A historical `recreating_container` row does not authorize deleting its still-present source. Reconnect any live worker first; otherwise restore into the retained container. If old teardown already removed the container, verify its bound checkpoint, verify surviving volume labels and reattach surviving workspace/cache storage. Restore repository contents only if that workspace was positively absent. Never mistake an engine inspection error for storage absence. Avoid recursive workspace chown on retained storage. Preserve the checkpoint and intent on recovery failure.

Acceptance is a real engine failure after source rename, daemon death at that boundary, daemon death after the replacement has accepted a new prompt, and historical removed-container recovery retaining files outside checkpoint coverage.

### Milestone four: user surfaces and final validation

Expose distinct menu/Restart choices across TUI and web, with Cancel initially selected. Show current/target versions and cached image IDs, retention boundary and writable-layer warning before acknowledgment. CLI/API use the same inspected selection, validate acknowledgment and persist acceptance before answering. Run preview I/O in supervised background work, never on the render/event loop. Existing lifecycle views show pending progress and cancellation.

Update the existing TUI/browser golden cases rather than adding small implementation-mirroring tests. Update public container, session, target, web and API documentation. Complete final touched-crate suites and workspace clippy once after the last code fixes. Commit only this task's files on current master, without pushing.

## Concrete Steps and Validation

All commands run from `/home/jonathan/Projects/mjolnir/.mj/clones/147484ae9f241f60935bba15b7bde8cb` unless a different directory is stated. Use the existing mbx Cargo wrapper and managed target location; never redirect builds. Run every Cargo test outside the restricted sandbox. New binaries use a named isolated instance and private `MJ_CONFIG_DIR`/`MJ_DATA_DIR`; never install or point them at the live default store.

Build the real regression binaries:

    cargo build -p brokk-mjolnir --features test-hooks
    cargo build --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker

The downloaded official v2.51.0 Linux release and bundled portable worker are verified by its published SHA256 under `/home/jonathan/.cache/mjolnir/build/.agents/docs/container-upgrade-regression/v2.51.0`. They are test artifacts, not installed binaries. With cached Podman 2.51.0 and 2.52.0 images:

    python3 tests/e2e/container_upgrades.py --mj target/debug/mj --worker target/x86_64-unknown-linux-musl/debug/mj-worker --old-mj /home/jonathan/.cache/mjolnir/build/.agents/docs/container-upgrade-regression/v2.51.0/mj --instance container-upgrade-regression

The driver creates its own Git server, deterministic ACP fixture, profile, store and labelled Podman resources. It verifies old/new daemon handoff, old-worker compatibility, outside-checkpoint files and caches, legacy pending-intent cancellation, new-session image selection, busy queue/handoff/cancel, normal Restart, source-retained interruption, interruption after new work, and historical already-removed-container recovery. Cleanup stops only its daemon and removes its containers before their volumes. Artifacts live under `target/reliability-artifacts/container-upgrades-*`.

Focused controller tests include:

    MJ_INSTANCE=container-upgrade-regression cargo test -p brokk-mj-controller --lib container_upgrade -- --include-ignored
    MJ_INSTANCE=container-upgrade-regression cargo test -p brokk-mj-controller --lib container_replacement
    MJ_INSTANCE=container-upgrade-regression cargo test -p brokk-mj-controller --lib failed_container_preparation

Update/verify the existing TUI golden and browser golden:

    MJ_INSTANCE=container-upgrade-regression MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-tui --lib -- --exact dialogs::tests::golden_tui_container_editor_details
    cd tests/e2e/web
    MJ_UPDATE_GOLDEN=1 npx playwright test new-session.spec.js --project deterministic --grep golden_viewer_new_session

After the last fixes, run full suites for touched core, client, controller, TUI and CLI crates, workspace `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `git diff --check` and applicable web unit checks. If a test fails, rerun only that failed case after its fix; do not repeat already-passing full suites without another relevant change.

## Idempotence and Recovery

Only the durable operation identity may reuse a replacement. Before mutations, inspect exact managed labels and source/replacement identities. Missing/inaccessible storage or changed identity fails visibly. Pin image identity while waiting. A newer daemon never inherits an earlier idle observation: if the original worker remains live, acquire its atomic reservation again. If a replacement is live, do not restore older state or start another worker. Retain the original checkpoint for historical recovery until the session is usable. Historical migration tests include interrupt/reopen boundaries and compatibility rejection; schema 91 prevents older destructive daemons from opening the new store.

## Artifacts and Notes

Earlier plan `.agents/plans/issue-1139-versioned-agent-dev-images.md` documents the retired automatic recreation policy and is superseded on that point. The persistent `container-mount-recreation.md` memory also needs updating at completion. Do not change the incident session or host default daemon as a test.

Revision note (2026-10-21): Rewrote the initial investigation plan around the implemented retained-storage design, real-engine discoveries, completed targeted evidence and outstanding validation. Earlier checkpoint-based explicit-upgrade proposals are superseded: explicit replacement transfers private control state and retains actual mounts.

Revision note (2026-10-21): The full real old/new-release regression passed with handoff in 0.36 seconds: artifacts `target/reliability-artifacts/container-upgrades-seed-1326-545803`. Added a regression for a recovered committed replacement failing before its helper is reentered; lifecycle admission now restores commit authority from the durable phase before exposing the operation, and unrelated lifecycle exclusion consults that same control. Daemon protocol 58 protects the renamed feed fields and new actions without changing worker protocol. The first full run passed client, core and controller suites; expected help/menu goldens were updated, while instance-specific port-only golden changes were reverted. Final suites use explicit private configuration/data roots and existing per-test named instances so render fixtures keep their default port without reaching live data. Final lint cleanup and historical bind-workspace preservation are being checked.

Revision note (2026-10-21): Completed retained-environment ownership review. A durable explicit intent now suppresses background worker recovery, worker-only maintenance and checkpoint copies, and guards missing-target writes inside the database transaction even after lifecycle failure. The Runtime also avoids queuing that stale write while its retained intent is present. The recovery regression proves these paths cannot mutate or discard the retained environment. Child admission uses `SessionState::has_live_worker` plus retained targets under the same database writer, including a registered child's provisioning interval before its target is assigned; the Controller's duplicate child-state precheck was removed. Final controller/CLI reruns are in progress. One unrelated PTY color assertion inherited `NO_COLOR=1`; rerun that fixture with NO_COLOR removed while retaining its explicit isolated config/data and named instance.

Revision note (2026-10-21): Recorded final green suites and real-release/crash evidence. Scoped container-upgrade cancellation now checks lifecycle kind under the owner lock so a late cancellation cannot stop a later operation; successful pending cancellation leaves no false session error. Local private-file and SSH stdin environment transport preserve engine settings, and oversized individual env entries fail preflight. Both platform CI path lists cover the new engine module. Legacy recovery deliberately never overwrites surviving workspace data with checkpoint contents; failures retain the archive and durable intent. Final review found exactly the 84 task-owned paths from the reviewed manifest, with no unrelated edits.
