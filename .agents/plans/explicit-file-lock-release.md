# Release file locks at their owning scope

This plan follows `.agents/PLANS.md` and remains a living record.

## Purpose / Big Picture

An unrelated process fork inherits a locked file description until exec. Closing the original file alone can therefore leave commands refusing ownership, repairs reporting an installation in use, or cleanup skipping work after the real owner finishes. A shared guard explicitly unlocks when its owning scope ends.

## Progress

- [x] (2026-10-27) Trace file-lock acquisition sites and contenders throughout the workspace.
- [x] (2026-10-27) Introduce `mj_core::file_lock::FileLock` and convert scoped lock ownership, including existing private explicit-unlock guards.
- [x] (2026-10-27) Add a deterministic regression that holds an unrelated child before exec while verifying managed-install collection after release.
- [x] (2026-10-27) Run original-base core/controller/worker suites and workspace clippy; pause for the parent's upstream merge.
- [x] (2026-10-27) Convert WorkerRootOwner following parent decision; retain login re-exec and make its existing deterministic test assert immediate acquisition before releasing the inherited child.
- [x] (2026-10-27) Complete merged-base core/controller/worker/CLI suites, focused harness tests, workspace clippy, rustfmt and diff checks; no failures.
- [x] (2026-10-27) Parent released relay_tests.rs; replace its two helper calls and remove acquire_after_fork_exec_window, retaining the deterministic fork fixture.
- [x] (2026-10-27) Complete final focused root/checkpoint tests, full worker suite, workspace clippy, rustfmt and diff checks on 44b0021cc; every command passed. No commits created.

## Surprises & Discoveries

Config updates, Muse token refresh, Move capture, and the Codex quota client's early-return paths also held file locks. SSH's opening mutex and worktree-fetch mutex are in-process mutexes, so their similar names do not imply file locks. The daemon already explicitly unlocks its store guard and its startup client probes store ownership before launching a successor.

## Decision Log

- Decision: Use one guard owning an already locked File, an inspection accessor, and a consuming fallible release.
  Rationale: Existing call sites retain their acquisition policy and cancellation semantics; every exit path then releases through one owner.
  Date/Author: 2026-10-27 / lock audit sub-agent.
- Decision: Report the worker root's process-lifetime lock separately without editing root_owner.rs.
  Rationale: It has intentional login re-exec descriptor inheritance and test-only retries; parent approval was required and subsequently granted.
  Date/Author: 2026-10-27 / lock audit sub-agent.

## Outcomes & Retrospective

The original-base core, controller and worker suites and workspace clippy passed. Parent fast-forwarded to 0a47952e5, then authorized WorkerRootOwner conversion with abrupt-exit refusal remaining visible and no retries. Merged-base validation and final follow-up passed with no failures; relay helper cleanup is complete. The parent committed the parallel executable-write fix as 65d15a5ea while checks ran; this agent created no commits. Evidence is written in `.mj/agents/ce99dd6418e0bbe5a6098b64eb878b06/`.

## Context and Orientation

`mj-core` is the shared Rust crate. Its new `file_lock.rs` owns release; `harness_install.rs` coordinates installers and runtime leases; `config.rs` and `attachment.rs` serialize persisted writes; `targets/ssh.rs` coordinates shared masters. Controller consumers hold setup, image-download, daemon-store and quota locks. The worker supervisor holds managed-install leases; Move captures hold a per-destination lock. The CLI's existing startup guard is replaced by the shared mechanism.

## Plan of Work

Convert only successful acquisition paths into FileLock immediately, before subsequent fallible work or await points. Keep failed acquisition Files unguarded. Preserve existing WouldBlock policies. Replace the supervisor, daemon startup and controller store private release implementations. Add a real managed-install cleanup regression with a child blocked in pre_exec. Do not edit relay_tests.rs or concurrent agents' files.

## Concrete Steps

From the repository root, run rustfmt on changed modules, then run `cargo test -p brokk-mj-core`, `cargo test -p brokk-mj-controller`, `cargo test -p brokk-mj-worker`, and `cargo test -p brokk-mjolnir` outside the restricted sandbox. Run `cargo clippy --workspace --all-targets --exclude brokk-mj-desktop -- -D warnings`. Rerun failures alone. Preserve normal Cargo/mbx configuration.

## Validation and Acceptance

The regression must collect an obsolete install immediately after the lease owner drops even while a child retains the inherited descriptor. Existing supervisor regression must continue passing. Replace the core tests' former fork-window polling with immediate assertions now that their lease owners explicitly unlock. Record full suite and clippy logs in the report directory; report unrelated failures with names and individual rerun evidence.

## Idempotence and Recovery

All regression files are in temporary isolated directories. No live daemon or store is touched. Changes are ordinary source edits and can be reviewed without installing a build.

## Artifacts and Notes

The final audit table includes protection, contender response, concurrent fork exposure, observable impact, and action for each file-lock site. Process exit bypasses Drop and needs a separate ownership decision when a successor immediately refuses.

## Interfaces and Dependencies

`FileLock::new(File) -> FileLock` transfers an already held lock; `file(&self) -> &File` supports inode inspection; `release(self) -> io::Result<()>` explicitly unlocks and closes, while Drop logs unlock errors. No new dependencies or persisted formats.

## Parent decision after upstream merge

The parent approved converting WorkerRootOwner to FileLock and explicitly retaining the abrupt-exit limitation: a crash, SIGKILL, or process::exit can leave an inherited lock until an unrelated child execs, and a successor in that interval may be refused visibly. No new retry or wait is allowed. from_reexec adopts the inherited File into FileLock; set_reexec_inheritance accesses that File's descriptor. The worker root's existing deterministic test now reacquires immediately while its child is still held before exec. No new regression was added because that existing test covers this distinct owner.

relay_tests.rs remains owned by another child. Its two calls to acquire_after_fork_exec_window must become WorkerRootOwner::acquire. Keep the helper until those callers can be edited; remove it afterward and rerun affected tests. This update records the new base, the parent's decision, and the staged ownership handoff.

Parent released relay_tests.rs after committing the timeout fixes as 44b0021cc. Both relay calls now acquire directly and the retry helper is deleted. Final requested validation is running. Git history c233a1fbc confirms the checkpoint-only flake was an inherited-flock refusal before the expected parse error; no temp-root collision is needed.

Final outcome: both relay callers acquire directly, no acquire_after_fork_exec_window symbol remains in Rust sources, and all requested final checks passed. The shared guard owns every audited scoped release; only accepted abrupt-exit limitations remain. Final artifact inventory and commit-body draft are in the report directory as commit-files.txt and commit-message.txt.
