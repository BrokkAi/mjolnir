# Make retained session branches inspectable and show deletion only when applicable

This ExecPlan is a living document maintained under `.agents/PLANS.md`. The user requested planning and implementation one issue at a time. This plan covers #1073 only. #1063 is published and closed; #1083 remains later in the queue. Do not implement #1073 until the user approves this plan.

## Purpose / Big Picture


Destroying a session keeps its `mj/<session-id>` Git branch by default, because that branch may contain work the user wants. A person needs a way to find branches left behind after sessions are destroyed, see whether another branch contains their commits, and explicitly remove a chosen branch. In the terminal UI, a session without a managed worktree should not offer a branch-deletion choice that does nothing.

Issue #1073 also requested a branch-deletion choice in the web viewer. That choice already exists in the current tree and has browser coverage, so the plan verifies it rather than rebuilding it. Archiving already deletes a session branch when another non-session branch contains every commit. The remaining cleanup concerns branches kept after destruction, particularly unmerged ones.

## Progress


- [x] (2026-09-23) Finished #1063, pushed the validated result to origin/master, closed the issue, and removed its active-work label.
- [x] (2026-09-23) Read #1073 and its comments; it was open and unassigned. Self-assigned it and added `agent-in-progress` for planning.
- [x] (2026-09-23) Found the existing web destroy checkbox and browser tests, merged-branch archive cleanup, branch preservation rules, and the TUI confirmation paths.
- [ ] Receive user approval before implementing #1073.
- [ ] Prove a repository-scoped retained-branch inventory against an isolated instance and disposable Git repository, including merged and unmerged branches.
- [ ] Add explicit single-branch cleanup and make TUI destroy confirmations conditional on a managed worktree.
- [ ] Validate, commit on the current branch, push to origin/master, comment on #1073, close it, and remove `agent-in-progress`.
- [ ] Prepare #1083's plan only after #1073 is complete.

## Surprises & Discoveries


Commit `a6c1f3ed` already deletes a fully merged session branch during archiving. `mj-controller/src/controller/worktree.rs::managed_branch_is_merged` defines merged conservatively: the branch tip must be contained by a local or remote-tracking branch outside `refs/heads/mj/`. A squash merge or rebase does not count, so Git does not silently discard commits.

Commit `2e3077d9` already added a web destroy dialog checkbox labelled “Also delete the managed branch.” `mj-controller/src/web/viewer.js::confirmSessionDestruction` passes the choice as `delete_branch`; `tests/e2e/web/compact-cards.spec.js` covers the default keep choice and an explicit deletion choice. The API and CLI also pass this option. This resolves the issue's web-control item in the current tree.

`mj-tui/src/dialogs/render.rs::confirmation_buttons` always includes “Destroy and delete branch” for both `ForceDestroy` and `DestroyStopped`. The Sessions-pane action constructs `ForceDestroy` from a `SessionRecord`, which already has `managed_worktree: Option<ManagedWorktree>`. The resume dialog builds `ResumeRow` values in `mj-tui/src/resume.rs`; those rows currently omit the managed-worktree fact, including rows for stopped Mjolnir sessions. The confirmation handler in `mj-tui/src/dialogs.rs` maps button index 2 to `delete_branch: true`.

A destroyed session no longer has a record pointing to its source repository. Mjolnir cannot promise to discover every retained branch by walking only current session records or configured projects. Multiple named instances may also share one Git repository. Any inventory based on one instance can say only that a branch has no record in that instance; it cannot prove that another instance does not own it. The proposed command therefore requires the repository path, labels this distinction honestly, and never deletes branches in bulk.

## Decision Log


Decision (2026-09-23, proposed): implement a repository-scoped `mj branches` inspection command with an explicit exact-branch deletion option, rather than a global `mj doctor` count. A global count would miss source repositories no longer referenced after destruction and could misclassify branches owned by another instance. The caller supplies a repository path, so the inventory has a clear boundary and can show each branch's merged/unmerged state.

Decision (2026-09-23, proposed): list only local branches whose names match Mjolnir's `mj/<session-id>` shape. Show whether this instance has a session record for the ID and whether another non-session ref contains the tip. A branch absent from this instance is a candidate for manual cleanup, not automatically safe to delete. Delete only one exact branch named by the user, with an exact confirmation argument; refuse a branch whose session still exists in this instance or is checked out in any worktree. Explain that another instance's records are outside this check and require the user's inspection. No automatic or bulk deletion is planned.

Decision (2026-09-23, proposed): derive the TUI branch choice from `SessionRecord.managed_worktree.is_some()` and carry that fact only on Mjolnir resume rows. Keep a single confirmation flow whose buttons and accepted indices match the available choice. Native and SessionWiki archive rows are never Mjolnir destroy targets. Do not add a database field or change the default branch-preservation behavior.

Decision (2026-09-23, proposed): treat the web viewer control as complete unless validation finds a concrete regression. The issue asked for a way to choose deletion, which the current checkbox and browser test demonstrate. Aligning the web dialog's visibility with the TUI would be a separate polish decision if it materially improves the flow, not a prerequisite for this ticket's web item.

## Context and Orientation


`mj-core/src/state.rs::SessionRecord` stores an optional `ManagedWorktree`, including its source repository and `mj/<id>` branch. `mj-controller/src/controller/worktree.rs` owns Git worktree and branch cleanup; `BranchDisposition::Keep` is the destroy default and `DeleteIfMerged` is the archive policy. Git commands must use the repository's shared subprocess helpers in `mj-core/src/targets`, not raw pipe management.

`mj-cli/src/main.rs` defines one-shot CLI commands and `mj-cli/src/api_commands.rs` holds session command implementations. The proposed repository-scoped command will run as a CLI operation, not on a terminal render loop. Its inventory needs current-instance session IDs from the persisted controller state; it must not start, stop, or contact workers. It accepts a `PathBuf` repository argument and reads Git refs there. A deletion operation is a user-authored write to that repository and must be exact and opt-in.

`mj-tui/src/resume.rs` builds rows for live, stopped Mjolnir, native, and archived sessions. `mj-tui/src/dialogs.rs` owns confirmation state and result mapping; `mj-tui/src/dialogs/render.rs` owns button labels and explanatory text. The Sessions pane and resume dialog both reach destroy confirmations. Tests for state transitions live in `mj-tui/src/tests.rs` and `mj-tui/src/dialogs/tests.rs`; resume-row tests live in `mj-tui/src/resume/tests.rs`.

## Plan of Work


### Milestone 1: Prove the repository inventory


Create a disposable Git source repository and isolated `MJ_CONFIG_DIR`/`MJ_DATA_DIR`. Add local `mj/<session-id>` branches representing a branch fully contained by another non-session branch, a branch with unique commits, and a branch whose session ID still exists in the isolated store. Confirm which ref data Git provides for merged state and for a branch checked out in another worktree. Reuse the existing merged-branch interpretation in `mj-controller/src/controller/worktree.rs`; do not create a second parser with subtly different safety behavior. Record the observed output shape before committing a public CLI interface.

Acceptance: the prototype distinguishes these cases without modifying any branch or reading the default user's store. If the persisted state cannot be read safely by a one-shot command, adapt the design to use the existing controller API, still without blocking a UI loop.

### Milestone 2: Add exact cleanup and conditional TUI choices


Add `mj branches --repository <path>` for a read-only list, with JSON output if the neighboring CLI patterns support it. Each row identifies the exact local ref, whether this Mjolnir instance has a session record, and whether the branch tip is contained by a non-session ref. Add an explicit one-branch deletion form with an exact confirmation value; it refuses a current-instance session branch and lets Git refuse any branch checked out in a worktree. Never infer consent to delete unmerged work from a “merged” flag or from a missing current-instance record. Document the other-instance limitation in command help and CLI documentation. Test the advertised command against temporary repositories and isolated state, including wrong confirmation and no mutation on refusal.

Add a managed-worktree boolean to `ResumeRow` only where a Mjolnir session record supplies it. Carry the same fact into both TUI destroy confirmation variants. Render two actions for sessions without a managed worktree (Cancel, Destroy session) and three when a managed branch exists. Ensure keyboard accelerators and pointer selection use the same button list, and that button index 2 cannot dispatch deletion when absent. State-machine tests should drive both entry paths and assert the resulting `DashboardAction`, not merely reconstruct button arrays. Update human docs describing the destroy choice.

### Milestone 3: Validate and publish


Run focused CLI branch-inventory tests, TUI state-transition tests, and the existing web destroy browser tests to confirm the web item remains satisfied. Then run dev-profile `cargo test` outside the restricted sandbox and `cargo clippy --all-targets -- -D warnings`, formatting, `git diff --check`, and applicable docs checks. Use normal mbx build storage. No database migration is expected; if stored state changes become necessary, classify the new migration before implementation under the repository's compatibility rules.

Commit the validated issue on the current branch and push HEAD:master, as already authorized. Record the existing web fix and new evidence on #1073, close the issue, and remove `agent-in-progress`. Only then investigate #1083 and prepare its separate plan.

## Validation and Acceptance


A caller can inspect `mj/<id>` branches in an explicitly named repository and understand which ones still have current-instance records and which contain unique commits. No read-only invocation changes refs. An exact deletion request changes one named branch only after confirmation and the refusal checks. A terminal user never sees the delete-branch action for a session without a managed worktree, whether destroying from the Sessions pane or from the resume dialog. A managed-worktree session still offers the choice. The web viewer continues to send `delete_branch: false` by default and `true` after its explicit checkbox choice.

## Idempotence and Recovery


The inventory can be rerun without mutation. A failed or interrupted deletion leaves other branches untouched; after a successful deletion the same request reports the branch absent. All tests use disposable repositories and isolated stores. No branch is deleted during planning, proof of concept, or a test outside its fixture. The public command's exact confirmation applies only to the specific ref supplied in that request.

## Outcomes & Retrospective


Planning found two remaining gaps and one already completed web feature. The branch inventory design deliberately avoids implying that absence from one named instance proves global orphanhood. No #1073 application code has changed; implementation and its local proof await approval.

Revision (2026-09-23): initial review plan after #1063 completion and current-tree inspection.
