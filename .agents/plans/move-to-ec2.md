# Enable Move to EC2

This is a living ExecPlan maintained under `.agents/PLANS.md`.

## Purpose / Big Picture

Move a running container session to a configured EC2 target without manually creating an SSH target. Preview must create no billed resources. Confirmation prepares the instance while the source runs, then checkpoints and transfers the session. Preparation failure cleans up the destination and keeps the source alive. Session identity, transcript, Git state, file selection and queue handling use the existing Move contracts.

## Progress

- [x] (2026-10-01) Investigated Move, provisioning, recovery and control surfaces; user chose creation after confirmation and automatic cleanup on preparation failure.
- [x] Add durable preparation ownership and atomic breaking migration 71.
- [x] Implement destination creation, checks, adoption, bounded cancellable cleanup and restart reconciliation.
- [x] Integrate preview, resource defaults and progress across CLI, TUI and web.
- [x] Add isolated behavior and upgrade regressions; pass full dev tests, clippy, web checks and prepare the required commit.

## Surprises & Discoveries

The EC2 refusal is an unconditional blocker in `mj-controller/src/controller/move_session/transfer.rs`. Existing workspace Moves retain source storage until destination readiness, but stop the source before provisioning. `MoveOperation.destination_target` already means a switched destination and causes source-stop skipping, so it cannot also represent prepared infrastructure. AWS locator discovery waits for boot and SSH before the instance ID reaches the session record. Source children currently stop at lifecycle admission, before destination checks. Inherited container sizing is invalid for EC2. Schema revision at planning is 70; older readers deny unknown Move JSON fields and can silently skip undecodable operations.

## Decision Log

Decision: Create only after confirmation and automatically clean up failures before sealing the source. Rationale: user-selected defaults avoid preview charges and abandoned instances. Date/Author: 2026-10-01, user and Codex.

Decision: Give prepared destinations their own durable state machine and preserve immutable launch parameters and a per-attempt client token. Rationale: one owner must account for instances across lost acknowledgements and daemon replacement. Date/Author: 2026-10-01, Codex.

Decision: Use destination defaults for incompatible inherited container sizing, preserving explicit EC2 allocations. Rationale: normal cross-target Move should not require an obscure clearing flag. Date/Author: 2026-10-01, Codex.

## Outcomes & Retrospective

Implemented and validated on hel4 after integrating origin/master. Preview launches no resources, inherited container sizing resolves to EC2 defaults, preparation runs before source interruption, and resource ownership persists through launch, adoption, cleanup and daemon handoff. No live AWS resources or default-instance store changes are authorized for testing.

## Context and Orientation

The daemon is the controller process owning the database and lifecycle tasks; workers run agent turns independently. `mj-core/src/state/session_move.rs` defines Move requests and durable operation state. `mj-controller/src/controller/move_session.rs` validates confirmation and seals the source. Its `transfer.rs` module assesses capacity, captures Git and files and restores workspace data through rsync. `mj-controller/src/controller/provisioning.rs` creates infrastructure, then installs repositories and workers; `backend.rs` discovers EC2 instances and SSH addresses. `mj-controller/src/daemon/session_move.rs` supervises Move and restart recovery. Upgrade admission is the bounded daemon-work gate: long resumable preparation must release its hold, while durable control transitions reacquire it. Clients render typed preparation in `mj-cli/src/main.rs`, `mj-tui/src/wizards/render.rs` and `mj-controller/src/web/viewer.js`.

## Plan of Work

Milestone 1 introduces `MovePreparation.destination_checks` (checked or after provisioning), a defaulted prepared-destination record in MoveOperation, accepted confirmation information and schema revision 71. The migration raises minimum compatibility because old readers cannot preserve new JSON ownership. Old records remain decodable under the new binary.

Milestone 2 implements a controller preparation module beside Move transfer. Persist the exact AWS launch command with immutable template ID and numeric version, tags and stable client token before launch, and persist the instance ID before boot or SSH waits. Use created, checked, adopted, cleanup-pending and released transitions. Reconcile lost launch acknowledgements by client token and managed tags. Share target creation, discovery, Git bootstrap and exact termination with existing provisioning. Bootstrap supported rsync and probe actual destination disk capacity before source sealing. Adopt the prepared instance in resume instead of running another launch. No destination harness starts before checkpoint transfer. Stop children only after destination preparation succeeds. Acquire the source relay lease after long preparation, recheck fingerprint, queue and interruption acknowledgement and validate current workspace selection before sealing.

Milestone 3 integrates failure and restart behavior. Before seal, cleanup leaves the source record and workers running. After seal, preserve the handoff and source environment for retry. Persist cleanup failures and retry them with backoff without holding upgrade admission; no new operation may overwrite unresolved resource ownership. Destroy and resource recovery recognize prepared instances. Handoff interruption retains active durable phases for automatic continuation and never counts as user cancellation. Existing queue admission boundaries remain authoritative.

Milestone 4 updates all previews and progress. Preview checks AWS configuration but does not launch anything, marks destination checks deferred and explains they run after confirmation. Normalize incompatible inherited container sizing centrally to destination defaults, preserving explicit sizing and existing attachments and file consent. Progress and cancellation use supervised lifecycle state and remain nonblocking.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir4` on its current branch. Use normal Cargo storage and its existing mbx configuration; do not redirect targets. Format with `cargo fmt --all`. Run every `cargo test` outside the restricted sandbox. Run focused new tests, then `cargo test` and `cargo clippy --all-targets -- -D warnings` on the dev profile. Use the existing isolated test helpers and a named `move-ec2` instance for any binary invocation. Review the diff, stage only task files and commit on the current branch and push `HEAD:master` to origin, as explicitly requested.

## Validation and Acceptance

Use hand-written AWS/SSH executors, existing real Git test fixtures and transferred data above 64 KB. Preview must launch zero instances. Confirmed Move must use one created instance, preserve identity, transcript, HEAD, staged and unstaged changes, stash, selected files and queue policy. Inject failures at launch, boot, SSH, bootstrap, disk probes, confirmation recheck, transfer and cleanup. Before seal the source stays running; after seal it retains recovery data. Restart at each durable transition including a lost launch acknowledgement must neither duplicate instances nor replay accepted queue entries. Verify unresolved cleanup stays visible and owned and orphan scans cannot expose it for conflicting destruction. Verify daemon handoff and unrelated sessions remain responsive during slow preparation. Exercise CLI, TUI and web preparation defaults. Retain existing Move and isolated schema/upgrade regressions.

## Idempotence and Recovery

Each AWS launch attempt gets a persisted client token of at most 64 ASCII characters and frozen launch parameters. Retrying that same attempt uses the same request. A user retry after confirmed cleanup creates a fresh attempt. Ambiguous launch cancellation never starts an instance merely to discover it: query by token with backoff and retain unresolved cleanup ownership. Persist exact instance identity and access settings through termination. Only confirmed absence releases ownership; cleanup failure names the resource and does not overwrite the original error. Accepted Move can resume after daemon replacement from its durable phase. Automatic tests never call actual AWS APIs or migrate the default store.

## Artifacts and Notes

Focused Move suite: 46 passed before upstream integration. Integrated 13 upstream commits by fast-forward on hel4, preserving its new worker lifecycle ownership around Move, rollback and destruction. Added tests for lost launch acknowledgements, daemon handoff during boot, automatic cleanup failure and retry, insufficient disk, adoption, orphan ownership, new attempts after cleanup, preview defaults and configuration edits during boot. Migration tests verify atomic revision/minimum compatibility changes and preservation of existing source and Move rows. Final dev validation passed. Initial full run found three historical schema fixtures assuming floor 69; updated their historical floor metadata and final floor expectations for breaking revision 71. Web unit checks (8 files), syntax, formatting and diff checks passed. Final-source clippy passed with warnings denied. Preserve unrelated `scripts/__pycache__/` from the initial working tree.

## Interfaces and Dependencies

Use existing CommandSpec/CommandExecutor helpers for all child processes, ProvisionStageGuard for progress, MoveMutationGuard for conflicting mutations, MoveSourceRelay for checkpoint admission and TargetRuntimeSettings for durable access. Keep prepared infrastructure separate from the running SessionRecord until adoption. Reuse existing package bootstrap and cleanup helpers rather than independent implementations. No new crate or external dependency is planned.

Plan created 2026-10-01 from the approved conversational plan.

Decision: Clear a failed prior destination before preparing a retry. Rationale: resource acquisition must follow the rollback decision; a new instance must never be mistaken for the failed previous destination. Date/Author: 2026-10-01, Codex.

Decision: Cleanup respects supervisor cancellation and is bounded to 15 seconds per command, with durable background retries. Rationale: cancellation and daemon shutdown remain responsive while ownership is retained. Date/Author: 2026-10-01, Codex.

Decision: Release a refused launch only for a newly created attempt executed in that call. Rationale: refusal of an idempotent retry does not prove the previous request was never accepted; a lost acknowledgement retains ownership until reconciliation and termination. Two regressions cover first-call refusal and refusal after a previously accepted launch. Date/Author: 2026-10-01, Codex.

Validation update: the full dev suite passed after fixture corrections; the passing final full suite and clippy rerun include the template-name replacement and refused-retry regressions. Launch recovery freezes the resolved template ID, rather than a mutable template name. Final code is frozen for validation.

Final validation (all exit 0): `cargo test -- --test-threads=16`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `node --check mj-controller/src/web/viewer.js`, eight web unit-test files, and `git diff --cached --check`. Cargo tests ran outside the sandbox with isolated data/instances. Reduced test concurrency avoids overloading the shared host. AWS behavior was tested with hand-written executors; no live AWS resources were created. The user explicitly authorized pushing this completed implementation to origin/master.
