# Reuse the in-place Move optimizations for Restart

This ExecPlan follows `.agents/PLANS.md` and is a living document.

## Purpose / Big Picture

Restart should seal a small session handoff, keeping workspace files, build caches, the staged harness home and the installed worker in place. It should use the same capture and restore machinery as an in-place Move. The last complete recovery checkpoint must remain usable.

## Progress

- [x] (2026-10-28) Traced Restart, Move, archive retention and native-home restoration.
- [x] (2026-10-28) Implemented schema 102, durable handoff ownership, shared export policy and native retention.
- [x] (2026-10-28) Initial implementation passed 64 focused tests, the full controller suite (2,186 passed, 17 ignored), Clippy, formatting and real-worker acceptance.
- [x] (2026-10-28) Follow-up: unified Move and Restart sealing through the existing close state machine, removing Move’s separate sealer.
- [ ] Validate the shared close state machine (completed: 177 Move-related tests passed; remaining: full controller suite, Clippy and real-worker acceptance after integrating upstream).
- [ ] Commit and push the validated change to origin/master.

## Surprises & Discoveries

Move stores a separate `handoff` archive in its durable operation. Restart only stores the digest of a full session checkpoint. Replacing that checkpoint with metadata-only repository content would lose the last full recovery copy, so Restart needs its own archive reference.

The first real-worker acceptance attempt timed out before Restart, at the initial Move. Setup had imported the container's real profiles, and delegation validation probes all enabled profiles. The scenario detaches its setup TUI before editing the isolated configuration, cancelling its discovery owner so it cannot import host profiles afterward. It keeps only its two deterministic ACP profiles enabled and waits for accepted background child destruction to remove the child before changing delegation. No product code changed for this fixture issue.

## Decision Log

- Decision: add a nullable handoff column with breaking schema revision 102.
  Rationale: older daemons cannot recover or retain an archive owned by Restart.
  Date/Author: 2026-10-28, Codex.
- Decision: share the `SessionHandoff` export policy between Move and explicit Restart, while retaining full checkpoints for historical automatic Restart and container replacement recovery.
  Rationale: those historical intents retain the recovery semantics they were accepted under.
  Date/Author: 2026-10-28, Codex.
- Decision: retain native state only on a freshly sealed session with its own staged home.
  Rationale: a failed reset must reconstruct state from its archive; a fresh seal still owns its native files.
  Date/Author: 2026-10-28, Codex.

## Outcomes & Retrospective

The shared export policy and Restart-owned handoff are implemented. All 64 Restart-related tests pass, including the new oversized workspace and migration rollback regressions. The full controller suite passed (2,186 tests, 17 ignored), along with workspace Clippy and formatting. Real-worker acceptance is running.

## Context and Orientation

The daemon is the controller process that owns durable lifecycle operations. `mj-controller/src/daemon/restart.rs` admits Restart and records recovery phases. `database/session_restart.rs` persists its intent. `controller/lifecycle.rs` captures and seals the worker relay (the worker's journal connection). `controller/checkpoint/latched.rs` exports an archive while holding dispatch at a known journal position. `controller/lifecycle.rs` now captures and seals both Move and Restart handoffs in the same close state machine; Move’s separate handoff sealer was removed. `controller/resume/in_place.rs` restores into an existing environment and can retain a staged native harness home. `controller/checkpoint/archives.rs` deletes unreferenced archives at startup.

## Plan of Work

Add a Restart-owned handoff reference and migration 102. Share the metadata-only repository layout between Move and Restart, preserving Restart's existing close, failure and cancellation transitions. Record the handoff before sealing and retain it during startup sweeping. Restore Restart from that handoff when present and enable the same native-home retention used by Move on a fresh seal. Keep full-checkpoint restoration for legacy intents. Extend isolated regression coverage for oversized workspace files, recovery archive retention and native-home retention.

## Milestones

The first milestone adds durable Restart ownership and a shared export policy. Its acceptance is the isolated oversized-workspace regression: Restart preserves the full checkpoint and native-home inode, and its separate handoff survives startup sweeping until the operation finishes.

The second milestone validates the user-facing daemon action with real workers through the existing Move acceptance scenario. Run the controller suite, Clippy and formatting, then run the acceptance command below. Success means Restart completes beside a 9 GiB untracked file while retaining the checkout, native identity, staged home and ignored build output. Commit and push the validated result.

## Concrete Steps

From the repository root, implement the database, capture, restore and retention changes. Run focused tests with `cargo test -p brokk-mj-controller <filter>`, then `cargo test -p brokk-mj-controller` and `cargo clippy --all-targets -- -D warnings`. Cargo uses the configured mbx cache unchanged. All daemon-facing tests use their existing isolated configuration and data directories. Run `python3 tests/e2e/session_move.py --hel "$PWD/target/debug/mj" --worker "$PWD/target/debug/mj-worker" --instance restart-handoff-acceptance-test` after `cargo build -p brokk-mjolnir -p brokk-mj-worker`. Review `git diff --check`, stage only changed files, commit on the current branch and push `HEAD:master`.

## Validation and Acceptance

A Restart with a workspace file exceeding the full-checkpoint size limit must seal and restore successfully, without including the file in its handoff. The full checkpoint reference and archive must survive. A newly sealed compatible Restart must keep staged native state, while failure recovery must be able to restore it. Existing automatic-close, refused-close, cancellation and migration regressions must pass.

## Idempotence and Recovery

The Restart intent is the sole owner of its handoff. Persist ownership before sealing; retain that archive across daemon replacement and until the intent finishes. No test changes the default instance or live store. Repeat focused tests safely using their isolated fixtures.

## Artifacts and Notes

Initial evidence: Restart calls `suspend_session_for_restart` with full `CheckpointExportPolicy::Always`; native retention currently requires `InPlaceRestoreMode::Move`.

## Interfaces and Dependencies

Use existing `CheckpointMetadata`, checkpoint export, relay sealing and in-place restore APIs. Add no workspace crate or external dependency. Schema revision and minimum compatible revision advance together for the new Restart-owned persisted format.

Revision note: initial plan records archive ownership and recovery requirements discovered during tracing.

Revision note: implementation completed; added real daemon/worker acceptance coverage and recorded the first focused validation result.

Revision note: recorded full controller, Clippy and formatting results; clarified milestones and the exact isolated acceptance command.

Revision note: recorded the acceptance fixture isolation failure and its correction; rerunning only the affected Python acceptance scenario.

Revision note: the user asked to DRY the implementation. Move now selects the shared close state machine for handoffs, including the same refusal, sealing and recovery behavior as Restart. Its dedicated 97-line handoff sealer is deleted. The first real-worker acceptance passed before this refactor; focused and final validation are running for the shared implementation.

Revision note: all 177 Move-related regressions passed with the shared close state machine. Preparing a coherent implementation checkpoint and integrating concurrent upstream changes before final validation.
