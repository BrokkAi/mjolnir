# Reuse the in-place Move path for Restart

This completed ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Restart now seals a small session handoff while keeping workspace files, build caches, the staged harness home and the installed worker in place. Move and Restart use the same close state machine and in-place restore implementation. The last complete recovery checkpoint remains usable.

## Progress

- [x] (2026-10-28) Traced Restart, Move, archive retention and native-home restoration.
- [x] (2026-10-28) Added schema 102, durable Restart handoff ownership, shared export policy and native retention.
- [x] (2026-10-28) Validated the initial implementation with 64 Restart-related tests, the full controller suite, Clippy and real-worker acceptance.
- [x] (2026-10-28) Responded to the user's DRY request by removing Move's separate 97-line sealer and using the existing close state machine for both operations. All 177 Move-related regressions passed.
- [x] (2026-10-28) Committed implementation as `16d8edbe7` and merged concurrent upstream changes as `60e9e02bd` before final validation.
- [x] (2026-10-28) Final validation passed: 2,188 controller tests, 18 ignored; workspace Clippy, formatting, Python compilation and real local-worker acceptance passed.

## Surprises & Discoveries

Move already stored a separate handoff archive in its durable operation. Restart stored only the digest of a full session checkpoint. Replacing that checkpoint with repository metadata alone would lose the last full recovery copy, so Restart needs its own archive reference.

The real-worker scenario initially timed out before Restart because the setup TUI imported host profiles, and delegation validation probes every enabled profile. The fixture now detaches its TUI before editing the profile list, so its discovery task cannot add profiles afterward. Only the two deterministic ACP profiles remain enabled. The scenario also waits for accepted background child destruction to remove each child before changing delegation.

## Decision Log

- Decision: add a nullable handoff column with breaking schema revision 102.
  Rationale: older daemons cannot recover or retain an archive owned by Restart. The compatibility floor and migration revision advance together; legacy intents retain their full-checkpoint recovery behavior.
  Date/Author: 2026-10-28, Codex.
- Decision: share the `SessionHandoff` export policy and existing close state machine between Move and explicit Restart.
  Rationale: capture, publication decisions, refusal handling, relay sealing and failure recovery should have one implementation. The operation's durable intent remains responsible for its archive reference.
  Date/Author: 2026-10-28, Codex.
- Decision: retain native state only on a freshly sealed session with its own staged home.
  Rationale: a failed reset reconstructs state from its archive; a fresh seal still owns its native files. Queue behavior and historical automatic/container recovery remain unchanged.
  Date/Author: 2026-10-28, Codex.

## Outcomes & Retrospective

Both operations now use `Controller::suspend_session_controlled_with_manager` and the existing in-place restore path. Move's separate handoff sealer is deleted. Restart's handoff survives startup archive reconciliation while its intent owns it, and never replaces the full recovery checkpoint.

The final real-worker scenario completed Restart in 2.110 seconds beside a 9 GiB untracked file. It verified the same checkout and staged-home inodes, native session identity, ignored build output and checkpoint reference, then completed subsequent queue discard/replay, suspension and process cleanup checks. The final controller suite passed 2,188 tests with 18 external prerequisites ignored, and workspace Clippy and formatting passed. No implementation or validation work remains.

## Context and Orientation

The daemon owns durable lifecycle operations. `mj-controller/src/daemon/restart.rs` admits Restart and records recovery phases; `database/session_restart.rs` persists its handoff ownership. `controller/lifecycle.rs` captures and seals both Move and Restart through one close state machine. The worker relay is its journal connection, held at a known position during capture and sealing. `controller/checkpoint/latched.rs` implements the metadata-only handoff export policy. `controller/resume/in_place.rs` restores into an existing environment and retains a compatible staged native harness home. `controller/checkpoint/archives.rs` retains archives owned by unfinished Move or Restart intents during startup cleanup.

## Milestones

The first milestone added durable Restart ownership and the shared export policy. The isolated oversized-workspace regression demonstrates that Restart preserves the full checkpoint and native-home inode, and that startup sweeping keeps its separate handoff until the intent finishes. Migration coverage verifies rollback, preservation of legacy intents and refusal of older recovery code.

The second milestone unified the close state machine at the user's request. Focused Move tests cover handoffs, interrupted sealing, retained targets, retries, queues and workspace transfers. Final full-suite and real-worker validation after merging upstream confirmed that the shared implementation preserves those behaviors.

## Concrete Steps

Run these commands from the repository root using the existing mbx configuration without redirecting build storage:

    cargo test -p brokk-mj-controller move --no-fail-fast
    cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    cargo build -p brokk-mjolnir -p brokk-mj-worker
    python3 -m py_compile tests/e2e/session_move.py
    python3 tests/e2e/session_move.py --hel "$PWD/target/debug/mj" --worker "$PWD/target/debug/mj-worker" --instance restart-shared-close-acceptance
    git diff --check

Cargo tests use their existing isolated configuration and data directories. The acceptance scenario sets its named instance and its own temporary configuration and data directories; never run a test build against the default instance.

## Validation and Acceptance

The focused Move suite passed 177 tests. Final controller validation passed 2,188 tests with 18 ignored and no failures. Clippy and formatting exited successfully. The real-worker acceptance reported `move acceptance passed: real local-bare workers, deterministic ACP` and recorded Restart at 2.110 seconds. Its 9 GiB file would exceed a full checkpoint's file limit; successful bounded export and inode preservation demonstrate the requested optimization.

## Idempotence and Recovery

The Restart intent is the sole owner of its handoff. Ownership is persisted before sealing, retained across daemon replacement and released when the intent finishes. Full recovery archives and their projection retention remain separate. A failed restore retains the environment for retry. Every automated scenario uses isolated data and stops its owning processes before removing files.

## Artifacts and Notes

Implementation checkpoint: `16d8edbe7`. Upstream integration tested: `60e9e02bd`. The final acceptance trace was written under `target/reliability-artifacts/session-move-seed-1-56428/trace.json`; target artifacts are disposable, so the result and reproduction commands are recorded here.

## Interfaces and Dependencies

The change reuses `CheckpointMetadata`, checkpoint export, worker relay sealing and in-place restore. It introduces no crate or dependency. Schema 102 adds `session_restart_intents.handoff_json`, classifies it as breaking for older recovery code and retains all shipped forward migrations.

Revision note: completed the user's DRY follow-up, integrated upstream, and recorded final validation and acceptance evidence.
