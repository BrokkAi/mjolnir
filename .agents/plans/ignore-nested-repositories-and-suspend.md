# Offer Ignore and Suspend after a nested repository blocks checkpointing

This ExecPlan is maintained under `.agents/PLANS.md`.

## Purpose / Big Picture

When a nested Git repository prevents suspension, the TUI will replace Retry suspension with Ignore and Suspend. Choosing it takes a fresh, verified checkpoint that omits nonignored, untracked nested Git repositories, then completes ordinary suspension. The dialog states that those directories cannot be recovered. Other checkpoint errors retain Retry suspension. Ordinary checkpoints remain strict.

## Progress

- [x] (2026-10-30) Trace the failure dialog, background client action, serialized daemon lifecycle, and target archive collection.
- [x] (2026-10-30) Implement the explicit suspension option through the existing lifecycle and target export.
- [x] (2026-10-30) Add target export/restore coverage for nested linked worktrees and independent clones; the focused real-Git test passes.
- [x] (2026-10-30) The dialog golden and isolated interrupted-close export-failure regression pass; reviewed both rendered layouts. Initial workspace Clippy and formatting pass.
- [x] (2026-10-30) Merge current upstream 2.63.1 without conflicts; final merged-source Clippy and formatting pass.
- [x] (2026-10-30) Complete all affected-crate suites, remaining CLI integration checks, documentation checks, workspace Clippy, and formatting.
- [x] (2026-10-30) Review and commit the implementation checkpoint on the current branch; merge upstream.
- [ ] Push the final validated commits to origin/master.

## Surprises & Discoveries

Git reports an untracked nested repository as one directory ending in a slash. The tar writer deliberately refuses it. The daemon normally routes suspension through interrupted-close recovery because it persists Closing before admission. Both routes must carry the explicit option. Suspension uses single-shot target export while holding its relay barrier; ordinary capture/pack checkpointing needs no new option.

## Decision Log

- Decision: Omit nested repository directories for this explicit suspension only, without editing Git ignore files or bypassing checkpoint verification.
  Rationale: This gives the requested recovery action without changing future checkpoint policy or parsing a diagnostic path into a filesystem mutation.
  Date/Author: 2026-10-30 / Codex
- Decision: The dialog explains that nested Git repositories are omitted as a category. Only that known failure offers this action; unrelated errors retain Retry suspension.
  Rationale: A retry may discover several nested repositories, and the user must understand the scope of the omission.
  Date/Author: 2026-10-30 / Codex

## Outcomes & Retrospective

Implementation is complete. The target export/restore regression passes: both nested repository forms are omitted, staged/unstaged/untracked outer work restores, and Git ignore rules remain unchanged. The dialog golden and isolated close-gate regression also pass. Initial workspace Clippy and formatting pass. Upstream 2.63.1 has been merged without conflicts. Final merged-source Clippy and formatting pass; the only formatting correction was an upstream logging expression. All affected library suites, CLI unit cases, CLI integration suites, and documentation checks are complete. The full run encountered ETXTBSY while launching one existing CLI login fixture; exactly that case passed on isolated rerun without a source change. The remaining CLI integration binaries were executed from the same complete build with isolated MJ_DATA_DIR/MJ_CONFIG_DIR, rather than rebuilding a different Cargo feature combination. No checks remain pending; publication follows the final commit. No live sessions have been changed.

## Context and Orientation

`mj-tui/src/dialogs.rs` owns failure-dialog input and `mj-tui/src/dialogs/render.rs` owns its text and buttons. `mj-cli/src/dashboard/actions.rs` runs daemon calls in supervised background operations. `mj-client/src/daemon.rs` defines daemon requests. `mj-controller/src/daemon/close.rs` admits and supervises the suspension lifecycle. `mj-controller/src/controller/lifecycle.rs` owns checkpoint-before-teardown ordering; `checkpoint/latched.rs` creates the target export specification. `mj-checkpoint/src/checkpoint.rs` defines that specification, `checkpoint/capture.rs` gathers repository snapshots, and `archive/git.rs` selects untracked files for the tar. A nested repository is a Git-reported untracked directory containing a `.git` entry; a submodule is tracked Git state and retains its existing separate rules.

## Plan of Work

Add an explicit daemon action and dashboard action for Ignore and Suspend. Advance the daemon protocol from 68 to 69 to advertise the added action; the frozen management request formats remain unchanged. Pass the choice through the existing serialized close operation, including interrupted-close recovery, to a fresh-export checkpoint policy. Add a default-false, omitted-when-false field to the single-shot checkpoint export specification. Workers that predate it reject the unknown field visibly. Pass that option to repository collection and skip only Git-reported nested repository directories after existing path and symlink-ancestor validation. Do not suppress other filesystem errors or dirty-submodule checks. Normal checkpoints and capture/pack remain strict.

Extend the existing real-Git nested-worktree regression to exercise the explicit option and prove normal dirty and untracked files still restore, and that a later ordinary checkpoint still rejects the nested tree. Extend the existing dialog golden with both checkpoint/no-checkpoint failure variants, warning text, and real input yielding the new action. Keep unrelated retry behavior covered by existing tests.

## Concrete Steps

From the repository root, use normal Cargo/mbx storage. Run Cargo tests outside the sandbox, initially filtered to the affected regression and dialog golden. Review regenerated golden output. After final changes run the complete suites of touched crates and `cargo clippy --all-targets -- -D warnings` in the dev profile, plus `cargo fmt --all -- --check`. Use a named `--instance ignore-nested-suspend` for any manual binary invocation and existing isolated directories for automated tests. Stage only changed files, commit on the current branch, and push HEAD to origin/master; merge origin/master if upstream moved.

## Validation and Acceptance

The nested-worktree regression must still fail collection normally, succeed with explicit omission, restore the outer checkout's work, and remain strict on a subsequent ordinary collection. The whole failure dialog must show Ignore and Suspend and the loss warning only for nested-repository failures, and activating it must produce the explicit daemon action through the background dispatcher. Both ordinary and interrupted suspension must use a fresh verified archive and retain all existing teardown gates.

## Idempotence and Recovery

The option is scoped to one operation and changes no ignore files. Failed exports leave the target intact under existing close recovery. A daemon restart loses the transient consent and retries strictly, allowing the person to choose again. Old target workers reject the unknown export field before doing any collection; the existing schema-mismatch recovery replaces only the installed exporter binary and retries once, preserving the running worker. Extend the existing replacement regression with this new field. Do not alter the live session named in the report.

## Artifacts and Notes

Reported path: `bifrost/.scratch/s47-landing/master-check/`. Original error: `cannot checkpoint nested Git repository directory`.

## Interfaces and Dependencies

Use the existing daemon request enum, background lifecycle operation, controller checkpoint policy, GitCollectionSpec, and checkpoint export specification. No database migration, new dependency, or new crate is needed.

Created 2026-10-30 to implement the user's requested bounded recovery option.

Updated 2026-10-30 after implementation and passing target round-trip validation; recorded the existing automatic exporter upgrade mechanism and daemon protocol revision.

Updated 2026-10-30 after the dialog golden and isolated close-gate regression passed. Validation logs: `/tmp/mj-ignore-suspend-tests.log` (complete touched-crate suites), `/tmp/mj-ignore-suspend-final-clippy.log` (final Clippy), `/tmp/mj-ignore-suspend-controller.log` (close gate), and `/tmp/mj-ignore-suspend-tui.log` (golden).

Updated 2026-10-30: focused target, dialog, and close-gate regressions, final workspace Clippy, and formatting pass. Upstream advanced from 2.62.0 to 2.63.1; stopped only this task's full-suite build before testing, and save the validated implementation as a checkpoint before merging upstream and running final suites.

Updated 2026-10-30 after the clean upstream merge and merged-source Clippy/formatting passed. Confirmed existing managed-worktree teardown also removes explicit disposable nested clones/worktrees without an additional cleanup path. The full-suite log records the passing checkpoint/client suites, old-exporter replacement, and fresh-checkpoint close gate.

Updated 2026-10-30 after validation completed. Full-suite results: checkpoint 165 passed, client 43 plus its integration test, controller 2203 passed, core 553 plus Jev scenarios, TUI 562 passed, worker 727 plus binary/integration tests, CLI 226 initial passes plus the isolated successful rerun. All uncompleted CLI integration suites then passed, including startup/upgrade/store/PTY regressions. Completion log: `/tmp/mj-ignore-suspend-cli-completion.log`; documentation log: `/tmp/mj-ignore-suspend-doctests.log`. Initial CLI failure was `Text file busy` for its temporary login bridge, not a suspension failure.
