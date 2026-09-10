# Prevent upload permission failures and retain launch errors

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Users must launch the default non-root image without permission errors. Failed launches must remain visible in a scrollable TUI dialog with recovery guidance and a safe retry using the original settings.

## Progress

- [x] (2026-09-10) Traced chmod failure to container copy ownership and added correction before initial and replacement worker permission changes.
- [x] (2026-09-10) Retained creation requests and added persistent, scrollable recovery dialogs with explicit dismissal and safe retry.
- [x] (2026-09-10) Full Cargo suite, focused worker and TUI tests, live Docker regression, rustfmt, clippy, and diff checks passed.
- [x] (2026-09-10) Prepared the validated implementation and plan for the required repository commit.

## Surprises & Discoveries

Successful rollback removes provisional sessions, so row-based recovery cannot show their errors. Shared status notices can replace the only remaining error. The default image uses USER hel, while container copies may create root-owned files.

## Decision Log

Assign uploaded files the ownership of the directory created by the worker user, using root only for chown. Retain the creation action through background completion. Offer retry only when rollback removed the provisional record. Preserve any modal the user was editing when failure arrived. Keep full errors scrollable and do not retry automatically. Use numeric ownership from `stat -c` and POSIX shell argument forwarding rather than GNU-only chown options, preserving BusyBox compatibility. Apply the same helper to reviewer profile uploads. Document the image tool requirements in `docs/src/content/docs/custom-images.md`.

## Outcomes & Retrospective

The default agent image passed a disposable Docker test that installs and replaces the worker, then verifies executable and private-profile access as the non-root user. The full suite passed; focused worker tests passed after the final portability adjustment. Dialog tests prove persistent visibility, exact retry settings, scrolling to the final diagnostic line, and restoration of an interrupted modal. Podman and SSH runtimes were not exercised live. No release or installed application update is included.

## Context and Orientation

`mj-controller/src/hel_controller/worker_binary.rs` installs workers and profiles. `mj-cli/src/dashboard/io.rs` creates sessions in background tasks and applies lifecycle results. `mj-cli/src/dashboard/actions.rs` dispatches TUI actions. `mj-tui/src/dialogs.rs` renders modals and handles their controls.

## Plan of Work

Correct local and SSH container ownership before chmod, including replacements. Carry the creation action through registration and failure updates. Add a launch-failure confirmation with full wrapped details, scrolling, dismissal, and retry after successful rollback. Test persistent visibility, retry settings, dismissal, and cleanup-failure behavior.

## Concrete Steps

From `/Users/ryansvihla/code/mjolnir`, run `cargo fmt --all`, elevated `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `git diff --check`. Stage only changed files and commit to the current branch without pushing.

## Validation and Acceptance

Root-owned copied executables and private profiles become usable by the non-root worker. Failure dialogs survive unrelated notices, long errors can be scrolled, retry dispatches original settings once, dismissal launches nothing, and incomplete cleanup offers no duplicate creation. Required Rust checks pass.

## Idempotence and Recovery

Ownership correction is repeatable and limited to uploaded paths. Existing rollback remains responsible for teardown. Retry uses fresh session identifiers and existing supervised background work. Failed cleanup records remain available for removal.

## Artifacts and Notes

Observed: chmod changing permissions of `/var/lib/hel/workers/<session>/hel`: Operation not permitted.

## Interfaces and Dependencies

Reuse CommandSpec, ssh_command_spec, ConfirmDialog, DashboardAction, and existing lifecycle updates. No new dependencies.

Revision note: The user expanded the fix to include prevention and understandable recovery.

Validation evidence: `cargo test` exited 0; `cargo test hel_controller::worker_binary` passed 52 tests; `cargo test -p brokk-mj-tui launch_failure` passed 2 tests; the explicitly enabled Docker upload/replacement test passed; `cargo clippy --all-targets -- -D warnings` exited 0.

Revision note: Completed implementation and validation; expanded ownership repair to reviewer uploads and used GNU/BusyBox-compatible tools. Implementation and validation are complete; this plan accompanies the repository commit. Publication and installation are outside this change.
