# Import Move history once and restore refs in batches

This ExecPlan follows `.agents/PLANS.md` and must be kept current as implementation and validation proceed.

## Purpose / Big Picture

Moving a session must not fetch the same local Git bundle once for every branch, tag, or internal reference. Session e6d12f2023afb0682d3116fc140432dc had 1,823 saved references and was repeatedly fetching its 295 MB bundle inside the destination container. Import all bundled objects once, then restore the saved references in bounded numbers of Git commands, preserving the exact repository state.

## Progress

- [x] (2026-11-04 18:58Z) Traced the live delay to the per-reference fetch loop in `mj-worker/src/move_workspace.rs::restore`.
- [x] (2026-11-04 19:02Z) Checked Git's transaction protocol and reproduced the restriction on deleting a reference prefix and creating its child within one transaction.
- [x] (2026-11-04 19:03Z) Extended the real-Git roundtrip to 2,048 extra references; the old restore failed at its first individual scale-ref update.
- [x] (2026-11-04 19:04Z) Imported the bundle once, batched deletions and direct-ref updates, and passed the focused regression. Git tracing confirms one import and zero fetches.
- [x] (2026-11-04 19:08Z) Passed all 762 worker tests (10 existing ignored tests), workspace clippy, and formatting. Implementation is ready for the required commit, review and push; Git and the review gate record publication status.

## Surprises & Discoveries

Git cannot prepare one reference transaction that deletes `refs/notes/prefix` and creates `refs/notes/prefix/child`: it reports that the prefix exists. Move previously deleted destination-only refs before creating saved refs. Preserve that order with two batch transactions, rather than losing support for legitimate namespace changes.

Sub-agents are disabled for this session; the parent owns implementation and validation after a rejected spawn.

## Decision Log

- Decision: Use `git bundle unbundle` once for each repository, instead of fetching individual reference tips.
  Rationale: The capture already stores a complete bundle containing HEAD, saved references, and stash history. Unbundle imports the full object set without changing refs or repeating fetch connectivity work.
  Date/Author: 2026-11-04 / Codex.
- Decision: Delete destination-only refs in one transaction, then replace saved direct refs in one transaction. Keep symbolic-reference restoration afterward for compatibility with older Git versions.
  Rationale: Two fixed-size command sequences preserve namespace changes and avoid requiring newer symbolic-reference transaction commands. Reference count affects input size, not the number of fetch or update processes.
  Date/Author: 2026-11-04 / Codex.
- Decision: Use NUL-delimited `git update-ref --no-deref --stdin -z`, reject embedded NULs in manifest ref names, and use the shared subprocess helper.
  Rationale: Ref names remain data, symbolic refs cannot redirect direct writes, and concurrent pipe draining handles transactions larger than 64 KiB.
  Date/Author: 2026-11-04 / Codex.

## Outcomes & Retrospective

Implementation and validation are complete. Restore now imports the entire bundle once, removes obsolete refs in one transaction, and sets captured direct refs in another transaction. The regression also proves a transaction larger than 64 KiB and exact preservation of direct refs, symbolic refs, annotated tags, stash history, edits and selected files. This changes future Move restores; it does not replace or interrupt an already running helper. The original bottleneck was redundant Git work, and the regression protects the number of bundle imports rather than relying on a fragile elapsed-time threshold.

## Context and Orientation

`mj-worker/src/move_workspace.rs` captures Git state and workspace files into a disk-backed transfer and restores it at the destination. A Git reference is a name pointing to an object; symbolic references point to another reference. `capture` saves those names and hashes in a version-1 manifest plus a complete `history.bundle`. `restore` verifies hashes, imports history, replaces refs, restores symbolic refs, checks out the saved branch and restores edits. `mj-core/src/subprocess.rs::run_with_input` feeds stdin while draining stdout and stderr concurrently. There are no schema or wire-format changes.

## Plan of Work

Extend `disk_backed_move_preserves_git_state_and_selected_large_files` in the existing test module. Create 2,048 extra refs using real Git, add an annotated tag and destination-only symbolic and prefix refs, and install a real Git reference-transaction hook in the isolated target repository that rejects any partial update of the scale refs. The existing whole-state comparison must still pass. The hook also records evidence that one update transaction exceeded 64 KiB. Run the test inside its own child test process with `GIT_TRACE2_EVENT` set only on that process; inspect real Git command events to require one unbundle and zero fetches without changing concurrent tests' environment.

Refactor the small Git runner to accept input while retaining the existing empty-input convenience function. Replace the restore fetch loop with one unbundle. Build deletion and update command streams using the NUL-delimited Git protocol, obtaining membership from a set rather than repeated linear scans. Do not alter capture or persisted formats.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir/.mj/clones/ae44f27f243d94241a813fca13340393`. Use the existing mbx Cargo wrapper and normal build layout. All Cargo tests run with elevated sandbox permissions; their existing temporary directories isolate store and worker state. No new build is pointed at the default instance.

Run `cargo test -p brokk-mj-worker disk_backed_move_preserves_git_state_and_selected_large_files` before and after the restore change, keeping logs under `/mnt/optane/tmp`. The old restore must fail the hook's batch requirement. Then run `cargo test -p brokk-mj-worker` and `cargo clippy --all-targets -- -D warnings` once for final validation, plus `cargo fmt --all -- --check`. Rerun only individual failures when necessary.

Commit the changed module and this plan on the current branch, request the required before-push review, address any valid findings, and push HEAD to origin/master. If upstream moved, fetch and merge instead of rebasing.

## Validation and Acceptance

The existing real-Git roundtrip must preserve HEAD, all direct and symbolic refs, annotated tags, internal tree refs, the stash reflog and earlier stash objects, the index, staged and unstaged edits, large untracked files and exclusions. It must restore all scale refs in one transaction larger than a pipe buffer, and handle deletion of an old reference whose name is a prefix of a restored reference. The full worker suite and workspace clippy must pass in the dev profile.

## Idempotence and Recovery

Tests create temporary repositories and never use the live session's checkout. Move's durable state and retry behavior remain unchanged. Unbundle may be repeated safely; the deletion and update batches deterministically restore the captured namespace. Failed Git commands propagate errors. Do not stop or replace the live Move to exercise this change.

## Artifacts and Notes

Live evidence: destination helper progressed from saved ref 341 to 371 while running a separate `git fetch --no-tags <history.bundle> <hash>` each time. The bundle manifest contained 1,823 refs. The defective loop originated in commit `0cffddc616`.

Validation logs are `/mnt/optane/tmp/mj-move-restore-baseline.log` (old code rejected by the transaction hook), `/mnt/optane/tmp/mj-move-restore-focused.log` (focused regression passes), `/mnt/optane/tmp/mj-move-restore-worker-tests.log` (762 passes, 10 ignored), and `/mnt/optane/tmp/mj-move-restore-clippy.log` (clean dev-profile workspace check). Formatting and `git diff --check` also pass.

## Interfaces and Dependencies

Keep `restore(source: &Path, repositories: &[WorkspaceRepository]) -> Result<()>` and the version-1 manifest unchanged. Add no crates. Use the existing shared subprocess helper and Git plumbing (`bundle unbundle`, `update-ref --stdin -z`, `symbolic-ref`).

Revision note: Initial plan records the constant-command restore design and the Git namespace constraint found during investigation.

Revision note: Completed implementation and validation; recorded the failing-before evidence, process-scoped Git trace assertions and final check results.
