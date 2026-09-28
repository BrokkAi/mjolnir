# Move-only workspace transfers and file selection

## Purpose / Big Picture

Switching profiles in an unchanged environment must not package repository
contents. Rebuilding moves must support large nonignored files without storing
them in recovery checkpoints. Before moving at least 1,000,000,000 bytes, the
user gets a separate file-selection page with sizes on every checkbox.

## Progress

- [x] Audited the existing Move, checkpoint, restore, and wizard boundaries.
- [x] Implement Move-owned handoff and persistent transfer/source ownership.
- [x] Implement Move-only inventory, file-backed Git capture, transfer and verification.
- [x] Implement preparation, exclusions, large-transfer acknowledgement and cleanup.
- [x] Implement shared size tree and TUI, web, and CLI interfaces.
- [x] Validate isolated lifecycle, transfer, upgrade and UI regressions.
- [x] Commit the validated implementation on the current branch.

## Surprises & Discoveries

Container resource names and workspace volumes were derived solely from the
session ID, and Docker cleanup selected every volume with that session label.
Move now uses an operation-specific generation for destinations, recorded in
the locator; cleanup selects that generation and retains shared host caches.
Move bypasses rebuilding the source's session-scoped Git cache while it remains
retained. This is required for two environments to coexist safely.

The first full suite passed 1,963 controller tests and exposed eleven failures:
four schema-floor expectations, two incomplete old preparation fixtures, and
five old tests that expected unchanged handoff artifacts to be reused. Move writes
a metadata-only handoff rather than reusing an old full archive, which could
otherwise copy gigabytes of unused repository payloads. The recovery checkpoint
remains separate. The fixtures now exercise that handoff directly.

The reported morannon session has 18.55 GB of nonignored untracked files,
principally binaries in .agents/qualification/*/staged. Its ordinary checkpoint
fails at the unchanged 8 GiB payload limit. Current Move packages these files
even when restore_repositories is false. Existing MetadataOnly capture already
supports saving native state without workspace payloads.

## Decision Log

The Move helper streams to a unique temporary file in its own worker directory
and publishes through rename. It never reuses checkpoint upload staging. This
prevents concurrent preparation from exposing partial executables or overwriting
checkpoint specifications. The concurrent-upload test drives over 512 KiB through
the shared subprocess stdin path.

Rsync handles all temporary Move copies, including local copies, so restart and
partial-file behavior have one implementation. It must be installed on both
endpoints; managed container images include it. Storage checks combine logical
allocations sharing a filesystem and check container backing storage and custom
Podman workspace roots. Estimates include history, tracked checkout and edits.

New EC2 destinations are refused during preflight: their actual tool availability
and free space cannot be inspected before provisioning. An existing EC2 instance
can be used through an SSH target. Rebuilding between bare target aliases that
share worker storage is also refused; keep current resources for an in-place
profile switch. Split indexes and skip-worktree/assume-unchanged flags require
normalization before a rebuilding Move, with instructions before interruption.

A sealed Move retries its original destination and file selection. Changing
them would invalidate its captured manifest and retained-source identity.

Move copies release daemon upgrade admission only after their owning operation
is durable. They reacquire admission before control transitions. An upgrade
interruption keeps the Move phase active for restart recovery rather than
recording user cancellation. Explicit user cancellation remains distinct.

Retained sources have an independent durable table and an explicit CLI:
`mj move-sources --session ID` inspects them; `--cleanup OPERATION --yes` removes
one. Sources do not expire automatically. No live default store is migrated by
implementation or tests.

All large-file accommodation is exclusively Move-owned. No changes to
mj-checkpoint payloads, archives, capture policies, size limits, or retention.
Existing metadata-only handoff APIs may be reused, but a handoff must not be
published as a full recovery checkpoint. Transfer all means the existing
eligible file set, excluding ignored and conventional credential paths.

The threshold is decimal 1 GB inclusive. The separate Choose files page appears
automatically above that threshold and is reachable from review for smaller
rebuilding moves. It is skipped in place. All files start included. Exclusions
apply only to untracked paths and do not change ignore rules. Explicit
exclusions retain the stopped source until confirmed cleanup; ordinary ignored
files retain existing cleanup behavior. Installed packages and external mounts
are not migrated.

## Outcomes & Retrospective

The isolated in-place regression leaves a 9 GiB file untouched and produces a
session-only handoff under 1 MiB. The focused Move suite passed 46 tests. The
subsequent full run passed 1,981 controller tests, 854 TUI tests and 670 worker
tests; its only failure was missing CLI argument help, now fixed. The allocated (non-sparse) >8 GiB transfer acceptance passed in 148 seconds.
The final focused run passed 106 controller, 40 TUI and 16 worker Move-related
tests, including sparse-file restore and index preflight. The complete final
`cargo test` run passes, including isolated automatic-upgrade and PTY regressions.
A prior terminal auto-reexec timeout passed on its isolated rerun and in the final
full run; no unrelated test code was changed. The five final transport tests pass,
including concurrent atomic helper uploads, namespace adapters and partial-file
repair. `cargo clippy --all-targets -- -D warnings`, JavaScript syntax and diff
whitespace checks also pass.
No live session or default-store changes have been made.

## Context and Orientation

mj-controller/src/controller/move_session.rs owns durable lifecycle intent.
Its current close path in lifecycle.rs writes a full checkpoint to the session
record and destroys the source before provisioning the destination. Replace
that coupling for new moves with independent handoff and workspace ownership.
resume/in_place.rs already keeps files but consumes the full archive. Ordinary
Resume and checkpoint operations must preserve their existing behavior.

mj-core/src/state/session_move.rs defines shared Move wire records. The TUI's
ResumeWizard and web viewer move form currently combine preparation with final
review. Add an explicit file-selection step and shared inventory/tree semantics.
The CLI currently prints preparation then submits Move; expose the same
inventory and explicit acknowledgement/exclusion options there.

## Plan of Work

First give Move a durable handoff and ownership model independent of full
checkpoint metadata. Record exact source and destination identities and keep
the source until verified destination readiness. Persist transfer boundaries
so cancellation and daemon restart resume the same operation. Classify the
forward database migration as breaking because older daemons cannot preserve
the new ownership invariants.

Build workspace inventory and transfer utilities in Move-specific modules.
Git bundles and patches must be files produced with shared subprocess helpers;
large file bodies must never enter checkpoint RepositorySnapshot or Vec<u8>.
Use authenticated SSH rsync and container execution adapters, controller staging
for two remote endpoints. Use the same rsync staging contract for local and
container copies, with dependency checks before interruption.
Check dependencies and all staging storage before sealing. Verify file hashes
and full Git state before starting destination work. Remove successful staging;
retain explicit-exclusion sources for independent, confirmed cleanup.

Preparation runs in supervised background work and reports bytes, largest
contributors, storage needs, and blockers before final confirmation. Directory
checkboxes include aggregate sizes and file rows include individual sizes.
Collapse single-directory chains; sort descending by size then path. Initially
show roots at least 100 MB and expand substantial child directories while at
most eight substantial rows fit. Group smaller entries and additional roots
in expandable remainder rows; individual files are always reachable. Parent
checkboxes use mixed state and navigation never changes selections. Track
selections by repository-relative paths, not rendered row indexes.

## Concrete Steps

Work on the current master branch in /home/jonathan/Projects/mjolnir. Preserve
unrelated files. Do not push or install. Format only changed Rust files. Run
cargo test outside the sandbox and use the dev profile and normal mbx storage.
Use --instance move-transfer-test for direct runtime invocations and existing
isolated configuration/data directories in tests.

Required validation commands are cargo test and cargo clippy --all-targets --
-D warnings. Use focused package tests during milestones, then the full suite.

## Validation and Acceptance

An in-place move with over 8 GiB of untracked data must perform no repository
payload capture. Rebuilding moves must transfer beyond ordinary checkpoint
limits with bounded memory and exact Git HEAD, refs, stashes, index, dirty and
untracked state. Normal checkpoints retain their old rejection behavior.

Test just below, at and above 1 GB, the reported qualification-directory layout,
many roots, mixed selections, labels containing file sizes and selection
preservation across expansion/navigation. Exercise interrupted copying, daemon
restart, lost acknowledgement, cancellation, insufficient disk, missing
transport, concurrent source changes, failed startup and retained-source cleanup.
Old daemons must refuse the migrated isolated store; upgrades must preserve
durable accepted work and remain responsive.

## Idempotence and Recovery

Each operation owns its source, staging and destination identities. Never use
the session's current target as the cleanup target for an older operation.
Never discard a source before verification and readiness. Explicit exclusions
leave a discoverable stopped source with no automatic expiry. Queue admission
uses original durable command IDs after readiness. A handoff is not a portable
workspace backup and cannot authorize ordinary Resume to recreate files.

## Interfaces and Dependencies

Extend shared Move selection/preparation with inventory, explicit exclusions,
large-transfer consent, and blockers. Persistent ownership belongs to Move,
not checkpoint history. Source and destination worker capabilities and rsync
availability must be checked before interruption. Existing ordinary checkpoint
wire formats and limits remain unchanged.

## Validation evidence

The final complete suite is recorded in `/tmp/mj-move-validation.log`. The
allocated >8 GiB acceptance is in `/tmp/mj-move-large3.log`; it passed in 148.17
seconds. The five latest transport tests are in `/tmp/mj-move-transport-final.log`.
The final linter output is `/tmp/mj-move-verified-clippy.log`. These tests use
isolated storage; there was no deployment or live-session Move.
