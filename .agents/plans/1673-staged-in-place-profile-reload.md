# Staged In-Place Profile Reload for Same-Target Moves (#1673)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md`.


## Purpose / Big Picture

When a user moves a live session to another profile or harness on the same target ("in-place Move", such as switching from Claude to Codex or rotating to another account while keeping the session's container, workspace, and user shells alive), the current code overwrites the session's live staged profile directory, `launch.json`, and `ownership.json` before the worker's ACP runtime has accepted the reload, and runs worker-side profile configurators against that live directory before retiring the running harness. If the reload is then refused (for example because a turn started or preparation failed) or interrupted, the still-running harness can end up reading files from a launch that never happened, and error recovery relies on fragile best-effort restore and string heuristics (`RestoreProfileOnly`, `rpc_sent`, substring checks on `"already in progress"`, and `source_disk_confirmed_intact`).

After this change, an in-place Move uploads the candidate profile files, `launch.json`, `ownership.json`, and a small overlay manifest into a private per-attempt directory beneath the session's worker root (`<worker_root>/reload-candidates/<candidate_id>/`), touching no live file while the old harness is running. The worker performs slow private preparation (login shell resolution, private Git generation, project-tool paths, managed harness and Node runtime resolution) without mutating the live profile home or live root files. Only after the idle ACP runtime accepts the reload and reaps the old harness bridge does the worker cross a single atomic commit point (`<worker_root>/pending-reload.json`), apply the candidate overlay into the stable native profile home, atomically replace `ownership.json` and `launch.json` (with `0600` permissions), publish the Git setup generation, run the destination harness's home configurators, write `acp-supervisor.json`, and launch the replacement bridge. If the worker crashes anywhere after the commit point, worker boot rolls forward idempotently from `pending-reload.json` before reading `launch.json`. If the replacement harness fails to start after commit, the worker records a typed post-commit startup failure (`RelayErrorDetail::ReloadFailedAfterCommit` and `WorkerHarnessStartupFailure` in `worker-exit.json`) instead of returning `InvalidState`, and the controller settles the session on the destination profile in `SessionState::Error` so the user can move or restart cleanly without corrupting live disk or mis-routing credential sync.

A user or test can observe this working in four ways: rejecting an in-place Move while a turn is active or during pre-acceptance preparation leaves live `launch.json`, `ownership.json`, and every file in the live staged home byte-identical to before the attempt; a successful in-place Move across same-harness or cross-harness profiles (including OpenCode login replacement and preserving native session history in stable homes) applies the destination profile cleanly and cleans up the candidate directory; killing a worker mid-apply after `pending-reload.json` exists rolls forward to the destination profile cleanly on boot; and a replacement harness that fails to open after commit lands the session on the destination profile in `Error` state rather than restoring source profile files over a retired bridge.


## Progress

- [x] (2026-11-24 18:50Z) Claimed GitHub issue #1673, inspected Advisor review (`profile-move-review.md`), recent upstream reload commits (`fdd2bb564`, `54b5ef09b`, `5a1e73499`), and all target files; authored this ExecPlan.
- [x] (2026-11-24 19:04Z) Milestone 1: Atomic `WorkerLaunchConfig::write` (`0600` temp+rename), Relay Protocol 91 wire types (`RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL`), durable `InPlaceReloadPhase` on `InPlaceMoveHold`, `RelayErrorDetail::ReloadFailedAfterCommit`, and controller protocol capability gate in `mj-controller/src/controller/move_session/in_place.rs`.
- [x] (2026-11-24 19:04Z) Milestone 2: Private candidate staging (`WorkerFilePreparation::StageCandidate`, shared `profile_clear_spec`, `install_reload_candidate`, `remove_reload_candidate`) in `mj-worker-staging` and `mj-worker-launch` with zero live-path writes (keeping `RestoreProfileOnly` temporarily until Milestone 4 deletes it with `move_session.rs`).
- [x] (2026-11-24 19:34Z) Milestone 3: Worker-side two-phase reload, atomic `pending-reload.json` WAL + boot roll-forward (before `bootstrap_login_environment` reads `launch.json`), post-retirement overlay + configurator execution inside `HarnessRestarting` (`apply_harness_reload_transition`), and typed post-commit startup failure wiring (`SessionConfiguration::OpeningReload` -> `ReloadFailedAfterCommit` + `WorkerHarnessStartupFailure`).
- [x] (2026-11-24 20:30Z) Milestone 4: Fixed relative `target_profile_home` resolution in `reload_commit.rs` for `SshBare`/`AwsEc2`, moved relay admission ahead of candidate validation/writer acquisition/ACP check with typed `RelayErrorDetail::ReloadInProgress`, updated controller in-place Move state machine in `mj-controller/src/controller/move_session.rs` (query worker `in_place_move_hold` before staging on retry, send `StageCandidate` + `RestartHarnessInPlace`, deleted live `RefreshProfileOnly`/`RestoreProfileOnly`/`rpc_sent`/substring/`source_disk_confirmed_intact` heuristics, settled destination `Error` on post-commit startup failure while keeping credential-sync fenced until DB matches disk, settled cancelled-after-`Started` moves to B with recent-snapshot handoff and no new continuation, preserved hold recovery if `release_in_place_move_dispatch` disconnects), deleted `RestoreProfileOnly` from `mj-worker-staging`/`mj-worker-launch`, and validated all affected crates.
- [ ] Milestone 5: Pre-push review, push to `origin/master`, and remove `agent-in-progress` on #1673 (the three orthogonal follow-up issues are already filed as #1698, #1699, and #1700).


## Surprises & Discoveries

- Observation: Commit `fdd2bb564` ("Order in-place harness reload transition at HarnessRestarting") already introduced `RuntimeEvent::HarnessRestarting { reload: Option<HarnessReload> }` with a oneshot acknowledgement (`applied`) in `mj-worker/src/acp.rs:572-617` and `mj-worker/src/worker_runtime/unix/dispatch.rs:979-1040`.
  Evidence: `acp::run_inner` reaps the old bridge, terminals, and guardian completely, then emits `HarnessRestarting` and waits on the coordinator's oneshot ack before writing nothing more and entering `run_bridge` for the replacement harness. This gives us the exact serialized post-retirement, pre-replacement-spawn slot needed for the commit point and home overlay.

- Observation: `mj-worker-bin/src/main.rs:456-483` (`bootstrap_login_environment`) reads `WorkerLaunchConfig::read(config)` before re-execing the worker with a clean login environment and before `mj-worker` starts the relay runtime.
  Evidence: If a worker crashes after committing `pending-reload.json` and before atomically renaming `launch.json`, boot roll-forward must run in `mj-worker-bin/src/main.rs` at the start of `WorkerCommand::Run` before `bootstrap_login_environment` reads `launch.json`, so the login-environment re-exec reads the destination `launch.json` rather than the superseded source `launch.json`.

- Observation: `WorkerFilePreparation::RefreshProfileOnly` has two stopped-worker callers outside live in-place Move (`mj-controller/src/controller/resume/repair.rs:128` and `mj-controller/src/controller/subagent_park.rs:529`), whereas `WorkerFilePreparation::RestoreProfileOnly` is called only from `mj-controller/src/controller/move_session.rs:3571`.
  Evidence: Grep across `mj-controller/src`, `mj-worker-staging/src`, and `mj-worker-launch/src` confirms `RestoreProfileOnly` can be deleted completely while `RefreshProfileOnly` stays strictly for stopped-worker repair and unpark.

- Observation: OpenCode's `ImportedLogins::prepare` (`mj-worker/src/acp/opencode_logins.rs:35-63,107-196`) already runs in `run_bridge` (`mj-worker/src/acp.rs:811-820`) right after `HarnessRestarting` acknowledges and before spawning `opencode acp`, and `reconcile_database_logins` deletes mismatched provider credential rows from `.data/opencode/opencode.db` before `opencode auth import`.
  Evidence: Verified against `mj-worker/src/acp/opencode_logins.rs` and recorded in `.mj/agents/e366148c7b9b8faa8bbf8588c9aea33a/prereq_checks.md`. No extra OpenCode database wipe is needed as long as B's `mjolnir-credentials.json` is overlaid before `run_bridge` runs.

- Observation: `InPlaceMoveHold` is already persisted in `RelaySnapshot` and returned in `RelayOperationalState` on `Status` (`mj-core/src/relay/snapshot.rs:744-755`).
  Evidence: Adding an optional `reload_phase: Option<InPlaceReloadPhase>` field with `#[serde(default, skip_serializing_if = "Option::is_none")]` gives the controller an authoritative, queryable commit/outcome state on retry without adding a new status RPC or bumping the relay snapshot schema revision.

- Observation: `prepare_worker_files_for` previously called `prepare_installed_managed_harness`, which reads live `launch.json`, and the worker runtime library package is `brokk-mj-worker-runtime` while `brokk-mj-worker` is the binary-only package (`mj-worker-bin`).
  Evidence: Milestone 2 skips `prepare_installed_managed_harness` for `StageCandidate` so live `launch.json` is never read/assumed to be B prematurely, letting worker Phase A resolve the candidate harness; Milestone 3/4 tests target `-p brokk-mj-worker-runtime` and `-p brokk-mj-worker`.

- Observation: `SshBare` and `AwsEc2` workers resolve relative `target_profile_home` paths against the worker startup `current_dir` (matching `initialize_launch_environment`), so `reload_commit::validate_candidate` cannot demand `Path::is_absolute()`.
  Evidence: Milestone 4 relaxed `validate_candidate` to allow safe relative paths (normal components only, no `..`) and resolved them via `std::env::current_dir()?.join(home)` in `apply_committed_reload`.

- Observation: Running candidate validation, `PreparationWriter::try_acquire`, or the missing-ACP check before `admit_in_place_harness_restart` would misreport a retry after commit (when the candidate directory has already been consumed or ACP has exited) as a pre-commit `InvalidState` with `detail: None`.
  Evidence: Milestone 4 moved `admit_in_place_harness_restart` to the very start of `restart_harness_in_place_response` and added typed `RelayErrorDetail::ReloadInProgress { operation_id }` so retries after commit never look like pre-commit refusals.

- Observation: A cancelled in-place Move recovering after the worker already reached `InPlaceReloadPhase::Started` cannot roll back live disk (B is already running), and releasing the dispatch hold without settling DB or installing cross-harness handoff would leave DB on A and drop conversation context.
  Evidence: Milestone 4 updated `recover_move_managed_controlled`'s cancellation branch to query the live worker hold first: pre-commit releases the hold and clears the fence; `Committed`/unreachable retains the fence; `FailedAfterCommit` settles B in `Error`; `Started` settles DB to B, installs deterministic `render_recent_snapshot` handoff if cross-harness and not yet installed, releases the hold without submitting a new continuation or discarding queued user prompts, and clears the fence.


## Decision Log

- Decision: Keep stable native profile homes and apply a pre-uploaded private candidate overlay after A is reaped (Advisor Option B with an earlier worker commit point), rather than switching each harness to a brand-new per-attempt home directory.
  Rationale: All eight harnesses store native conversation history, session databases, or workspace registries under their profile home (`mj-checkpoint/src/checkpoint/native_scan.rs:101-149`), and the checkpoint walker skips symlinks (`native_scan.rs:501-523`). Keeping the stable home preserves native continuity across all eight harnesses without an eight-harness migration split, while staging into `<worker_root>/reload-candidates/<candidate_id>/` and overlaying only after A is reaped guarantees zero mutation of live files on any refusal or pre-commit failure.
  Date/Author: 2026-11-24 / Coordinator (reviewed by Advisor)

- Decision: Use an atomic `<worker_root>/pending-reload.json` write-ahead log (WAL) as the single durable commit point, deleting `pending-reload.json` before deleting `<worker_root>/reload-candidates/<candidate_id>/`.
  Rationale: A multi-file overlay plus `ownership.json`, `launch.json`, and `current-git-setup` cannot be made atomic in one filesystem syscall. Writing `pending-reload.json` atomically (`O_CREAT | O_EXCL` temp file, mode `0600`, `sync_all`, `rename`) after A is reaped establishes one indivisible commit point: any crash before that rename leaves live A intact; any crash after that rename leaves the candidate directory intact until `pending-reload.json` is removed, so boot roll-forward is 100% idempotent across every instruction boundary.
  Date/Author: 2026-11-24 / Coordinator

- Decision: Gate staged in-place reload on a new Relay Protocol 91 (`RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL = 91`) rather than Protocol 81 (`RELAY_IN_PLACE_MOVE_PROTOCOL`).
  Rationale: Both pre-change Rust workers and the Go worker answer Protocol 81 without candidate-directory commit support. Gating on Protocol 91 lets `ensure_worker_supports_in_place_move` cleanly refuse manual Moves (telling the user to run `mj restart`) and skip automatic quota Moves on older workers instead of sending an unsupported candidate-dir reload.
  Date/Author: 2026-11-24 / Coordinator (reviewed by Advisor)

- Decision: Record `accepted_reload_operation_id: Option<String>` on `WorkerLaunchConfig` (inside `launch.json`) and `reload_phase: Option<InPlaceReloadPhase>` on `InPlaceMoveHold` (inside `relay_state.json`), and return `RelayErrorDetail::ReloadFailedAfterCommit { operation_id }` (with `RelayErrorCode::Internal`) when B fails to start after commit.
  Rationale: This eliminates all three fragile heuristics in `move_session.rs` (`rpc_sent`, substring matching on `"already in progress"`, and `source_disk_confirmed_intact`). Whether the worker is alive (`Status` -> `in_place_move_hold.reload_phase`), returned an error (`ReloadFailedAfterCommit` vs pre-commit `InvalidState`), or exited with `worker-exit.json` (`WorkerHarnessStartupFailure` plus `launch.json`'s `accepted_reload_operation_id`), the controller has a single authoritative answer for whether B committed.
  Date/Author: 2026-11-24 / Coordinator

- Decision: Keep the post-commit failure recovery focused on data loss, crash, hang, security, and wrong-account invariants, and defer three orthogonal bugs uncovered by Advisor review to separate follow-up issues (Codex harvest before A is reaped; non-Codex refreshable grants not harvested; ACP vs controller native-continuity predicate on same-harness Codex provider switches).
  Rationale: Keeps #1673 tightly scoped to atomic staged reload ownership and prevents scope creep into credential-harvesting and continuity refactors.
  Date/Author: 2026-11-24 / Coordinator (per Advisor guidance)


## Outcomes & Retrospective

Milestones 1 through 4 are complete and validated across `brokk-mj-core`, `brokk-mj-worker-staging`, `brokk-mj-worker-launch`, `brokk-mj-worker-runtime`, `brokk-mj-worker`, `brokk-mj-session-runtime`, `brokk-mj-controller`, and `brokk-mj-daemon`. All live-path pre-acceptance writes (`RefreshProfileOnly` during live Move, `RestoreProfileOnly`, `rpc_sent`, `"already in progress"` string matching, and `source_disk_confirmed_intact`) have been eliminated in favor of private candidate staging under `<worker_root>/reload-candidates/<candidate_id>/`, atomic `<worker_root>/pending-reload.json` WAL roll-forward, post-retirement overlay and configurator execution inside `HarnessRestarting`, typed relay errors (`ReloadInProgress` and `ReloadFailedAfterCommit`), and durable `InPlaceReloadPhase` recovery settlement. Three orthogonal issues identified during design review were filed separately as #1698, #1699, and #1700. Final pre-push review and push remain in Milestone 5.


## Context and Orientation

In Mjolnir, the daemon (`mj-daemon` driving `mj-controller`) is the control plane and each session's worker (`mj-worker-bin` running `mj-worker`) is the data plane. A session's worker root (`<worker_root>`) holds the worker executable (`hel`), the persisted launch descriptor (`launch.json`), the environment ownership marker (`ownership.json`), the relay journal and snapshot (`relay_state.json`), the ACP supervisor descriptor (`acp-supervisor.json`), the published Git setup pointer (`current-git-setup`), and the session's exit record (`worker-exit.json`). The harness's staged profile directory (`<target_profile_home>`) holds allowlisted configuration, credentials, skills, rules, and instructions alongside the harness's own native conversation history (such as Codex rollouts, Claude project JSONL transcripts, or OpenCode's `.data/opencode/opencode.db`).

An in-place Move switches a live session from source profile A to destination profile B without stopping the worker process or recreating its container/workspace. Today, the flow spans six crates:

1. `mj-core`: defines `WorkerLaunchConfig` (`mj-core/src/worker_launch.rs`, whose `write` method at lines 637-648 currently truncates in-place and sets `0600` after writing), relay protocol versions (`mj-core/src/relay.rs:199-224`, currently max Protocol 90), relay request/response/error wire shapes (`mj-core/src/relay/protocol.rs:327-331,1100,1263-1288`), and `InPlaceMoveHold` (`mj-core/src/relay/snapshot.rs:744-755`).
2. `mj-worker-staging`: owns profile allowlists, refresh-token stripping (`mj-worker-staging/src/worker_binary/harness.rs`), `WorkerFilePreparation` (`mj-worker-staging/src/worker_binary/launch.rs:14-30`), `clear_profile_configuration` (`mj-worker-staging/src/worker_binary/install.rs:1172-1220`), and multi-transport upload `install_worker_profile_files` (`mj-worker-staging/src/worker_binary/install.rs:920-1168`, covering `LocalBare`, `SshBare`, `AwsEc2`, `LocalPodman`, `LocalDocker`, `AppleContainer`, `SshPodman`, `SshDocker`, `SshAppleContainer`, and `SshNitro`).
3. `mj-worker-launch`: builds `launch.json`, `ownership.json`, staged profile directory, instructions, project memory, and AWS credentials in `prepare_worker_files_for` (`mj-worker-launch/src/worker_launch.rs:190-460`).
4. `mj-worker`: implements the relay state machine (`mj-worker/src/relay/commands.rs:637-760`), the Unix worker runtime (`mj-worker/src/worker_runtime/unix.rs:801-1188` for `restart_harness_in_place_response`, and `2722-3290` for `prepare_harness_environment`), the ACP bridge loop (`mj-worker/src/acp.rs:560-840`), the event coordinator (`mj-worker/src/worker_runtime/unix/dispatch.rs:965-1115`), and `exit_record.rs`.
5. `mj-worker-bin`: boots the worker (`mj-worker-bin/src/main.rs:446-520` for `bootstrap_login_environment` and `650-730` for `run_worker` and `worker-exit.json` recording).
6. `mj-controller`: orchestrates in-place Move (`mj-controller/src/controller/move_session.rs:3126-3705`) and checks worker protocol compatibility (`mj-controller/src/controller/move_session/in_place.rs:1-62`).

Important repository rules for every milestone: use `mbx` (`mbx test`, `mbx clippy`, `mbx check`) rather than plain `cargo` because `target` is an `mbx` symlink; never edit `go/`; do not add `Hard-won` comments to new feature tests; keep `mj-worker/src/worker_runtime/project_tools.rs` untouched (it is already unformatted on base).


## Plan of Work

Work proceeds in four independently verifiable implementation milestones, each leaving the workspace compiling and passing tests.

### Milestone 1: Atomic Launch Writes and Relay Protocol 91 Contract

Update `mj-core/src/worker_launch.rs` so `WorkerLaunchConfig::write` writes to a unique temporary file in the destination directory created with mode `0600` on Unix (`OpenOptions::new().write(true).create_new(true).mode(0o600)`), flushes/syncs, and atomically renames onto `path` (cleaning up the temp file on any error). Add `#[serde(default, skip_serializing_if = "Option::is_none")] pub accepted_reload_operation_id: Option<String>` to `WorkerLaunchConfig` so `launch.json` itself records the committed Move operation ID when written by an in-place reload.

Define the staged candidate manifest type in `mj-core/src/worker_launch.rs` (or a new submodule `mj-core/src/reload_candidate.rs` re-exported from `mj-core`) so both `mj-worker-staging`/`mj-worker-launch` (the writer) and `mj-worker`/`mj-worker-bin` (the applier) share one exact schema:

- `RELOAD_CANDIDATES_DIR: &str = "reload-candidates"`
- `PENDING_RELOAD_FILE: &str = "pending-reload.json"`
- `RELOAD_CANDIDATE_MANIFEST_FILE: &str = "manifest.json"`
- `ReloadCandidateManifest`: carries `operation_id: String`, `expected_store_id: String`, `candidate_id: String`, `harness: HarnessKind`, `target_profile_home: String`, `clear_paths: Vec<String>`, `protected_paths: Vec<String>`, `shared_login: Option<ReloadSharedLogin>` (holding `relative_marker: String` and `source_path: String` for `LocalBare` shared symlinks), and `clear_github_token: bool`.
- `PendingInPlaceReload`: carries `operation_id: String`, `expected_store_id: String`, `candidate_id: String`, and `git_setup_name: Option<String>`.

Bump `mj-core/src/relay.rs` to Protocol 91 by adding `pub const RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL: u32 = 91;` and updating `RELAY_PROTOCOL_VERSION` to `RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL`. In `mj-core/src/relay/protocol.rs`, add `#[serde(default, skip_serializing_if = "Option::is_none")] candidate_id: Option<String>` to `RelayRequest::RestartHarnessInPlace`, and add `ReloadFailedAfterCommit { operation_id: String }` to `RelayErrorDetail`. Keep `RelayRequest::RestartHarnessInPlace`'s minimum protocol check compatible with Protocol 81 on deserialization while validating on the worker that Protocol 91 requests carry a non-empty, path-safe `candidate_id` (single path component, no `/` or `..`).

In `mj-core/src/relay/snapshot.rs`, add `InPlaceReloadPhase` (`Committed`, `Started`, `FailedAfterCommit { message: String }`) and `#[serde(default, skip_serializing_if = "Option::is_none")] pub reload_phase: Option<InPlaceReloadPhase>` to `InPlaceMoveHold`.

In `mj-controller/src/controller/move_session/in_place.rs`, gate `ensure_worker_supports_in_place_move` on `RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL` (91) so older workers (including Protocol 81 Rust workers and the Go worker) get the existing clean refusal ("Restart the agent first...") for manual Moves and clean skip notice for automatic quota Moves. Update `mj-session-runtime/src/worker_client.rs` (and any `restart_harness_in_place` client helper) to pass `candidate_id: Option<String>`.

### Milestone 2: Private Candidate Staging in `mj-worker-staging` and `mj-worker-launch`

In `mj-worker-staging/src/worker_binary/launch.rs`, remove `WorkerFilePreparation::RestoreProfileOnly` and add `WorkerFilePreparation::StageCandidate { operation_id: String, expected_store_id: String, candidate_id: String }`. Keep `WorkerFilePreparation::RefreshProfileOnly` solely for stopped-worker repair (`resume/repair.rs`) and parked sub-agent unpark (`subagent_park.rs`).

In `mj-worker-staging/src/worker_binary/install.rs`, factor the allowlist/protected-path computation out of `clear_profile_configuration` into a shared pure helper `profile_clear_spec(harness: HarnessKind) -> (Vec<String>, Vec<String>)` so both `clear_profile_configuration` and `ReloadCandidateManifest` use the exact same allowlist and harness-owned skill protection list (never drifting apart). Add a small helper `install_reload_candidate(...)` that uploads `launch.json`, `ownership.json`, `manifest.json`, and `profile/` into `<worker_root>/reload-candidates/<candidate_id>/` using the existing `LocalBare`, SSH, and Container transfer paths (reusing `install_worker_profile_files` for `launch.json`, `ownership.json`, and `profile/`, plus uploading `manifest.json` alongside them in the same candidate directory and omitting live `clear_profile_configuration` and live `shared_login` symlink creation — recording `shared_login` in `manifest.json` instead). Also add a small best-effort cleanup helper `remove_reload_candidate(executor, locator, worker_root, candidate_id)` that removes only `<worker_root>/reload-candidates/<candidate_id>` on pre-commit refusal.

In `mj-worker-launch/src/worker_launch.rs`, update `prepare_worker_files_for` to handle `WorkerFilePreparation::StageCandidate { operation_id, expected_store_id, candidate_id }`: set `launch.accepted_reload_operation_id = Some(operation_id.clone())`, build the staged profile and `launch.json`/`ownership.json` in the local tempdir as usual, stage any `mbx` build-cache shim (additive, does not alter A's config), write `manifest.json` with `profile_clear_spec(profile.kind)`, `target_profile_home`, `shared_login`, and `clear_github_token: session.no_github_credentials`, and call `install_reload_candidate` targeting `<worker_root>/reload-candidates/<candidate_id>`. Live `<worker_root>/launch.json`, `<worker_root>/ownership.json`, and `<target_profile_home>` must remain completely untouched. Remove the `RestoreProfileOnly` branch from `aws_credentials::source_for_staged_profile` usage in `prepare_worker_files_for` if unused elsewhere (keep `source_for_staged_profile` if repair/unpark uses it).

### Milestone 3: Worker Two-Phase Reload, Crash-Safe WAL Roll-Forward, and Typed Failure Wiring

Create a focused module `mj-worker/src/worker_runtime/reload_commit.rs` (owned by `mj-worker`) implementing the target-side candidate validator, atomic WAL commit, idempotent overlay application, and orphan cleanup:

- `validate_candidate(root: &Path, candidate_id: &str, operation_id: &str, expected_store_id: &str) -> Result<ValidatedCandidate>`: validates `candidate_id` is a single safe filename component, reads and validates `<root>/reload-candidates/<candidate_id>/manifest.json`, verifies `manifest.operation_id == operation_id` and `manifest.expected_store_id == expected_store_id`, and checks that `launch.json`, `ownership.json`, and `profile/` exist inside the candidate directory.
- `commit_pending_reload(root: &Path, pending: &PendingInPlaceReload) -> Result<()>`: atomically writes `<root>/pending-reload.json` (temp file created with mode `0600`, `sync_all`, `rename` — never truncating in place).
- `apply_committed_reload(root: &Path, pending: &PendingInPlaceReload) -> Result<AppliedReload>`: idempotently applies the committed reload from `<root>/reload-candidates/<pending.candidate_id>/`:
  1. Reads `manifest.json` from the candidate directory.
  2. Clears `manifest.clear_paths` inside `manifest.target_profile_home` while preserving `manifest.protected_paths` (native history and harness-owned skills), using pure Rust filesystem operations (`std::fs`) matching `clear_profile_configuration`'s exact semantics.
  3. Recursively copies `candidate/profile/` into `manifest.target_profile_home` (preserving file modes and setting `go-rwx` / `0600`/`0700` on copied entries; refusing symlinks outside `shared_login`). If `manifest.shared_login` is set (`LocalBare`), installs the shared-login symlink at `<target_profile_home>/<relative_marker> -> <source_path>`.
  4. Atomically copies/renames `candidate/ownership.json` onto `<root>/ownership.json` (`0600`) and `candidate/launch.json` onto `<root>/launch.json` (`0600`).
  5. If `pending.git_setup_name` names an existing generation directory under `<root>`, atomically publishes `<root>/current-git-setup` to point to it (reusing `git_env.rs`'s existing pointer write).
  6. If `manifest.clear_github_token` is true, calls `mj_core::credentials::remove_github_token(&root.join("github-token"))`.
  7. Removes `<root>/pending-reload.json` FIRST, and ONLY THEN recursively removes `<root>/reload-candidates/<pending.candidate_id>/`. Removing `pending-reload.json` before the candidate directory guarantees that a crash before removing `pending-reload.json` still has the full candidate directory to replay idempotently, while a crash after removing `pending-reload.json` leaves an already-applied disk state and only an orphan directory to sweep.
- `roll_forward_pending_reload_if_any(root: &Path) -> Result<Option<String>>`: called at the very start of `WorkerCommand::Run` in `mj-worker-bin/src/main.rs` BEFORE `bootstrap_login_environment` reads `launch.json` (and also safe to call before reading `launch.json` inside `mj-worker`). If `<root>/pending-reload.json` exists, parses it, runs `apply_committed_reload(root, &pending)`, sweeps any leftover `<root>/reload-candidates/*` directories, and returns `Ok(Some(pending.operation_id))`. If no `pending-reload.json` exists, sweeps any stale `<root>/reload-candidates/*` directories left by dead/aborted pre-commit attempts and returns `Ok(None)`.

Split `prepare_harness_environment` in `mj-worker/src/worker_runtime/unix.rs` cleanly into two phases that initial launch and in-place reload both share:

- Phase A (`prepare_harness_private`): performs everything that does NOT touch live `<target_profile_home>` or live `<root>` files — login shell resolution, private `git_setup` TempDir generation (without removing `<root>/github-token` during reload!), `prepare_project_tools`, `install_session_keep_dir`, managed harness + Node resolution and `freeze_node_entrypoints`, and constructing the candidate `AcpSupervisorSpec` in memory. Note: during reload, `configure_native_rules` and the per-harness profile configurators (`configure_codex_profile`, `configure_claude_*`, `configure_antigravity_mailbox_hook`, `configure_opencode_mailbox_hook` / `configure_opencode_guardian`, `configure_kimi_subagent_mcp`) MUST NOT run in Phase A!
- Phase B (`apply_profile_configuration`): runs `configure_native_rules` and the harness-specific profile configurators against `<target_profile_home>` and `<root>`, returning the resulting environment overlay (such as `OPENCODE_CONFIG_CONTENT`), `profile_registration`, `boundary_mailbox_hook_available`, and `mailbox_delivery_mode`.
- Initial worker boot (`initial_launch == true`) simply calls Phase A followed immediately by Phase B (preserving existing boot behavior identically).
- In-place reload (`restart_harness_in_place_response`) runs Phase A ONLY before sending `CommandRequest::ReloadLaunchConfig`. It persists `git_setup` (keeping its TempDir alive until either dropped on pre-commit refusal or committed/published at `HarnessRestarting`) and bundles a `CommittedReloadAction` (carrying `operation_id`, `expected_store_id`, `candidate_id`, `git_setup`, `launch_config`, `base_environment`, `supervisor_path`, `supervisor_spec`, `subagent_role`, `review_tools`, `managed_subagent`) inside `HarnessReloadTransition`.

Wire the single commit point inside `mj-worker/src/acp.rs` and `mj-worker/src/worker_runtime/unix/dispatch.rs`:

1. When ACP is idle, `acp/session.rs` accepts `CommandRequest::ReloadLaunchConfig` and exits `run_bridge` for A, reaping A's supervisor, bridge, terminals, and guardian completely.
2. **CRITICAL:** Move `supervisor_spec.write_spec(&supervisor_path)` OUT of `acp.rs:746-753` (before `HarnessRestarting`) so `acp-supervisor.json` is written ONLY AFTER the commit point and Phase B have completed! Instead, carry `pending_reload_transition` into `RuntimeEvent::HarnessRestarting { reload }`, and have the coordinator return the finalized post-Phase-B `AppliedReloadRuntime` (finalized `environment`, `subagent_mcp_socket`, `managed_subagent`) over the `HarnessReload` oneshot channel!
3. Inside `dispatch.rs`'s `apply_harness_reload_transition` (executed on `HarnessRestarting` while A is dead and B has not spawned):
   - If `transition` carries a `CommittedReloadAction` (staged reload):
     a. Keep the `git_setup` TempDir from being dropped on commit (`let git_setup_dir = git_setup.keep();`, recording its directory name in `PendingInPlaceReload`).
     b. Call `commit_pending_reload(relay_root, &pending)` — **THE SINGLE DURABLE COMMIT POINT**.
     c. Immediately mark `reload_phase = Some(InPlaceReloadPhase::Committed)` in the durable `InPlaceMoveHold` (`relay.lock().mark_in_place_harness_committed(&operation_id, &expected_store_id)?`), so even a live reconnect during B's startup sees `Committed`.
     d. Call `apply_committed_reload(relay_root, &pending)` on a blocking task (applying overlay, atomic `ownership.json` & `launch.json`, `current-git-setup`, optional `github-token` removal, deleting `pending-reload.json`, then deleting the candidate dir).
     e. Run Phase B (`apply_profile_configuration`) against the now-overlaid `<target_profile_home>` and `<root>`, merge any Phase B environment additions (like `OPENCODE_CONFIG_CONTENT`) into both `supervisor_spec.environment` and the bridge's `environment`, and atomically write `acp-supervisor.json`.
     f. Update `shared_credentials.set(...)`, `relay` policies (`turn_verdict_harness`, `authentication_login_path`, steering/mailbox modes, turn/background policies, `retire_harness_process_state`), `TaskMonitors::new(...)`, and `login_identity`.
     g. Transition `*session_configuration = SessionConfiguration::OpeningReload { operation_id }` and reset `startup` (arming the startup deadline and typed `WorkerHarnessStartupFailure` fault path for B!).
     h. Reply `Ok(applied_runtime)` on the `HarnessReload` oneshot so `acp::run_inner` updates `spec.environment` / `spec.subagent_mcp_socket` and spawns B in `run_bridge` (where OpenCode's `ImportedLogins::prepare` immediately reconciles `opencode.db` against B's overlaid `mjolnir-credentials.json`!).
   - If any step AFTER `commit_pending_reload` fails (step d, e, or B failing/timing out to open in `run_bridge` before `SessionConfigured`):
     - Use a typed `ReloadResolution` (`enum ReloadResolution { Started, RefusedBeforeCommit(String), FailedAfterCommit(String) }`) on `pending_reload_ack`!
     - Record `reload_phase = Some(InPlaceReloadPhase::FailedAfterCommit { message })` durably in `InPlaceMoveHold`.
     - Send `ReloadResolution::FailedAfterCommit(message)` on `pending_reload_ack` so `restart_harness_in_place_response` returns `RelayErrorCode::Internal` with `detail: Some(RelayErrorDetail::ReloadFailedAfterCommit { operation_id })` (NEVER `InvalidState`!).
     - Propagate `WorkerHarnessStartupFailure` out of the coordinator so the worker exits cleanly and `mj-worker-bin/src/main.rs:685-693` writes `worker-exit.json` with `harness_startup_failure`.
   - If B opens cleanly (`OpenedSession` + `SessionConfigured`):
     - `pending_reload_ack` sends `ReloadResolution::Started`; `restart_harness_in_place_response` calls `restart_guard.complete()` (setting `harness_restarted = true` and `reload_phase = Some(InPlaceReloadPhase::Started)`) and returns `RelayResponsePayload::HarnessRestartedInPlace`.
   - If reload is rejected BEFORE `commit_pending_reload` (busy turn, wrong store ID, Phase A error, ACP refused reload while busy):
     - `pending_reload_ack` sends `ReloadResolution::RefusedBeforeCommit(message)` -> `restart_harness_in_place_response` returns `RelayErrorCode::InvalidState` with `detail: None`. Live disk is 100% untouched.

Also update `mj-worker/src/relay/commands.rs`:
- `admit_in_place_harness_restart`: inspect `hold.reload_phase`:
  - `Some(InPlaceReloadPhase::Started)` (or `hold.harness_restarted`): return `AlreadyRestarted`.
  - `Some(InPlaceReloadPhase::Committed)`: return retryable `InvalidState` ("in-place harness reload already committed and is starting").
  - `Some(InPlaceReloadPhase::FailedAfterCommit { message })`: return `RelayErrorCode::Internal` with `detail: Some(RelayErrorDetail::ReloadFailedAfterCommit { operation_id })` and `message`.
- `mark_in_place_harness_committed` and `mark_in_place_harness_failed_after_commit`: persist `reload_phase` durably in `relay_state.json`.
- `mark_in_place_harness_restarted`: sets `harness_restarted = true` and `reload_phase = Some(InPlaceReloadPhase::Started)`.

### Milestone 4: Controller In-Place Move Cleanup and Recovery Settlement

Update `mj-controller/src/controller/move_session.rs:3415-3705`:

1. Before staging any candidate on a retry (`had_prior_expected_store_id`), read `conn.client.status().await?.operational_state.in_place_move_hold`:
   - If the hold matches `operation.operation_id` and `hold.harness_restarted` or `matches!(hold.reload_phase, Some(InPlaceReloadPhase::Started))`: skip candidate staging and `restart_harness_in_place` completely and proceed directly to `save_resumed_session(&B)` + `install_prompt_context` + queue settlement!
   - If `matches!(hold.reload_phase, Some(InPlaceReloadPhase::Committed))`: return retryable error without touching disk (another attempt already committed and is starting B).
   - If `matches!(hold.reload_phase, Some(InPlaceReloadPhase::FailedAfterCommit { message }))`: transition directly to the post-commit failure settlement below.
2. When nothing has committed yet, generate a unique `candidate_id` (via `mj_core::relay::new_command_id("reload-candidate")?`), stage B privately via `prepare_worker_files_for(..., WorkerFilePreparation::StageCandidate { operation_id, expected_store_id, candidate_id })`, and call `conn.client.restart_harness_in_place(&operation.operation_id, &expected_store_id, Box::new(launch_config), Some(candidate_id.clone())).await`.
3. Handle the typed outcome cleanly (deleting `rpc_sent`, substring `"already in progress"` parsing, `RestoreProfileOnly`, and `source_disk_confirmed_intact` completely!):
   - **Pre-commit refusal** (local staging error before RPC, or `RelayRejected` with `code == RelayErrorCode::InvalidState` and `detail == None`, excluding the `"already committed"` / `"already in progress"` cases — or even cleaner, distinguish with a typed `RelayErrorDetail` or check `status` if retryable!):
     - Live disk was NEVER modified!
     - Restore `self.state.sessions.insert(id, previous_session)` (in-memory only; DB still says A).
     - Best-effort `remove_reload_candidate(executor, &source_backend, &source_worker_root, &candidate_id)` to tidy the unused candidate dir.
     - Release dispatch hold, reopen subagent admission, clear `in_place_expected_store_id` & `in_place_held_command_ids` (since live disk is guaranteed intact on A!), and return the error.
   - **Post-commit startup failure** (`RelayRejected` with `detail == Some(RelayErrorDetail::ReloadFailedAfterCommit { .. })`, OR after a transport/disconnect error where probing the target's `launch.json` / `pending-reload.json` / `worker-exit.json` proves `operation.operation_id` committed to B):
     - Disk is durably on B!
     - Persist the session row on **B** in `SessionState::Error` with `last_error = Some(message)` (using `save_resumed_session` with `state = SessionState::Error`, or updating the record atomically and marking the Move operation `Failed`), so the DB row now matches on-disk B and credential sync can safely un-fence without pushing A's credentials onto B's disk! The user sees the clear startup error on B and can immediately Move to another profile or Restart agent.
   - **Ambiguous transport failure where target cannot be reached**:
     - Leave DB on A, leave `in_place_expected_store_id` set (keeping credential sync fenced until recovery/retry reconnects and reads `in_place_move_hold` or disk `launch.json`), and return the error so daemon recovery retries cleanly.

Also check `mj-controller/src/controller/move_session.rs` recovery path (`recover_move_operation` around lines 2535-2587) so an interrupted in-place Move recovering after daemon restart handles a dead worker whose `launch.json` has `accepted_reload_operation_id == Some(operation.operation_id)` and `worker-exit.json` has `harness_startup_failure` by settling the session on B in `SessionState::Error` rather than looping forever!


## Concrete Steps

All commands run from the checkout root `/home/jonathan/Projects/mjolnir/.mj/clones/62f67a3f25ad58150b0b60eb2a982e94` using `mbx` (never plain `cargo`).

After Milestone 1:

    mbx test -p brokk-mj-core --lib worker_launch
    mbx test -p brokk-mj-core --lib relay
    mbx test -p brokk-mj-controller --lib move_session::in_place
    mbx clippy -p brokk-mj-core -p brokk-mj-controller --all-targets -- -D warnings

After Milestone 2:

    mbx test -p brokk-mj-worker-staging --lib worker_binary
    mbx test -p brokk-mj-worker-launch --lib
    mbx clippy -p brokk-mj-worker-staging -p brokk-mj-worker-launch --all-targets -- -D warnings

After Milestone 3:

    mbx test -p brokk-mj-worker --lib reload_commit
    mbx test -p brokk-mj-worker --lib relay::in_place_move_tests
    mbx test -p brokk-mj-worker --lib worker_runtime::relay_tests
    mbx test -p brokk-mj-worker --lib acp::tests
    mbx test -p brokk-mj-worker-bin --lib
    mbx clippy -p brokk-mj-worker -p brokk-mj-worker-bin --all-targets -- -D warnings

After Milestone 4 (full affected boundary validation):

    mbx test -p brokk-mj-core --lib worker_launch
    mbx test -p brokk-mj-core --lib relay
    mbx test -p brokk-mj-worker-staging --lib
    mbx test -p brokk-mj-worker-launch --lib
    mbx test -p brokk-mj-worker --lib reload_commit
    mbx test -p brokk-mj-worker --lib in_place
    mbx test -p brokk-mj-controller --lib move_session
    mbx clippy -p brokk-mj-core -p brokk-mj-worker-staging -p brokk-mj-worker-launch -p brokk-mj-worker -p brokk-mj-worker-bin -p brokk-mj-session-runtime -p brokk-mj-controller --all-targets -- -D warnings


## Validation and Acceptance

Acceptance requires the following tricky-condition and integration tests (all of which would fail before this change and pass after):

1. Atomic `WorkerLaunchConfig::write` (`mj-core/src/worker_launch.rs`): writes via temp file created with `0600` mode on Unix and atomically replaces an existing `launch.json`, leaving no temp file behind and preserving the prior file intact if serialization or target path fails.
2. Protocol 91 capability gate (`mj-controller/src/controller/move_session/in_place.rs`): a Protocol 81 worker (including pre-#1673 Rust workers and the Go worker) is refused for manual Moves with the `mj restart` instruction and skipped for automatic quota Moves; a Protocol 91 worker is accepted.
3. Zero live-disk mutation on refusal (`mj-worker` and `mj-controller`): staging a candidate (`StageCandidate`) and rejecting `RestartHarnessInPlace` because a turn is active, store ID mismatches, or Phase A preparation fails leaves live `launch.json`, `ownership.json`, `acp-supervisor.json`, `current-git-setup`, and every file in `<target_profile_home>` byte-identical to before the attempt (no `config.toml`, `.claude.json`, `settings.json`, or `opencode-mailbox-plugin.mjs` edits leaked into live A).
4. Crash-idempotent roll-forward (`mj-worker/src/worker_runtime/reload_commit.rs` and `mj-worker-bin`): writing `pending-reload.json` and simulating crashes at intermediate states (before overlay, after partial overlay, after `launch.json` promote before removing `pending-reload.json`) and invoking `roll_forward_pending_reload_if_any` converges identically to B's profile overlay, `ownership.json`, `launch.json`, `current-git-setup`, preserves native history and harness-owned skills, and removes both `pending-reload.json` and the candidate directory.
5. OpenCode post-commit login reconciliation (`mj-worker`): after candidate overlay replaces `mjolnir-credentials.json` in a stable OpenCode home with an existing `.data/opencode/opencode.db` containing profile A's login and session history, `ImportedLogins::prepare` replaces A's credentials with B's while preserving session history.
6. Post-commit replacement startup failure (`mj-worker` and `mj-controller`): when ACP accepts reload, commits B, reaps A, and replacement bridge B fails to open (`SessionFault`), the worker records `InPlaceReloadPhase::FailedAfterCommit` and exits with `WorkerHarnessStartupFailure` in `worker-exit.json` (returning `RelayErrorDetail::ReloadFailedAfterCommit`, NEVER `InvalidState`), and the controller settles the session row on B in `SessionState::Error` without restoring A over disk.
7. Retry idempotence after lost reply (`mj-controller`): when a daemon retries an in-place Move after B already started (simulated via `had_prior_expected_store_id` with `in_place_move_hold.reload_phase == Started`), the controller queries `Status`, skips re-staging and `RestartHarnessInPlace`, does not overwrite worker-configured files on live B, and completes DB settlement, prompt-context install, and queue release cleanly.


## Idempotence and Recovery

Every stage of the protocol is safe to repeat or interrupt:

- Pre-commit failure or cancellation leaves only `<worker_root>/reload-candidates/<candidate_id>/`, which is removed on refusal and also swept on the next worker boot if a crash interrupted cleanup. Live A is untouched.
- Post-commit crash at any instruction before `pending-reload.json` is unlinked replays `apply_committed_reload` identically on the next worker boot (before `bootstrap_login_environment` reads `launch.json`) because the candidate directory is deleted only after `pending-reload.json` is deleted.
- Daemon crash or RPC reply loss after commit is resolved on reconnect by reading `in_place_move_hold.reload_phase` (if the worker is alive) or `launch.json`'s `accepted_reload_operation_id` + `worker-exit.json` (if the worker exited on startup failure).
- Credential sync stays fenced by `in_place_expected_store_id` until either a pre-commit refusal confirms live disk is still A or DB settlement commits the session row to B (running or `Error`).


## Artifacts and Notes

Reference investigations used to design this plan (already read and folded into the sections above):

    Advisor review: /mnt/optane/tmp/mjolnir/91233b0efdd193066e7b4eeb46c8db10/home/jonathan/Projects/mjolnir/.mj/clones/91233b0efdd193066e7b4eeb46c8db10/.mj/agents/40fd698281a810d7baf5205d6afd1f78/profile-move-review.md
    OpenCode DB check: .mj/agents/e366148c7b9b8faa8bbf8588c9aea33a/prereq_checks.md
    Upstream post-retirement hook: git show fdd2bb564 54b5ef09b 5a1e73499


## Interfaces and Dependencies

In `mj-core/src/relay.rs`:

    pub const RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL: u32 = 91;
    pub const RELAY_PROTOCOL_VERSION: u32 = RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL;

In `mj-core/src/relay/protocol.rs`:

    RelayRequest::RestartHarnessInPlace {
        operation_id: String,
        expected_store_id: String,
        launch_config: Box<crate::worker_launch::WorkerLaunchConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        candidate_id: Option<String>,
    }

    RelayErrorDetail::ReloadInProgress {
        operation_id: String,
    }

    RelayErrorDetail::ReloadFailedAfterCommit {
        operation_id: String,
    }

In `mj-core/src/relay/snapshot.rs`:

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "phase", rename_all = "snake_case")]
    pub enum InPlaceReloadPhase {
        Committed,
        Started,
        FailedAfterCommit { message: String },
    }

    pub struct InPlaceMoveHold {
        pub operation_id: String,
        pub captured_pending_command_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub harness_restarted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reload_phase: Option<InPlaceReloadPhase>,
    }

In `mj-core/src/worker_launch.rs`:

    pub const RELOAD_CANDIDATES_DIR: &str = "reload-candidates";
    pub const PENDING_RELOAD_FILE: &str = "pending-reload.json";
    pub const RELOAD_CANDIDATE_MANIFEST_FILE: &str = "manifest.json";

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ReloadSharedLogin {
        pub relative_marker: String,
        pub source_path: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ReloadCandidateManifest {
        pub operation_id: String,
        pub expected_store_id: String,
        pub candidate_id: String,
        pub harness: HarnessKind,
        pub target_profile_home: String,
        pub clear_paths: Vec<String>,
        pub protected_paths: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub shared_login: Option<ReloadSharedLogin>,
        #[serde(default)]
        pub clear_github_token: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct PendingInPlaceReload {
        pub operation_id: String,
        pub expected_store_id: String,
        pub candidate_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub git_setup_name: Option<String>,
    }

In `mj-worker-staging/src/worker_binary/launch.rs`:

    pub enum WorkerFilePreparation {
        Install,
        RefreshProfileOnly,
        StageCandidate {
            operation_id: String,
            expected_store_id: String,
            candidate_id: String,
        },
    }

In `mj-worker/src/worker_runtime/reload_commit.rs`:

    pub fn validate_candidate(
        root: &Path,
        candidate_id: &str,
        operation_id: &str,
        expected_store_id: &str,
    ) -> Result<ReloadCandidateManifest>;

    pub fn commit_pending_reload(root: &Path, pending: &PendingInPlaceReload) -> Result<()>;

    pub fn apply_committed_reload(root: &Path, pending: &PendingInPlaceReload) -> Result<ReloadCandidateManifest>;

    pub fn roll_forward_pending_reload_if_any(root: &Path) -> Result<Option<String>>;
