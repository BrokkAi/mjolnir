# Isolate checkpoint exports and exclude destruction from recovery

This ExecPlan is maintained in accordance with `.agents/PLANS.md`. Its living sections are Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective.

## Purpose / Big Picture

Issues #990 and #991 describe a failed bundle export and a closed Podman worker restarting after cleanup failed. Export operations must transfer their own immutable archive, report useful evidence when checksum verification fails, and preserve the partial implementation. Once a verified close enters destruction, automatic recovery must never restart its target. Scope is repository changes and isolated tests, not recovery of the original live sessions.

## Progress

- [x] (2026-09-13) Read both issues, repository instructions, and relevant transfer, checkpoint, lifecycle, polling, and recovery code; user approved repository-only scope and implementation plan.
- [x] (2026-09-13) Implemented operation-specific checkpoint paths, successful legacy specification cleanup, checksum diagnostics, and overlapping/corrupt/truncated transfer regressions.
- [x] (2026-09-13) Implemented durable recovery exclusion, exact source-target checks, per-session serialization with destruction, and lifecycle/race regression tests.
- [x] (2026-09-13) Passed formatting, full serial workspace tests, Clippy, and diff review.
- [x] (2026-09-13) Committed #990 as `23d86668`; the #991 fix and this completed ExecPlan form the final commit on `hel2`. No push was requested or performed.

## Surprises & Discoveries

`session_target_is_pollable` includes `Destroying` because it uses dashboard visibility as its starting point. Runtime lifecycle exclusion only lasts while a lifecycle task is active, so a failed cleanup exposes the target to recovery again. Recovery runs in a blocking task and can start stopped containers without rereading lifecycle state.

Checkpoint exports use `checkpoint.hel.zip` and `checkpoint-spec.json` on the target, and transfer staging uses a session-wide filename. Local installed checkpoints already have unique names. This is a concrete collision risk, but is not proof of the reported incident's cause.

## Decision Log

Decision (2026-09-13, Codex): retain SHA-256 verification and atomic installation, use unique operation paths, and report expected/actual digest, size, session, and retained archive path. Rationale: failed verification must not destroy source work or replace a verified checkpoint.

Decision (2026-09-13, Codex): use durable state eligibility plus a per-session guard shared by recovery and destruction. Rationale: filtering future polls alone does not stop an already-running recovery task. Keep blocking waits and subprocess work on supervised background execution paths.

## Outcomes & Retrospective

Both fixes are implemented and recorded in separate commits on `hel2`. Export collisions are eliminated for independently identified operations, and checksum failures preserve the previous checkpoint and target archive while reporting evidence. Durable destruction now excludes automatic recovery even after cleanup failure, with a shared guard ordering already-admitted recovery before destruction. The original live incidents were not reproduced or modified. Final Clippy and formatting checks passed. The corrected session-manager suite passed all 42 tests, and the workspace run passed all 1,178 controller tests with 5 ignored. That run later hit an unrelated Grok cache-lock race in the worker suite; its isolated rerun passed. The full serial workspace run passed: 3,179 tests passed, 22 ignored, zero failures, including worker and CLI integration tests and documentation tests. No live-session changes were made.

## Context and Orientation

`mj-controller/src/controller/checkpoint.rs` captures target state and installs checkpoint metadata. `mj-controller/src/checkpoint_transfer.rs` copies the archive and creates `VerifiedCheckpoint`, the proof needed to permit later cleanup. `mj-controller/src/controller/lifecycle.rs` seals the relay, persists `Destroying`, verifies the installed archive, and stops/removes the exact target. `mj-controller/src/pollers.rs` selects live workers. `mj-controller/src/session_manager.rs` owns relay connections and runs recovery after failed connections. Durable state is read through the controller database; `Closing` still requires a connection, while `Destroying` must only clean up resources.

## Plan of Work

### Milestone 1: Isolated checkpoint transfer

Generate one operation identifier before export and use it in the specification, archive, capture directory, and transfer staging. Propagate exact paths into cleanup; never remove a session-wide staging file. Maintain compatibility with existing workers by changing supplied paths, not the checkpoint format. On checksum mismatch, identify the whole archive, report both hashes and downloaded size, and explain that the source archive is retained and a fresh export may be retried. Add interleaved-transfer tests using distinct payloads larger than 64 KiB and failure tests proving that the prior verified checkpoint and source survive.

### Milestone 2: Exclusive destruction

Remove `Destroying` from polling eligibility. Add a shared per-session target guard; recovery holds it while reloading durable state, checking target identity, and issuing recovery commands. Destruction holds the same guard from before persisting `Destroying` through target cleanup. Recovery returns an explicit suppressed outcome if durable state is ineligible, removed, or points at another target. Preserve normal recovery and interrupted-close behavior for eligible states. Test failed cleanup, stale plans, and recovery admitted before destruction; demonstrate no start occurs after destruction is persisted.

### Milestone 3: Validation and commit

Run focused regressions, then required workspace checks. Review the final diff for unrelated changes and commit the validated implementation on the current branch without pushing. Update this document with the evidence and any implementation discoveries.

## Concrete Steps

Work from `/home/jonathan/Projects/hel2`. Use `cargo test -p brokk-mj-controller <filter>` for focused tests, outside the restricted sandbox. Run `cargo fmt --all -- --check`, `cargo test` outside the restricted sandbox, and `cargo clippy --all-targets -- -D warnings`. Keep normal Cargo build storage; do not redirect builds to `/tmp`. Expect zero failed tests and no Clippy warnings.

## Validation and Acceptance

Two exports for one session must download distinct correct archives even when their copy/download/cleanup phases interleave. Corrupt or truncated downloads must fail with digest and size evidence, leave previous metadata and archives intact, and not delete the target archive. A failed stop must leave durable `Destroying` excluded from manager targets across refreshes. A queued stale recovery must emit no target commands. An already admitted recovery must finish before destruction is persisted and cleanup begins. Cleanup retry must still work and a changed installed checkpoint must still block destruction.

## Idempotence and Recovery

Use temporary test directories and isolated configuration/data roots for database tests. Do not open or upgrade the live store. Failed exports retain source evidence; successful transfers clean only their own temporary artifacts. Guard release follows lexical ownership even on errors. No schema migration or archive-format revision is intended.

## Artifacts and Notes

Initial evidence: the working tree was clean, and the current implementation uses a single target archive path plus `.local/share/hel/transfers/{session_id}.hel.zip`. Validation evidence: `cargo test -p brokk-mj-controller session_manager::tests::` passed 42 tests. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed. The parallel workspace run passed the controller suite but failed `grok_launchers_execute_after_relocation_and_invalid_cache_repairs_itself` with “managed harness repair deferred … still in use.” The worker test module documents brief lease inheritance between fork and exec during parallel tests. The exact isolated test passed, and `cargo test -- --test-threads=1 --quiet` subsequently passed the full workspace without changing unrelated worker code.

## Interfaces and Dependencies

Use existing command executors, path validation, `new_command_id`, checkpoint hashing, and database APIs. Extend internal transfer helpers with exact operation staging paths and add an internal suppressed recovery outcome. No new crate, CLI flag, or external dependency is needed.

Revision note (2026-09-13): created from the approved implementation plan before editing source.

Revision note (2026-09-13): implemented both milestones. The existing `RecoveryGate` coordinates recovery copies and upgrades but does not serialize actor-driven worker restart with close; added a weakly retained per-session mutex beside it. Recovery carries the original durable target locator and reloads session state after taking this mutex. A paused-recovery/failed-Podman-stop regression passed and confirms that destruction waits, subsequent recovery is suppressed, and cleanup can be retried with the verified archive intact. The slow close-latch test intentionally delays startup and passed.

Revision note (2026-09-13): recorded corrected fixture validation and the unrelated parallel worker test failure; final verification uses `cargo test -- --test-threads=1 --quiet`.

Revision note (2026-09-13): all validation is complete. The final serial workspace run passed 3,179 tests with 22 ignored and zero failures. The validated changes are recorded as two issue-specific commits; no implementation work remains.

Revision note (2026-09-13): closed out the plan with completed validation and separate local commits. The original #990 cause remains unproven; the implemented collision fix and failure diagnostics satisfy its repository scope without bypassing verification.
