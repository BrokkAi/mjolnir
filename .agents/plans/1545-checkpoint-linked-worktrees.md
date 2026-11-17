# Checkpoints capture and restore linked Git worktrees inside a session's repository

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root. Tracking issue: GitHub #1545.


## Purpose / Big Picture

A Mjolnir session works in a Git checkout (usually a "managed clone" at `<project>/.mj/clones/<session-id>`). Agents often create extra working directories inside that checkout with `git worktree add`, for example `<clone>/.mj/lanes/<name>`, to run parallel experiments. Such a directory is a *linked worktree*: a second checkout of the same repository. Its `.git` is a one-line file pointing into the outer repository's `.git/worktrees/<name>/`, and its commits live in the outer repository's object store.

Today every operation that saves a "recovery copy" of a session (Suspend, Move, recovery checkpoints) fails on such a directory with `cannot checkpoint nested Git repository directory at <path>: for a disposable scratch worktree, exclude this path ...`. The session then shows `running (failed)` and the user must exclude the lanes by hand, which silently discards any uncommitted lane work and any commit that only a detached lane HEAD holds. On 2026-11-16 three bifrost sessions hit this; two lanes had real uncommitted edits that had to be rescued by hand.

After this change a recovery copy carries every linked worktree of the captured repository that lives inside the captured checkout: its HEAD (branch or detached commit), staged changes, unstaged changes, untracked files, lock state, and (where the main checkout's operation state is captured) its in-progress merge/rebase state. Resume and every other restore recreate each worktree at the same relative path with `git worktree add` and reapply that state. A directory that is a *different* repository (an independent clone, or a worktree of some other repository) is still refused with the existing error, and the existing "ignore nested repositories and suspend" choice still applies to those.

To see it working: in a managed-clone session, create `git worktree add .mj/lanes/demo -b demo`, commit in it, leave a staged edit, an unstaged edit and an untracked file, then `mj suspend` and `mj resume`; the lane exists again with the same branch, commit and edits.


## Progress

- [x] (2026-11-16) Map the capture, archive, restore and caller code (report: `.mj/agents/58e8ba96963e27e24db2b4762d6f62d2/MAP.md`, summarized in Context below).
- [x] (2026-11-16) Write this plan.
- [x] (2026-11-16) Milestone 1: capture linked worktrees into `RepositorySnapshot` and the archive (schema 11).
- [x] (2026-11-16) Milestone 2: restore linked worktrees.
- [x] (2026-11-16) Milestone 3: tests rewritten/added (`linked_worktrees_inside_a_clone_survive_a_recovery_copy`, `linked_worktree_commits_travel_when_the_checkout_has_none_of_its_own`, rewritten nested tests, schema+1 case); clippy and suites run; isolated-instance suspend/resume check passed (instance `wt-checkpoint`, driven by `reliability_lab.Lab`).


## Surprises & Discoveries

- Observation: `git ls-files --others --exclude-standard` run in the main checkout does not descend into a linked worktree; it emits the worktree directory once with a trailing slash (`.mj/lanes/A/`). A worktree nested inside that one (`.mj/lanes/A/.mj/lanes/C`) appears only when listing from inside A. `git worktree list --porcelain` is flat and identical from every worktree.
  Evidence: verified with git 2.53 in a scratch repository during mapping.
- Observation: nested lanes exist in the wild: `<clone>/.mj/lanes/11535-repair-integration/.mj/lanes/isolated11535-artifact-recipients` was a worktree of the same outer repository.
- Observation: a bundle with a HEAD tip that the exclusions remove (managed worktree whose own HEAD adds nothing) has no HEAD, so `fetch bundle HEAD` cannot restore it. Restore therefore unpacks the bundle (`bundle unbundle`) and checks out the recorded commit whenever the snapshot has linked worktrees, as it already does for clone refs; the capture omits HEAD from a bundle it creates only for worktree tips.
- Observation: restore has no non-fatal warning channel; a worktree restored detached instead of on its branch is reported with `tracing::warn!` (worktree path and branch).
- Observation: worktree metadata lives on `RepositoryManifest.linked_worktrees` (not `RepositoryMetadata`) because each entry also names its payload paths; `RepositoryMetadata` is unchanged.
- Observation: the capture previously leaked clone temporary refs if `collect_review_refs` or later steps failed; the capture now removes clone and worktree temporary refs together on every path, attempting all removals.
- Observation: `checkpoint_has_file_changes` (mj-controller publication check) now counts a linked worktree with a detached HEAD, dirty files or an operation as unpublished work.

- Review round 1 (2026-11-16): discovery decides inside/outside lexically before resolving, so only candidate paths can fail a capture; per-worktree restore is all-or-nothing (a failure after `worktree add` runs `worktree remove --force --force`); capture reports cleanup failure alongside a capture error; discovery and retry check reuse `git_directories`/`same_directory`; the manifest writes the worktree path as `/`-separated text through a serde module in mj-core. Added `a_linked_worktree_that_fails_to_restore_leaves_nothing_for_a_retry_to_skip`.


## Decision Log

- Decision: Recognise linked worktrees from Git state, not from a path convention. A directory under the captured checkout is "ours" when `git rev-parse --git-common-dir` run inside it resolves (canonicalized) to the same directory as the captured checkout's common dir. Everything else with a `.git` entry stays a foreign nested repository.
  Rationale: `.mj/lanes` is only a habit; any `git worktree add` inside the checkout must work. Comparing common dirs is what Git itself means by "same repository" and cannot be fooled by a hand-written `.git` file.
  Date/Author: 2026-11-16 / parent session.
- Decision: Scope is worktrees whose directory exists and lies inside the captured checkout's top level, at any depth. Registered worktrees outside the checkout, and registered worktrees whose directory is missing ("prunable"), are not captured and do not fail the capture. Capture never runs `git worktree prune` or otherwise mutates registrations.
  Rationale: outside paths cannot be expressed relative to the checkout and are not part of the session's workspace; a missing directory holds no work. Pruning would change the user's repository as a side effect of saving.
  Date/Author: 2026-11-16 / parent session.
- Decision: Every captured worktree's HEAD commit travels in the committed bundle in every history mode, by adding a temporary ref per worktree under the existing `refs/mj-checkpoint/<id>/` temporary namespace for the duration of bundle creation (the mechanism `collect_clone_refs` already uses for stash and operation objects), and removing it afterwards on every exit path. The worktree's HEAD commit id and branch name (if any) are recorded in the snapshot; restore uses the recorded ids, never a ref value read at the destination.
  Rationale: a detached lane HEAD is otherwise unreachable and lost. `git bundle create` needs named tips. Restore keeps existing `refs/mj/*` refs at the destination, so a persistent pin ref would go stale; recording the id avoids that.
  Date/Author: 2026-11-16 / parent session.
- Decision: Per worktree, capture exactly what the main checkout captures in the same history mode: staged patch, unstaged patch, untracked tar, and (only in `CloneFrom` mode, as for the main checkout) operation/conflict state, plus dirty-submodule refusal. Each checkout's untracked tar (main and worktree) excludes the directories of worktrees nested directly or indirectly inside it, because those are captured as their own entries. Lock state: record whether the worktree is locked and its reason, and relock on restore.
  Rationale: one consistent definition of "a checkout's state", reusing the existing functions with the worktree directory as the repository path.
  Date/Author: 2026-11-16 / parent session.
- Decision: The archive schema advances to 11 (`ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES`), written only when at least one repository carries a linked worktree, taking precedence over every earlier schema like the existing constants do. Payloads use new `PayloadRole` variants keyed by repository and worktree index.
  Rationale: `RepositoryMetadata` has no `deny_unknown_fields` and is flattened, so an older reader would silently drop the worktrees; the schema whitelist in older readers rejects unknown schemas explicitly, which is the visible failure we want. Archives without worktrees stay readable by older builds.
  Date/Author: 2026-11-16 / parent session.
- Decision: No change to `CheckpointExportSpec`, `CheckpointCaptureSpec` or the export/staging protocol versions. Worktrees are discovered from Git state on the target.
  Rationale: those specs deny unknown fields and run on the session's installed worker binary; avoiding a spec change means an old worker simply keeps its current behavior (the existing error) until it upgrades, and no protocol bump is needed.
  Date/Author: 2026-11-16 / parent session.
- Decision: Restore creates worktrees after the main checkout's own restore has completed (bundle, refs, checkout, stash, patches, untracked files, operation), in parent-before-child order (shorter relative path first). For each: `git worktree add` at the recorded relative path, on its branch if the branch is free at the destination (creating the branch at the recorded commit if absent; if present it must already point at the recorded commit, which holds after `CloneFrom` saved-ref restore), otherwise detached at the recorded commit with a warning naming the worktree and the branch it could not take. Then apply staged patch with `--index`, unstaged patch, untracked tar, operation state, and lock.
  Rationale: `git worktree add` refuses a branch checked out elsewhere, and the nested path must not be pre-created by the outer untracked restore. Detaching instead of failing keeps resume working while preserving every commit and edit; the warning makes the change visible.
  Date/Author: 2026-11-16 / parent session.
- Decision: Restore is idempotent per worktree. If the target directory already exists and is a worktree of this repository whose HEAD is the recorded commit, treat that worktree as already restored and skip it. If the directory exists in any other form, fail before changing that worktree with an error naming the path. If the path is registered but its directory is missing, `git worktree add -f` (and `-f -f` if locked) reuses the registration.
  Rationale: a retried restore must not fail on its own earlier partial progress, and must never overwrite unknown content.
  Date/Author: 2026-11-16 / parent session.
- Decision: Out of scope: `mj-worker/src/move_workspace.rs` (a separate raw-file mover for raw checkouts that bypasses the archive); sparse-checkout configuration of a worktree (not captured for the main checkout either; a restored worktree is a full checkout); per-worktree reflogs.
  Rationale: keep the change to the archive path that produced the failures; these are existing limitations shared with the main checkout.
  Date/Author: 2026-11-16 / parent session.


## Outcomes & Retrospective

Implemented as planned (schema 11, capture and restore of linked worktrees at any depth, foreign nested repositories still refused). Known limits: a worktree restored while its branch is held elsewhere or has moved comes back detached (logged with `tracing::warn!`); an old session worker keeps refusing worktrees until it upgrades. The `review_host::tests::push_review_delivers_each_verdict_without_holding_or_prompting_the_primary` controller test hangs on this host, also in an unmodified checkout, and is unrelated.


## Context and Orientation

Paths are relative to the repository root; "git.rs" means `mj-checkpoint/src/archive/git.rs`.

A *checkpoint* (recovery copy) is a single archive file holding a session's agent state and, per repository, a `RepositorySnapshot`: a Git bundle of committed history, a staged patch (`git diff --binary --cached`), an unstaged patch (`git diff --binary`), a tar of untracked files (`git ls-files --others --exclude-standard`), and in some modes saved refs, stash and in-progress operation state. The capture runs on the session's target machine inside the session's installed worker binary (`hel worker export-checkpoint | capture-checkpoint | pack-checkpoint`, driven by `mj-controller/src/checkpoint_transfer.rs`), not in the daemon.

The *history mode* is chosen per session by `mj-controller/src/controller/checkpoint/layout.rs:40-140`: a managed clone or remote workspace uses `CloneFrom` (all local branches, tags, notes, `refs/mj/`, stash and operation state are saved by `collect_clone_refs`, git.rs:1590, which also creates temporary refs `refs/mj-checkpoint/<id>/...` for bundle tips and removes them at :1728); a managed worktree uses `DeltaFrom` (bundle `HEAD ^merge-base`); a raw checkout is `MetadataOnly` (no content captured, unaffected by this plan). `select_git_history` is at git.rs:1745.

Capture flow: `mj-checkpoint/src/checkpoint/capture.rs:804` `collect_checkpoint_repositories_with_policy` → `reject_dirty_submodules` → `collect_git_snapshot_with_progress` (git.rs:122) → `collect_git_contents` (git.rs:1808), which runs bundle, staged diff, unstaged diff and `ls-files --others` in parallel and builds the untracked tar in `build_untracked_tar` (git.rs:2513). Each untracked entry goes through `append_worktree_entry` (git.rs:2595); a directory with a `.git` entry (`is_nested_repository`, git.rs:2585) bails with `NESTED_REPOSITORY_CHECKPOINT_ERROR` (constant in `mj-core/src/archive.rs:56`) unless the policy is `UntrackedFiles::IgnoreNestedRepositories`, which skips it (git.rs:2529). That policy comes from the user's explicit "ignore nested repositories and suspend" choice (`mj-controller/src/daemon/close.rs:91`, `CheckpointExportPolicy::IgnoreNestedRepositories` in `mj-controller/src/controller/checkpoint/lease.rs:167`) and must from now on affect only foreign repositories. Operation state is captured by `operation::collect` (`mj-checkpoint/src/archive/git/operation.rs:199`), which already resolves paths with `git rev-parse --git-path` and so works when run inside a linked worktree.

Archive format: schema constants are in `mj-core/src/archive.rs:12-42` (newest is `ARCHIVE_SCHEMA_VERSION_REVIEW_REFS = 10`). The writer picks the version in a precedence chain in `mj-checkpoint/src/archive.rs` around :1004; the reader whitelist is in `parse_archive_manifest` around :1812; `validate_manifest` around :1872 repeats the chain. `RepositoryMetadata` (`mj-core/src/archive.rs:63-160`) and `RepositoryManifest` (flattened, :204) do not deny unknown fields; `ArchiveManifest`, `PayloadRole` and `PayloadDescriptor` do (:418, :473). Per-repository payload files are named `repositories/{index:08}/...`. A new snapshot field must also be threaded through `collect_git_metadata_snapshot` (git.rs:224), the plain snapshot (git.rs:251), `StagedRepository` / `write_staged_repositories` and `checkpoint_repository_bytes` (capture.rs:210-295, the two-phase capture's staging directory), the archive writer and reader, and existing test literals.

Restore flow: `mj-checkpoint/src/checkpoint/restore.rs:517` `restore_repositories`, :527 `restore_repositories_from_archive`, :645 `restore_single_repository_onto_branch`, :681 `restore_single_repository_into_checkout`, all ending in `restore_git_snapshot_for_session` (git.rs:1988). Its order: metadata-only early return, `ensure_branch_available_for_checkout` (git.rs:2338, uses `git worktree list --porcelain`), bundle unbundle or fetch, checkout (`-B branch` or `--detach`), saved refs (existing `refs/mj/*` at the destination are kept, git.rs:2137-2175), stash, staged patch with `apply --index`, unstaged patch, `restore_untracked_tar` (never replaces a file), `operation::restore`. Callers include resume (`mj-controller/src/controller/resume.rs:2025-2065`), container upgrade, `mj-worker/src/checkpoint.rs` (:126, :233), and raw-to-workspace conversion (`mj-controller/src/controller/worktree.rs:1215-1245`, which uses `collect_git_snapshot` and therefore gains worktree capture automatically).

Git compatibility: the project supports Git 2.25/2.31 (`Git231` test shim in `mj-checkpoint/src/test_support.rs:38`). `git worktree list --porcelain` has no `-z` there and the `prunable` annotation is newer; paths are newline-delimited, which is safe because the code only needs worktree paths and checks directory existence itself. A worktree's registration lives in the common dir (`<common>/worktrees/<name>/`, with `locked` holding the lock reason); registration names are flat and may carry numeric suffixes, so worktrees are identified by relative path, not name.


## Plan of Work

Milestone 1 (capture). In `mj-core/src/archive.rs`, add `ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES: u32 = 11` with a doc comment in the style of its neighbours, and a serializable per-worktree record (suggested name `LinkedWorktreeMetadata`: relative path as protocol text with `/` separators, head commit id, optional branch name, optional lock reason, flags/lengths for its payloads mirroring how the main checkout's payloads are described) held as `linked_worktrees: Vec<...>` on the repository metadata with `#[serde(default, skip_serializing_if = "Vec::is_empty")]`. Add `PayloadRole` variants for a worktree's staged patch, unstaged patch, untracked tar and operation payloads, keyed by repository id and worktree index. Extend `RepositorySnapshot` with the worktree contents.

In git.rs, add discovery: from the captured checkout's top level, list `git worktree list --porcelain`, keep entries other than the main worktree whose path canonicalizes to a location strictly inside the top level and whose directory exists, and confirm each by comparing canonicalized `git rev-parse --git-common-dir` inside it with the captured checkout's. Sort by relative path. For each, collect HEAD (commit id, symbolic branch or detached), lock reason (read `git rev-parse --git-path locked` inside the worktree, or the porcelain `locked` line), staged and unstaged patches and untracked tar by calling the existing functions with the worktree directory as the repository, and operation state only in `CloneFrom`. Run `reject_dirty_submodules` for each worktree too. Change `build_untracked_tar` so that an untracked directory entry which is one of this repository's discovered worktrees (relative to the checkout being walked) is skipped because it is captured separately; a directory with `.git` that is not one of them keeps today's refusal/ignore behavior unchanged. Add the worktree HEADs as bundle tips via temporary refs under the existing `refs/mj-checkpoint/<id>/` namespace in every history mode that produces a bundle, with removal on every exit path (reuse or generalize the existing cleanup). In `mj-checkpoint/src/archive.rs`, write and read the new payloads and pick schema 11 first in the writer chain when any repository has worktrees; accept 11 in `parse_archive_manifest` and `validate_manifest`. Thread the field through the staging directory format of the two-phase capture and through `checkpoint_repository_bytes`.

Milestone 2 (restore). At the end of `restore_git_snapshot_for_session` (after `operation::restore` for the main checkout), restore each worktree in parent-before-child order following the idempotence and branch rules in the Decision Log: verify the recorded commit exists (it arrived in the bundle), decide branch versus detached, run `git worktree add` (with `-f` / `-f -f` only for a registered-but-missing path), apply the staged patch with `--index`, the unstaged patch, the untracked tar (`restore_untracked_tar` with the worktree as root), operation state when present, and `git worktree lock --reason` when recorded. Report the detach-instead-of-branch case through whatever warning channel restore already uses (search for how restore surfaces non-fatal notes; if none exists, emit a `tracing::warn!` with the worktree path and branch, and record that in Surprises). The ordinary restore paths listed in Context all pass through this function, so no caller change should be needed; verify `restore_single_repository_onto_branch` (host-worktree resume) behaves, because there the lanes register in the user's main repository.

Milestone 3 (tests and validation). Follow the repository's test rules in `AGENTS.md` ("Which tests to add"): these are integration tests against real Git. In `mj-checkpoint/src/checkpoint/tests.rs`, rewrite `ignore_nested_repositories_exports_outer_work_without_persisting_the_omission` (~:670) so the registered worktree no longer errors and the independent clone still does (and is still omitted under the ignore policy), and update `nested_worktree_checkpoint_names_the_directory_and_explicit_scratch_exclusion` (~:2608, Hard-won #1384) so its refusal case uses a foreign repository. Add one integration test, modelled on `a_subdirectory_session_restores_work_across_its_whole_checkout` (~:2692), that builds a managed-clone-style fixture with: a lane on its own branch with a new commit, a staged edit, an unstaged edit and an untracked file (include a non-ASCII file name); a detached lane whose HEAD is a commit no ref holds; a lane nested inside the first lane; a locked lane; and a registered lane whose directory was deleted. Export, restore into a fresh directory, and assert every lane's branch/HEAD, index, worktree contents and lock, the absence of the deleted lane, that the outer checkout's untracked files do not include lane contents, that no `refs/mj-checkpoint/` refs remain in the source, and that the archive manifest declares schema 11. Run the restore a second time onto the result to prove idempotence. Run the same test under the `Git231` shim if the file's other worktree tests do. Extend the existing "rejects schema version + 1" test (~:4012) if it is not already generic over the newest constant. Add a 64KB-plus untracked file in one lane only if the lane path uses a different streaming code path than the main checkout's untracked tar (it should not).


## Concrete Steps

Work from the repository root. Build and test on the dev profile, outside the restricted sandbox:

    cargo test -p mj-checkpoint
    cargo test -p mj-core archive
    cargo clippy --all-targets -- -D warnings

During development run only the affected tests by name, for example `cargo test -p mj-checkpoint linked_worktree`. Run the full `mj-checkpoint` and `mj-core` suites and workspace clippy once at the end; run `mj-controller` and `mj-worker` suites too if any file in those crates changed.

Live check in an isolated instance (never the default instance), after building with the normal dev build: start an isolated daemon (`mj --instance wt-checkpoint ...`), create a local managed-clone session on a scratch Git repository, use the session's shell to run `git worktree add .mj/lanes/demo -b demo`, commit there, leave staged/unstaged/untracked edits and a detached second lane, then `mj --instance wt-checkpoint suspend --acknowledge-unpublished-work --session <id>` and `mj --instance wt-checkpoint resume --session <id>`, and inspect the restored clone. Destroy the session and stop the instance's daemon afterwards.


## Validation and Acceptance

Acceptance: the new integration test fails on current master (capture errors with `cannot checkpoint nested Git repository directory`) and passes after the change; the rewritten tests pass; all suites and clippy pass. In the isolated-instance live check, suspend succeeds (state `suspended`, no error) and after resume `git -C <clone>/.mj/lanes/demo log -1` shows the lane commit, `git -C <clone>/.mj/lanes/demo status --porcelain` shows the same staged, unstaged and untracked entries as before suspend, and the detached lane is at its recorded commit. A foreign clone placed inside the checkout still makes suspend fail with the existing message.


## Idempotence and Recovery

Capture is read-only apart from temporary `refs/mj-checkpoint/<id>/` refs, which are removed on every exit path; a crash can leave them, and they are harmless and namespaced. Restore of worktrees is idempotent per the Decision Log, so a retried resume completes. No database migration is involved.


## Artifacts and Notes

The original failure, from the daemon log:

    lifecycle operation failed session_id=a8f351993ec7b7ce03c956fe6fd5c668 kind=Suspend ... repository 'bifrost-dev': cannot checkpoint nested Git repository directory at /workspace/a8f351993ec7b7ce03c956fe6fd5c668/bifrost-dev/.mj/lanes/10941-control/: for a disposable scratch worktree, exclude this path using the outer repository's .git/info/exclude; otherwise preserve its work separately


## Interfaces and Dependencies

No new crates. Use the existing `GitCommandRunner` abstraction and helpers in git.rs (`run_git`, `git_text`, `git_directories`) for every Git call; do not spawn processes directly. Keep path handling in `Path`/`PathBuf` and convert to `/`-separated text only when writing the manifest. At the end of Milestone 1, `mj_core::archive::ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES` exists and `RepositorySnapshot` carries the linked worktrees; at the end of Milestone 2, `restore_git_snapshot_for_session` recreates them.
