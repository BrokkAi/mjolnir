# Delete a fully merged session branch on archive, and preview reclaimable space in Setup

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It must be maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir (the `mj` binary) keeps a copy of every stopped coding session on disk: a checkpoint archive and any image attachments. An optional setting, `archive_after_days`, makes the daemon delete that copy for stopped sessions older than N days once SessionWiki (a full-text index of sessions) holds the conversation. Today that job always keeps the session's git branch, and the setting is a bare number with no indication of what it will free.

After this change:

1. When the archive job removes a session that ran in a managed worktree, it also deletes the session's `mj/<session id>` branch, but only when every commit on that branch is already reachable from some other branch in the repository. A branch with unmerged work is kept exactly as before.
2. In the Setup screen (the terminal configuration editor opened from the dashboard), the SessionWiki page shows how much disk space sessions use today, and as you type a value for "Archive after (days)" it shows how much of that space the job would reclaim under that value. The help text tells the user up front that an estimate will appear, and the estimate is computed in the background so the screen never stalls.

Success is visible from a terminal. For the branch: with `archive_after_days = 1`, stop a session whose branch was merged into the repository's main branch, age it two days, wait for the archive tick, and `git branch --list 'mj/*'` no longer shows it, while a sibling session whose branch holds an unmerged commit keeps its branch. For the preview: open Setup, go to SessionWiki, and the "Archive after (days)" row reads `Never · sessions use 4.8 GB`; type `30` and within a moment it reads `30 · would reclaim 1.2 GB of 4.8 GB (12 of 40 sessions)`.

## Progress

- [ ] Milestone 1: `BranchDisposition::DeleteIfMerged` and the merge check in `cleanup_managed_worktree`; archive job uses it; unit tests; docs and help text updated.
- [ ] Milestone 2: `archive_space_preview` in `mj-controller`, the `PreviewArchiveSpace` dashboard action and `ArchiveSpacePreviewed` update, live rendering in the SessionWiki setup page; unit tests; docs and help text updated.
- [ ] Final: `cargo test` and `cargo clippy --all-targets -- -D warnings` pass on the dev profile; each milestone committed on the current branch.

## Surprises & Discoveries

(none yet)

## Decision Log

- Decision: "Fully merged" means the branch tip is an ancestor of at least one local or remote-tracking branch that is not itself a Mjolnir session branch (not under `refs/heads/mj/`). Squash merges and rebases are therefore not detected and such branches are kept.
  Rationale: This is git's own meaning of merged (`git branch --merged`), it is cheap to compute with one command, and it errs on the side of keeping work. A user who squash-merges can still delete the branch by hand. Sub-agent children are torn down with `Keep` as today because they share their parent's worktree.
  Date/Author: 2026-09-17, Fable (design) at the user's request.

- Decision: When the source repository is gone, the branch is treated as kept (nothing to delete) and archiving proceeds; when the git query itself fails in an existing repository, the error propagates and that session is retried on the next hourly pass.
  Rationale: Mirrors how `remove_managed_worktree_checkout` already handles a vanished repository. A failing git command in a repository that exists is worth surfacing rather than silently keeping or deleting.
  Date/Author: 2026-09-17, Fable.

- Decision: The space estimate uses the same selection rule as the job (`sessions_ready_to_archive` with the current time) but does not apply the "is it indexed yet" gate.
  Rationale: The gate depends on how far the hourly index sync has got. Applying it would make the estimate flicker between 0 and the true value during the first index build and would mislead a user deciding on a policy. The help text already says the job waits for the index. The estimate is labelled "would reclaim", not "will reclaim now".
  Date/Author: 2026-09-17, Fable.

- Decision: The estimate replaces the row's value column, the way the build cache preview does, and the longer sentence goes in the dialog notice. The static help text is changed to promise the estimate.
  Rationale: The setup screen already has exactly this mechanism (`automatic` label passed to `value_summary`, `notice` for prose). Reusing it keeps one pattern for live text and needs no new layout.
  Date/Author: 2026-09-17, Fable.

## Outcomes & Retrospective

(to be written at completion)

## Context and Orientation

Mjolnir is a Rust workspace. The crates that matter here:

- `mj-core`: shared types. `mj-core/src/state.rs` holds `ManagedWorktree` (lines 719 to 732): `source_repository: PathBuf`, `worktree_root`, `branch: String` (always `mj/<session id>`), `target: ManagedWorktreeTarget` (`Local` or `Ssh { .. }`), `base_commit: Option<String>`. `mj-core/src/config.rs` lines 139 to 169 hold `SessionWikiConfig { archive_after_days: Option<u32>, .. }`. `mj-core/src/config/loading.rs:233` has `sessions_dir()`, the directory under the data directory that holds checkpoint archives. `mj-core/src/attachment.rs` has `AttachmentStore::controller(session_id)` whose root is `sessions_dir()/<session id>/attachments`.
- `mj-controller`: the daemon and controller logic. `mj-controller/src/controller/lifecycle.rs` lines 22 to 31 define `pub enum BranchDisposition { Delete, Keep }`, re-exported from `mj-controller/src/controller.rs:80`. `destroy_session_controlled_with` (lifecycle.rs 576 to 613) and `force_destroy_session_with` (641 to 687) do the teardown: worktree cleanup, delete the checkpoint file at `session.checkpoint.archive_path`, remove attachments, delete the database row. `mj-controller/src/controller/worktree.rs` lines 1715 to 1751 hold `cleanup_managed_worktree(executor, worktree, branch: BranchDisposition)`: it removes the checkout, returns early for `Keep`, otherwise probes the branch with `show-ref --verify --quiet refs/heads/<branch>` and deletes with `branch -D`. Git commands are built with `managed_git_command(target, directory, args, purpose)` (line 426) and run with `managed_git_stdout` (line 450) or `execute_checked` (`mj-controller/src/controller.rs:1266`). These helpers already handle SSH targets; do not call `std::process::Command` directly.
- The archive job: `mj-controller/src/daemon/resume.rs:179` `archive_aged_sessions(older_than_days)` syncs the index, selects candidates with `mj_controller::sessionwiki::sessions_ready_to_archive` (`mj-controller/src/sessionwiki.rs` 1047 to 1085, pure over `state.sessions` and `state.subagents`, selecting `Stopped` sessions whose `updated_at` is at or before now minus N days, children before parents), checks each is indexed, then calls `archive_stopped_session` (resume.rs 240 to 250) which calls `tear_down_stopped_session(session_id, LifecycleKind::ArchiveStopped, BranchDisposition::Keep)`. The hourly trigger is in `mj-controller/src/server_runtime/run.rs` 665 to 688; `MJ_PRUNE_TICK_SECONDS` shortens it for live checks.
- `mj-tui`: the terminal UI, pure state machines that return actions. `mj-tui/src/setup.rs` is the Setup screen. `mj-tui/src/setup/schema.rs` holds defaults (line 19: `"sessionwiki" => json!({"archive_after_days": null})`), labels (117 to 118), `null_label` (172 to 180, returns `"Never"` for `archive_after_days`), and `help(path) -> &'static str` (269 to 346). The live-preview precedent is the build cache: `SetupDialog.build_cache_preview` (setup.rs 105 to 128), `preview_build_cache_action` (692 to 722, called at the end of `handle_setup_event` at 1319 to 1321), `build_cache_automatic_label` (726 to 757) fed as the `automatic` argument of `value_summary` (213 to 250, call site 1679 to 1685), and `build_cache_previewed` (1327 to 1370) which drops stale answers by generation and key and sets `notice`.
- `mj-cli`: the host that runs the TUI. `mj-cli/src/dashboard/actions.rs` 217 to 241 handles `DashboardAction::PreviewBuildCache` with `spawn_cancellable_io_with_token` (`mj-cli/src/dashboard/io/spawn.rs` 352 to 373), and `mj-cli/src/dashboard/io.rs` 172 to 176 and 873 to 879 carry `DashboardIoUpdate::BuildCachePreviewed` back into the dialog. `DashboardAction` lives in `mj-tui/src/lib.rs` (the `PreviewBuildCache` variant is at 166 to 173).
- Documentation for humans: `docs/src/content/docs/sessions.md`, section "What archiving deletes and keeps" (around line 291) and the `archive_after_days` example at line 234.

Terms. A "managed worktree" is a git worktree Mjolnir creates under `<repository>/.mj/worktrees/<session id>` on branch `mj/<session id>` so each session works in isolation. "Stopped" is the session state after the user closes it; the checkout is already removed at that point, and only the branch, the checkpoint, and attachments remain. The "Setup screen" is the configuration editor inside the `mj` dashboard.

## Plan of Work

### Milestone 1: delete a fully merged branch on archive

In `mj-controller/src/controller/lifecycle.rs`, add a third variant to `BranchDisposition`:

    /// Delete the branch only when every commit on it is reachable from some
    /// other branch, local or remote-tracking, that is not a session branch.
    DeleteIfMerged,

In `mj-controller/src/controller/worktree.rs`, next to `cleanup_managed_worktree`, add:

    /// Whether the session branch is contained in a branch that is not a
    /// Mjolnir session branch. `Ok(None)` means the repository is gone.
    fn managed_branch_is_merged(
        executor: &impl CommandExecutor,
        worktree: &ManagedWorktree,
    ) -> Result<Option<bool>>

It runs, through `managed_git_stdout`, in `worktree.source_repository`:

    git for-each-ref --contains refs/heads/mj/<id> --format=%(refname) refs/heads refs/remotes

and answers `true` when any printed line is not under `refs/heads/mj/` and is not `refs/remotes/*/HEAD`. (A remote-tracking ref counts because a branch merged upstream and fetched is merged.) If the branch does not exist (the existing `show-ref` probe says absent), answer `true` so the caller's delete is a no-op path. Use `git rev-parse --is-inside-work-tree` or the existing repository-gone detection to return `Ok(None)` when the repository is missing, mirroring `remove_managed_worktree_checkout`.

Change `cleanup_managed_worktree` so that after the checkout removal:

- `Keep` behaves as today.
- `Delete` behaves as today.
- `DeleteIfMerged` calls `managed_branch_is_merged`; `Some(true)` falls through to the existing delete path; `Some(false)` or `None` behaves as `Keep`. Log one `tracing::info!` line naming the branch and whether it was deleted or kept, with the reason.

In `mj-controller/src/daemon/resume.rs`, `archive_stopped_session` passes `BranchDisposition::DeleteIfMerged` instead of `Keep`. Children in `tear_down_stopped_session` stay on `Keep`. Update the doc comment on `LifecycleKind::ArchiveStopped` in `mj-controller/src/daemon.rs:198`.

Tests. In the `#[cfg(test)]` block of `worktree.rs`, using the existing fake `CommandExecutor` pattern found there, add tests that `cleanup_managed_worktree` with `DeleteIfMerged` issues `branch -D` when the fake answers a `for-each-ref` line `refs/heads/master`, does not issue it when the only line is `refs/heads/mj/<other id>`, and does not issue it when the output is empty. Name them in the repository's style, for example `archive_deletes_a_branch_another_branch_contains` and `archive_keeps_a_branch_only_session_branches_contain`. If the fake executor cannot script per-command output, extend it minimally rather than adding a mocking framework.

Docs. In `docs/src/content/docs/sessions.md` "What archiving deletes and keeps", replace the sentence about keeping the branch with: the branch is deleted when all of its commits are already on another branch (local or remote-tracking) and kept otherwise; squash-merged branches are kept. In `mj-tui/src/setup/schema.rs` `help`, change the `archive_after_days` text accordingly (Milestone 2 rewrites this text again; do both in one final wording if working sequentially).

### Milestone 2: live space preview in Setup

In `mj-controller/src/sessionwiki.rs`, in the "The archive job" section, add:

    /// How much disk Mjolnir's session copies use, and how much an
    /// `archive_after_days` value would free.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct ArchiveSpacePreview {
        pub sessions: usize,
        pub bytes: u64,
        pub reclaimable_sessions: usize,
        pub reclaimable_bytes: u64,
    }

    pub fn archive_space_preview(older_than_days: Option<u32>) -> Result<ArchiveSpacePreview>

It loads the controller (`Controller::load()`), sums for every session record the size of `session.checkpoint.archive_path` (a missing file counts as zero) plus the recursive size of `sessions_dir()/<id>/attachments` (a missing directory counts as zero), and when `older_than_days` is `Some(n)` sums the same over `sessions_ready_to_archive(&state.sessions, &state.subagents, Utc::now(), n)`. Put the per-session size in a small private helper `session_bytes(record) -> u64` so both sums use one definition. This function does filesystem I/O and must only be called from a blocking task. Add a unit test for the pure part by factoring the sizing over a `BTreeMap<String, SessionRecord>` plus a closure or path base, so a test with a temporary directory holding two fake `.hel.zip` files and one attachments directory asserts both totals; keep the test independent of the live data directory by using an isolated `MJ_DATA_DIR` if `Controller::load` is involved, or by testing the factored function directly.

Also add a byte formatter if none exists (search `mj-tui` and `mj-core` for an existing `format_bytes` or similar first and reuse it): `1.2 GB`, `640 MB`, `12 KB`, `0 B`.

In `mj-tui/src/lib.rs`, add:

    DashboardAction::PreviewArchiveSpace { generation: u64, older_than_days: Option<u32> }

In `mj-tui/src/setup.rs`, mirroring the build cache preview:

- `SetupDialog.archive_space_preview: Option<ArchiveSpacePreviewState>` with `key: Option<u32>` (the days value the answer is for) and `result: Resolving | Ready(ArchiveSpacePreview) | Failed(String)`.
- `fn preview_archive_space_action(&mut self) -> DashboardAction`: when the current page is the `sessionwiki` section, or the open text editor is for `archive_after_days`, compute the effective value: the editor's draft text if one is open (parsed as `u32`; an empty or unparsable draft means `None`), otherwise the stored value. If no preview exists for that key, set `Resolving` and return the action with a fresh generation. Call it from the end of `handle_setup_event` in the same place `preview_build_cache_action` is called, so it runs on every keystroke in the editor and on page entry. Because a filesystem walk takes a moment, stale answers are dropped by generation and key exactly as `build_cache_previewed` does.
- `fn archive_space_automatic_label(&self, field: &str) -> Option<String>` returning, for `archive_after_days` only: `Resolving…` while resolving; for `Ready` with key `None`: `Never · sessions use <bytes>`; for `Ready` with key `Some(n)`: `<n> · would reclaim <reclaimable> of <bytes> (<k> of <m> sessions)`; for `Failed`: `Unknown`. Pass it as the `automatic` argument at the `value_summary` call site (setup.rs 1679 to 1685) for that row, so it replaces both the `Never` label and the plain number. Note `value_summary` today only uses `automatic` for `Value::Null`; extend it so that for `archive_after_days` a `Some` automatic label also replaces a numeric value (keep the existing `Never` fallback when no preview is available so the test `empty_archive_after_days_renders_as_never` still passes).
- `pub fn archive_space_previewed(&mut self, generation: u64, older_than_days: Option<u32>, result: Result<ArchiveSpacePreview, String>)`: drop stale answers, store, set `notice` to one sentence such as `Sessions use 4.8 GB. Archiving after 30 days would reclaim 1.2 GB across 12 sessions.` or, for a failure, the error text.

In `mj-cli/src/dashboard/io.rs` add `DashboardIoUpdate::ArchiveSpacePreviewed { generation, older_than_days, result }` and dispatch it to `archive_space_previewed`. In `mj-cli/src/dashboard/actions.rs` handle `PreviewArchiveSpace` with `spawn_cancellable_io_with_token(...)` running `mj_controller::sessionwiki::archive_space_preview(older_than_days)` in the blocking pool, label `"estimating archive space"`.

Help text. In `schema.rs` `help`, set `archive_after_days` to: `Stopped sessions older than this many days are removed from Mjolnir once SessionWiki has indexed them. The checkpoint and any image attachments are deleted. The session's branch is deleted only if all its commits are already on another branch. Leave empty to keep every session. This row shows how much space sessions use now and, as you type a number, how much that value would reclaim.` Keep it within the two rendered help rows at a typical width; if it wraps past two rows, shorten it rather than let it be cut off (check `render_setup` at setup.rs 1575 to 1585 for the fixed height).

Tests in `mj-tui/src/setup/tests.rs`, following `the_build_cache_page_shows_the_values_its_host_resolves_for_blank_fields`: entering the SessionWiki page returns a `PreviewArchiveSpace { older_than_days: None }` action; typing `3` then `0` in the editor returns actions for `Some(3)` then `Some(30)`; a `Ready` answer for `Some(30)` renders the row with `would reclaim`; an answer for a stale key is ignored.

Docs. In `docs/src/content/docs/sessions.md`, after the `archive_after_days` example, add two sentences: the Setup screen's SessionWiki page shows how much disk sessions use and, while you type a value, an estimate of what that value would reclaim; the estimate covers checkpoints and attachments and counts every aged stopped session whether or not the index has caught up with it yet.

## Concrete Steps

All commands run from `/home/jonathan/Projects/hel4`.

Build and test on the dev profile, outside the sandbox:

    cargo test
    cargo clippy --all-targets -- -D warnings

Commit each milestone on the current branch with only the files touched, using the attribution lines the session provides.

Live check for Milestone 1 (optional, isolated): with `MJ_CONFIG_DIR` and `MJ_DATA_DIR` set to a scratch directory and `archive_after_days = 1`, start two sessions in a scratch git repository, commit on both branches, merge one into `master`, stop both, set both records' `updated_at` two days back in the isolated `mj.sqlite3` (`UPDATE sessions SET updated_at = ... WHERE session_id = ...`; the exact column format is what `parse_time` in `mj-controller/src/sessionwiki.rs` reads), run the daemon with `MJ_PRUNE_TICK_SECONDS=5`, and observe `git branch --list 'mj/*'` showing only the unmerged branch.

Live check for Milestone 2: run `mj`, open Setup, go to SessionWiki, watch the row read `Never · sessions use …`, type a number, watch it change to `would reclaim …`, and confirm the screen stays responsive while it resolves.

## Validation and Acceptance

Milestone 1 is accepted when the new worktree tests pass, the full suite passes, and the live check shows a merged branch deleted and an unmerged branch kept after archiving. Milestone 2 is accepted when the new setup tests pass, the full suite passes, and the Setup row shows the space used today on entry and a recomputed reclaim estimate after each keystroke without blocking input.

## Idempotence and Recovery

Every step is additive. `DeleteIfMerged` only ever deletes a branch whose commits are on another branch, so nothing is lost if the job runs twice. The preview only reads the filesystem. If the estimate misbehaves, leaving `archive_after_days` empty keeps every session and the estimate row still reports the space used.

## Artifacts and Notes

(to be filled in as work proceeds)

## Interfaces and Dependencies

In `mj-controller/src/controller/lifecycle.rs`:

    pub enum BranchDisposition { Delete, Keep, DeleteIfMerged }

In `mj-controller/src/controller/worktree.rs`:

    fn managed_branch_is_merged(executor: &impl CommandExecutor, worktree: &ManagedWorktree) -> Result<Option<bool>>;

In `mj-controller/src/sessionwiki.rs`:

    pub struct ArchiveSpacePreview { pub sessions: usize, pub bytes: u64, pub reclaimable_sessions: usize, pub reclaimable_bytes: u64 }
    pub fn archive_space_preview(older_than_days: Option<u32>) -> Result<ArchiveSpacePreview>;

In `mj-tui/src/lib.rs`:

    DashboardAction::PreviewArchiveSpace { generation: u64, older_than_days: Option<u32> }

In `mj-tui/src/setup.rs`:

    pub fn archive_space_previewed(&mut self, generation: u64, older_than_days: Option<u32>, result: Result<ArchiveSpacePreview, String>);

In `mj-cli/src/dashboard/io.rs`:

    DashboardIoUpdate::ArchiveSpacePreviewed { generation: u64, older_than_days: Option<u32>, result: Result<ArchiveSpacePreview, String> }

No new crates or dependencies. Git is invoked only through the existing managed-target command helpers.
