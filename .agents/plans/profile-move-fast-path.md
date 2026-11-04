# Keep native session state during profile Moves

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

A Move between compatible profiles on the same environment should keep the installed worker, native conversation files, workspace, and build cache. It should replace profile configuration and restart the harness. Opening its confirmation must not wait for an automatic recovery checkpoint. The existing verified session handoff remains the durable recovery boundary; transferring that archive back to its source and restoring native files that never moved is unnecessary.

## Progress

- [x] (2026-10-28) Trace the reported Move and both checkpoint paths.
- [x] (2026-10-28) Make confirmation preparation observe the session actor without taking worker lifecycle ownership.
- [x] (2026-10-28) Retain native files and the worker for a compatible in-place Move; refresh profile files and install only the small relay seed.
- [x] (2026-10-28) Pass all 64 initial Move integration and ordering regressions, including the new retention and ownership cases.
- [x] (2026-10-28) Finish full controller/checkpoint suites, rerun the sole shell-startup timeout in isolation, and pass workspace Clippy and formatting.
- [x] (2026-10-28) Commit implementation as `06f4f8792`, merge the concurrent macOS golden-only update as `035409e8e`, and push to origin/master.
- [x] (2026-10-28) Confirm successful agent-dev publication for both architectures and the manifest in run `40627986226`.

## Surprises & Discoveries

The reported session waited roughly 58 seconds behind a full recovery checkpoint during initial preparation. That checkpoint prestaged native and repository files, captured again under a barrier, packaged, and downloaded. The Move's own handoff used repository metadata only and took 6.35 seconds. Destination restoration then spent about 45 seconds resetting the profile and worker, installing them again, and restoring the archive, although the environment and native conversation did not change.

## Decision Log

- Decision: Preparation reads the session actor's published snapshot. Only confirmed execution takes exclusive worker ownership and refreshes state immediately before interruption.
  Rationale: An inspector cannot need the same mutation permit as a checkpoint. Execution already owns the authoritative confirmation check.
  Date/Author: 2026-10-28, Codex.
- Decision: Reuse the unchanged accepted sub-agent policy and skip worker, runtime, mount-source, and checkout provisioning probes for a compatible unchanged environment. Continue validating changed configuration and the destination's supported model and effort before interruption.
  Rationale: A credential switch does not change the resources already installed in the running environment or the unchanged delegation policy. Retrying an interrupted replacement still checks prerequisites and restores from its handoff because an older attempt may have removed native files.
  Date/Author: 2026-10-28, Codex.
- Decision: Keep the verified handoff and existing Move phases, queue admission, retry, and restart reconciliation. For a compatible native conversation retained in place, replace only staged profile configuration and launch metadata and seed a fresh relay from the verified canonical snapshot.
  Rationale: This removes native restoration, archive upload, worker installation, build-cache provisioning, and Git-settings provisioning without inventing a second recovery protocol. The small relay seed is the existing persisted format, shared with checkpoint restoration.
  Date/Author: 2026-10-28, Codex.

## Outcomes & Retrospective

The initial 64 Move regressions passed. Final validation ran the complete touched-crate suites: checkpoint had 165 passes and 2 ignored tests; controller had 2,173 passes, 17 ignored tests, and one failure in `container_runtime_freezes_relative_path_and_provider_selection`. That failure was the existing Bash login-shell initialization exceeding 10 seconds. The exact test passed on an isolated rerun in 3.11 seconds. Workspace `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` both passed. The timeout is recorded without changing the unrelated login-shell deadline or rerunning the complete suite.

The normal compatible in-place path retains native file and executable inodes and avoids archive restoration. The ownership regression observes the actor while a checkpoint permit is held. Existing queue, failed-swap, cross-harness, and daemon-restart regressions passed. No live instance was upgraded or used for testing. Implementation commit `06f4f8792` and merge `035409e8e` are pushed to master. The merge brought only macOS golden-file regeneration and required no repeat of the Linux checks. Agent-dev image run `40627986226` completed successfully for amd64, arm64, and manifest publication: https://github.com/BrokkAi/mjolnir/actions/runs/40627986226.

The implementation is complete. The user requested that the final completion report also go to session `6a3d03316390200372b0f6e4603c35ee`; that handoff follows the final documentation commit.

Review also identified a worker surviving a daemon upgrade. Profile refresh reads the installed executable's build identity under lifecycle ownership and upgrades an older/different worker only when needed. Native state remains in place in both cases; the existing second-local-target integration case covers executable replacement while preserving that state. The initial full validation run was cancelled before completion when this case was found; the final validation above includes the corrected source.

## Context and Orientation

`mj-controller/src/daemon/session_move.rs` prepares a confirmation by calling `refresh_move_source`, defined in `mj-controller/src/controller/move_session.rs`. That helper currently leases the relay under a worker mutation permit. `move_session_managed_controlled` independently obtains the permit and refreshes the source again during confirmed execution.

`mj-controller/src/controller/resume/in_place.rs` verifies a sealed Move's archive and invokes `restore_into_target` in `mj-controller/src/controller/resume.rs`. The general tail deletes the old native profile and installed worker, stages a profile, installs the worker and build cache, transfers the archive, and restores it. `mj-controller/src/controller/worker_binary/launch.rs` stages profiles; `worker_binary/install.rs` installs configuration and binaries. Profile paths are independent of profile identity, so a compatible profile switch can keep native history at its existing path.

`mj-checkpoint/src/checkpoint/restore.rs` constructs `mj_core::relay::RestoredRelaySeed` from a verified canonical snapshot. The worker already reads this small format on startup. Share that construction with the retained native path instead of making a second interpretation. The seed carries accepted model and effort, the history frontier, and assessment state; Move keeps queued commands held until explicit admission after readiness.

## Plan of Work

First remove lifecycle admission from confirmation observation, retaining strict admission on confirmed execution. Extend an existing isolated relay test to hold worker ownership while observing.

Next distinguish a retained native conversation from a general in-place harness replacement in the restore plan. Stop the owning process before clearing relay files. Retain the installed worker and native home. Refresh the profile's allowlisted configuration, removing stale source configuration while preserving harness-owned skills and native state. Reuse profile staging and installation code, with binary installation and machine provisioning only in the full installation mode. Construct and upload the existing small relay seed instead of uploading and restoring an archive. Continue through the existing readiness and queue logic.

Extend the existing in-place Move integration fixture to verify retained native state, exact destination configuration, queue and selector preservation, retries, and absence of archive restoration, worker installation, and cache provisioning. Cross-harness Moves must still take the existing full replacement path.

## Concrete Steps

Work in the repository root. Use normal Cargo storage managed by mbx; do not redirect targets. Run all Cargo tests outside the restricted sandbox with isolated test stores. During implementation run filtered tests with `cargo test -p brokk-mj-controller controller::move_session` and affected checkpoint tests. After the last change run the full suites for touched crates and `cargo clippy --all-targets -- -D warnings`, on the dev profile. Run `cargo fmt --check` and inspect the diff before staging only changed files, committing, and pushing `HEAD:master`.

## Validation and Acceptance

An isolated confirmation test must complete while a recovery operation holds worker ownership. A compatible in-place Move must preserve native file contents and the installed executable, apply the destination credentials and configuration, keep the workspace and cache, preserve accepted selectors and queued-work policy, and complete using the existing worker/relay integration fixture. Its recorded commands must contain no archive restore or binary/cache provisioning. Existing cross-harness and crash/retry tests must still pass. Never run a test build against the live default store.

## Idempotence and Recovery

No schema or wire-protocol change is planned. The existing handoff archive and Move phases remain authoritative for retry and daemon restart. Profile refresh and seed installation must be repeatable before queue admission. Any failed refresh reports failure and retains the environment and handoff for the existing explicit retry; it must not silently switch execution paths.

## Artifacts and Notes

Reported session: `28c4a33bde5b6bc0b4f4d45aa56e46bd`, source `claude3`, destination `claude4`, retained `morannon-podman` environment. Recorded destination revalidation was 1.472 seconds; the roughly two-minute experience includes ownership waiting and redundant restoration.

## Interfaces and Dependencies

Keep the controller/worker ownership boundary and current shared seed format. Share seed creation in `mj-checkpoint`; no new workspace crate, launch field, or relay command is needed. Worker-file preparation should make install versus configuration refresh explicit. Profile allowlisting must have one definition used by staging and refresh cleanup.

Plan created 2026-10-28 from the reported latency and traced recovery boundaries.

Updated 2026-10-28 after initial regression success: document the preserved handoff boundary, explicit recovery behavior, and removal of provisioning and unchanged-policy checks.

Updated 2026-10-28 during final review: retain compatibility with a worker that outlived its daemon by conditionally upgrading its executable without restoring native state.

Updated 2026-10-28 after validation: record the complete test counts, successful isolated shell-timeout rerun, and clean Clippy/format checks.

Updated 2026-10-28 after pushing: record the implementation/merge commits, the unrelated macOS golden update, and the pending image publication and requested notification.

Updated 2026-10-28 at closeout: both image architectures and manifest publication succeeded; finish the validation record and hand off the completed work.
