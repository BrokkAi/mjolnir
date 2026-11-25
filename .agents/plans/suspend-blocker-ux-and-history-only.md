# Readable & Actionable Suspend Blockers and History-Only Suspend

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

When a user tries to suspend a session whose workspace has Git snapshot blockers (such as `skip-worktree`/`assume-unchanged` flags or linked worktrees outside the clone), Mjolnir currently dumps raw Rust `Debug` enum structures (`HiddenTrackedEdits([Scoped { ... }])`, `UnrepresentedGitState { ... }`) in front of a long semicolon-separated paragraph, emits a broken `git --git-dir '.../.git' --work-tree '.../.git' worktree remove` command, gives no command for clearing hidden tracked flags, and leaves the user stuck in a modal whose only button is **Cancel**.

After this change, a user suspending a blocked session sees clean, structured blocker sections across the TUI, Web UI, and CLI (`mj suspend --dry-run` / `mj move --dry-run`) with accurate copyable remediation commands, and has three direct actions from the suspend dialog (plus matching CLI/API support):

1. **Ask session to fix**: sends a structured prompt listing the blockers directly to that session so the agent can inspect and clean up its own workspace.
2. **Fix safe blockers & retry** (and `mj suspend --fix-blockers`): performs strictly lossless target-side remediation—clearing `skip-worktree` and `assume-unchanged` index bits (across the main checkout, linked worktrees, and initialized submodules using two separate `git update-index` calls per checkout) and moving unlocked `can_move_inside` external worktrees inside `<clone>/.mj/worktrees/<name>` via `git worktree move`—then refreshes preflight in place.
3. **Suspend with session history only** (and `mj suspend --history-only`): saves a real verified checkpoint archive containing the session's canonical conversation snapshot and native harness artifacts while skipping workspace Git delta capture, tears down the environment cleanly, and lets a later `mj resume` restore the conversation history into a fresh workspace checkout (or existing raw checkout).

## Progress

- [x] (2026-11-25 17:08Z) Completed read-only reconnaissance across `mj-checkpoint`, `mj-core`, `mj-store`, `mj-worker`, `mj-controller`, `mj-server`, `mj-client`, `mj-tui`, and `mj-cli`, and authored this ExecPlan.
- [x] Milestone 1: Readable suspend/move blockers and fixed remediation commands across `mj-checkpoint`, `mj-core`, `mj-worker`, `mj-controller`, `mj-tui`, `mj-server` (Web UI), and `mj-cli`, with updated goldens and unit tests.
- [ ] Milestone 2: Safe one-click blocker remediation (`resolve_safe_suspend_blockers`, `mj-worker resolve-suspend-blockers`, controller orchestration, `POST /api/v1/sessions/{id}/suspend-resolve-blockers`, CLI `--fix-blockers`, TUI & Web UI button) and tests.
- [ ] Milestone 3: "Ask session to fix" action in TUI and Web UI suspend blocker dialogs and tests.
- [ ] Milestone 4: "Suspend with session history only" (`SuspendConsent.history_only`, store migration 159, `CheckpointExportPolicy::HistoryOnly`, `MetadataOnly` capture + `FreshHead`/origin resume, HTTP API, CLI `--history-only`, TUI & Web UI confirmation flow) and integration tests.

## Surprises & Discoveries

- Observation: `SuspendBlocker.kind` is populated via `format!("{:?}", b.kind)` in two places, not one: `mj-controller/src/controller/lifecycle.rs:551` (local bare) and `mj-worker/src/checkpoint.rs:186` (remote/container targets).
  Evidence: Both files map `inventory.blockers` into `mj_core::suspend_consent::SuspendBlocker { kind: format!("{:?}", b.kind), ... }`. Both must be switched to `b.kind.kind_code().to_owned()`.

- Observation: Combining `--no-skip-worktree` and `--no-assume-unchanged` in a single `git update-index` invocation is unsafe on Git 2.53.0.
  Evidence: Direct test on Git 2.53.0 showed the last flag wins and `git update-index --no-skip-worktree --no-assume-unchanged -- <path>` left `S` (skip-worktree) intact and turned an assume-unchanged `s` entry into skip-worktree `S`. Running two separate calls (`--no-skip-worktree` then `--no-assume-unchanged`) clears both bits cleanly and exits 0 even when a bit is already clear.

- Observation: `HiddenTrackedEdits` paths collected from initialized submodules are attached to the parent checkout inventory with submodule-prefixed paths (`mj-checkpoint/src/snapshot.rs:3472-3473`, `mj-checkpoint/src/checkpoint/submodules.rs:365-370`), so running `git update-index` at the parent clone root fails with exit 128 (`Unable to mark file`).
  Evidence: Remediation must walk the main checkout, discovered in-clone linked worktrees, and initialized submodules directly and run `git update-index` in each checkout with paths relative to that checkout's index.

- Observation: Moving an external linked worktree into `<clone>/.mj/worktrees/<name>` via `git worktree move` makes `discover_linked_worktrees` include it automatically, and `mj-checkpoint/src/snapshot.rs:3475` already excludes discovered linked worktrees from the parent's untracked file and nested repository lists.
  Evidence: `mj-checkpoint/src/archive/git.rs:1939-1991` checks `starts_with` against the checkout root, and `snapshot.rs:3474-3481` skips any untracked path prefixed by `nested` (the discovered linked worktrees).

- Observation: `mj-checkpoint` already has `CheckpointRepositoryCapture::MetadataOnly` (`review_refs_metadata_only: true`), which records repository identity and `refs/mj/` while leaving all Git bundles, patches, worktrees, and untracked tars empty, and `restore_git_snapshot_for_session_with_options` (`mj-checkpoint/src/archive/git.rs:2325-2336`) already forces `restore_contents = false` when `review_refs_metadata_only` is set.
  Evidence: `mj-controller/src/controller/checkpoint/latched.rs:81-85` already uses `MetadataOnly` for `SessionHandoff`. Reusing `MetadataOnly` for `HistoryOnly` (while keeping `PlainDirectory` unchanged for non-Git directories) avoids any breaking wire change to `CheckpointRepositoryCapture` (`#[serde(deny_unknown_fields)]`).

- Observation: On resume of a retired `ManagedWorktree` of kind `Clone`, `restore_managed_worktree` calls `create_managed_clone` with `CloneSeedMode::FreshBranch`, which runs `git switch --no-track -C <branch> <base_commit>` (`mj-controller/src/controller/worktree.rs:2502-2521`).
  Evidence: For a history-only checkpoint, `base_commit` may be stale or absent from `source_repository`, and the user expects a fresh checkout from current HEAD. Adding `CloneSeedMode::FreshHead` (switching to `HEAD` when resuming a `review_refs_metadata_only` managed clone) avoids depending on the old launch commit.

- Observation: `SuspendConsent` has `#[serde(deny_unknown_fields)]` and is persisted as JSON in `sessions.closing_consent` (introduced in breaking schema revision 147; current `SCHEMA_VERSION` is 158).
  Evidence: `mj-store/src/database/sessions.rs:1331-1345` decodes `closing_consent` into `SuspendConsent` and returns an error on unknown fields. Adding `history_only: bool` with `#[serde(default, skip_serializing_if = "std::ops::Not::not")]` requires a breaking schema revision 159 (`minimum_compatible_version = 159`) so older daemons cannot crash during recovery on rows where `history_only` is true.

## Decision Log

- Decision: Keep `SuspendBlocker`'s existing JSON shape (`kind: String`, `paths: Vec<String>`, `remedy: String`) with `#[serde(deny_unknown_fields)]` intact, and replace `kind`'s value with the short `snake_case` variant name (e.g., `"hidden_tracked_edits"`, `"unrepresented_git_state"`).
  Rationale: Preserves wire compatibility with existing clients, workers, and HTTP API consumers while completely eliminating the Rust `Debug` dump. `SuspendBlocker::title()` maps known codes to human labels and falls back cleanly for any legacy string.
  Date/Author: 2026-11-25 / Coordinator

- Decision: Safe blocker auto-remediation performs only strictly lossless operations: clearing `skip-worktree`/`assume-unchanged` index bits (via two separate `git update-index` calls per repo/worktree/submodule) and moving unlocked `can_move_inside` external worktrees inside `<clone>/.mj/worktrees/<name>` via `git worktree move`. It never auto-deletes worktrees or adds per-kind content-inspection gates.
  Rationale: Aligns with the project rule against content-inspection deletion heuristics for unrepresented worktrees while resolving the two most common blockers safely without risking data loss. Locked or non-moveable worktrees remain reported so the user or the session ("Ask session to fix") can handle them deliberately.
  Date/Author: 2026-11-25 / Coordinator

- Decision: Implement "Suspend with session history only" by adding `SuspendConsent.history_only: bool`, breaking store migration 159, and `CheckpointExportPolicy::HistoryOnly`, reusing `CheckpointRepositoryCapture::MetadataOnly` (and preserving `PlainDirectory` for non-Git directories) rather than introducing an empty `repositories: []` archive or a new `CheckpointRepositoryCapture` variant.
  Rationale: Keeps repository provenance (`origin`, `remote_workspace`, `refs/mj/`) needed for container/network and bare resume, avoids breaking worker capture protocols, writes a real verified `hel-session` archive containing `CanonicalSessionSnapshot` + native harness artifacts, and cleanly restores into a fresh workspace checkout on resume.
  Date/Author: 2026-11-25 / Coordinator

- Decision: Require an explicit second confirmation step in TUI and Web UI before executing "Suspend with session history only".
  Rationale: History-only suspend discards unpushed commits and uncommitted workspace edits on managed checkouts while keeping conversation history. An explicit confirmation step prevents accidental workspace loss from a stray keypress in a busy blocker dialog.
  Date/Author: 2026-11-25 / Coordinator

- Decision: Do not modify `go/`.
  Rationale: Repository instructions specify working on `go/` only when explicitly instructed.
  Date/Author: 2026-11-25 / Coordinator

## Outcomes & Retrospective

To be completed after Milestone 4 validation and review.

## Context and Orientation

Mjolnir is a Rust workspace (`mj-core`, `mj-store`, `mj-checkpoint`, `mj-worker`, `mj-worker-bin`, `mj-session-runtime`, `mj-controller`, `mj-server`, `mj-client`, `mj-tui`, `mj-cli`). The daemon (`mj-controller` + `mj-server`) owns session records and lifecycle operations; target workers (`mj-worker` / `mj-worker-bin`) execute target-side commands locally, inside containers, or over SSH via `mj-core/src/targets.rs` (`command_on_locator`).

The relevant code flows are:

1. **Snapshot inventory and blockers**: `mj-checkpoint/src/snapshot.rs` defines `SnapshotRefused` (10 variants) and `InventoryBlocker { kind: SnapshotRefused, paths, remedy }`, plus `impl Display for SnapshotRefused`. Both `mj-controller/src/controller/lifecycle.rs` (local bare) and `mj-worker/src/checkpoint.rs` (remote/container via `mj-worker suspend-preflight --spec -`) convert `InventoryBlocker` into `mj_core::suspend_consent::SuspendBlocker { kind, paths, remedy }`.
2. **Blocker rendering**: TUI renders `Confirmation::SuspendSession` in `mj-tui/src/dialogs/render.rs` and handles input in `mj-tui/src/dialogs.rs`, `mj-tui/src/dashboard_sessions.rs`, and `mj-cli/src/dashboard/actions.rs`. Web UI renders the suspend modal in `mj-server/src/web/viewer.js` (`confirmSessionSuspension`). CLI renders dry-run preflight in `mj-cli/src/api_commands.rs` (`mj suspend --dry-run`) and `mj-cli/src/main.rs` (`mj move --dry-run`).
3. **Sending prompts to a session**: The daemon accepts `ControllerAction::Prompt { session_id, text, ... }` over `/api/v1/actions` (`mj-server/src/server/actions.rs`, `mj-server/src/server/handlers.rs`). Web UI has `sendSubmission({ action: 'prompt', session_id, text })` in `mj-server/src/web/viewer.js`. TUI dispatches `DashboardAction` values handled off the UI loop in `mj-cli/src/dashboard/actions.rs`.
4. **Checkpoints, suspend, and resume**: `SuspendConsent` (`mj-core/src/suspend_consent.rs`) is persisted in `sessions.closing_consent` (`mj-store/src/database/sessions.rs`, schema in `mj-store/src/database/schema.rs`, current `SCHEMA_VERSION = 158`). Suspend chooses a `CheckpointExportPolicy` (`mj-controller/src/controller/checkpoint/lease.rs`) in `mj-controller/src/controller/lifecycle.rs`, runs latched capture/pack in `mj-controller/src/controller/checkpoint/latched.rs` (calling `mj-checkpoint/src/checkpoint/capture.rs`), installs `SessionRecord.checkpoint`, and retires the checkout (`mj-controller/src/controller/worktree.rs`). Resume (`mj-controller/src/controller/resume.rs`) recreates the workspace (managed clone via `worktree.rs`, network workspace via `network_git.rs` + provisioning, or raw checkout in place) and calls `restore-checkpoint` (`mj-checkpoint/src/checkpoint/restore.rs` and `mj-checkpoint/src/archive/git.rs`).

## Plan of Work

### Milestone 1: Readable suspend/move blockers and fixed remediation commands

Goal: Eliminate raw Rust `Debug` output from blocker displays, fix the broken `--work-tree` worktree command and missing `HiddenTrackedEdits` command, group multi-item blockers cleanly, and render blockers as scannable structured sections in TUI, Web UI, and CLI.

Edits:

1. In `mj-checkpoint/src/snapshot.rs`:
   - Add `pub fn kind_code(&self) -> &'static str` on `SnapshotRefused` returning the `snake_case` tag matching its serde tag (`"untracked"`, `"operation_in_progress"`, `"dirty_submodule"`, `"missing_managed_origin"`, `"origin_removed_by_teardown"`, `"unrepresented_git_state"`, `"hidden_tracked_edits"`, `"intent_to_add"`, `"untracked_submodule_registration"`, `"tracked_keep_files"`).
   - Add a helper to format `CheckoutKey` + path without double colons: when `checkout_path` is empty, emit `{repository_id}:{path}` (or just `{path}` when `repository_id == "project"` and used in concise single-repo lists—keep explicit `{repository_id}:{path}` where existing tests check repository IDs, and omit the empty middle `::`). Specifically, write `format_scoped_path(checkout: &CheckoutKey, path: &Path) -> String` producing `"{}:{}"` when `checkout.checkout_path` is empty (or just `"{}"` if `repository_id` is empty) and `"{}:{}:{}"` when `checkout_path` is non-empty, and `format_checkout_key(checkout: &CheckoutKey) -> String` producing `"{}"` when `checkout_path` is empty and `"{}:{}"` when non-empty.
   - Fix `SnapshotRefused::Display` for `HiddenTrackedEdits`:
     - Give concrete safe two-command Git instructions (never combining `--no-skip-worktree` and `--no-assume-unchanged` in one call): for a single file, cite its path and `-C <checkout>` if `checkout_path` is non-empty; for multiple files, summarize count + list and show the two-command pattern `git update-index --no-skip-worktree -- <path>` followed by `git update-index --no-assume-unchanged -- <path>`.
   - Fix `SnapshotRefused::Display` for `UnrepresentedGitState`:
     - Add helper `worktree_git_invocation(git_dir: &Path) -> String`: if `git_dir.file_name() == Some(OsStr::new(".git"))` and `git_dir.parent()` is non-empty, return `format!("git -C {}", posix_quote(parent))`; otherwise return `format!("git --git-dir {}", posix_quote(git_dir))` (never pass `git_dir` to `--work-tree`).
     - Group unlocked worktrees sharing the same `git_dir` and `can_move_inside` capability: when there is 1 worktree, show its exact path and exact remove command (and `or move it inside the clone` if `can_move_inside && !locked`); when there are 2+ unlocked worktrees for the same `git_dir`, emit one concise summary (`"{n} unrepresented worktrees outside the clone; push or copy out anything wanted, then move each inside the clone or remove it with `{git_cmd} worktree remove --force <path>` before suspending"`, omitting the move clause if `!can_move_inside`), while locked worktrees are always called out individually with their `worktree unlock` + `worktree remove --force` commands. Keep `InventoryBlocker.paths` carrying every individual worktree path so UIs render the full bulleted list.
   - Clean up multi-item grouping for `IntentToAdd` and `UntrackedSubmoduleRegistration` when > 1 path shares a checkout so remedies stay concise while naming every affected checkout/path cleanly.

2. In `mj-core/src/suspend_consent.rs`:
   - Add `SuspendBlocker::title(&self) -> &str` mapping the `snake_case` `kind` codes to plain human titles (`"hidden_tracked_edits"` -> `"Hidden tracked edits"`, `"unrepresented_git_state"` -> `"External worktrees & Git state"` or `"External worktrees"` when appropriate, `"dirty_submodule"` -> `"Dirty submodule"`, `"missing_managed_origin"` -> `"Missing Git origin"`, `"origin_removed_by_teardown"` -> `"Origin depends on clone"`, `"intent_to_add"` -> `"Intent-to-add files"`, `"untracked_submodule_registration"` -> `"Untracked submodule registration"`, `"tracked_keep_files"` -> `"Tracked .mj/keep files"`, `"operation_in_progress"` -> `"Git operation in progress"`, `"untracked"` -> `"Untracked items"`), falling back to `&self.kind` if unknown.
   - Add `SuspendBlocker::is_auto_fixable(&self) -> bool` returning `matches!(self.kind.as_str(), "hidden_tracked_edits" | "unrepresented_git_state")` (and helper `SuspendPreflight::has_auto_fixable_blockers(&self) -> bool`).
   - Add `format_blocker_display_path(path: &str, max_len: usize) -> String` (or concise path display helper) and `format_blocker_cleanup_prompt(blockers: &[SuspendBlocker]) -> String` (used in Milestone 3).

3. In `mj-controller/src/controller/lifecycle.rs:551` and `mj-worker/src/checkpoint.rs:186`:
   - Replace `kind: format!("{:?}", b.kind)` with `kind: b.kind.kind_code().to_owned()`.
   - When building `paths` for `SuspendBlocker`, preserve checkout context when `p.checkout.checkout_path` is non-empty (e.g., join `checkout_path` and `value` when `value` is relative, or use `value` directly when `value` is absolute as for external worktrees).

4. In `mj-tui/src/dialogs/render.rs`:
   - Replace the single-line `  {} — {}` loop in `Confirmation::SuspendSession` (`:1599-1604`) with structured numbered sections:
     - Heading line (styled warning/bold): `"{}. {} ({} {})"`, using `blocker.title()` and `blocker.paths.len()` when non-empty.
     - Bulleted path lines from `blocker.paths`: show up to 5 paths per blocker unless `*viewing_all_untracked` (rename or reuse the `v` toggle field as `viewing_all_items` / keep `viewing_all_untracked` so `v` expands BOTH long blocker path lists and long untracked lists!), showing `"  and {N} more (v to view all)"` when collapsed.
     - Indented `"  Fix: {}"` line with `blocker.remedy`, plus a blank spacer line between blockers.

5. In `mj-server/src/web/viewer.js` (and `mj-server/src/web/viewer.css` if tiny styling helps):
   - Add a small `blockerTitle(kind)` map mirroring `SuspendBlocker::title()`.
   - In `confirmSessionSuspension` (`:6931-6939`), render each blocker as a structured section (`<div>` / `<li>`) with a bold heading + item count, a collapsible list of `b.paths` (first 5 visible + `Show N more` toggle), and a readable fix paragraph.

6. In `mj-cli/src/api_commands.rs` (`:2806-2811`) and `mj-cli/src/main.rs` (`:2026-2030`):
   - Render blockers in `mj suspend --dry-run` and `mj move --dry-run` using `b.title()`, bulleted `b.paths` (if any), and indented `fix: {}`.

7. Update existing tests and add new tests:
   - Update `mj-checkpoint/src/snapshot.rs` assertions for the fixed `git -C ... worktree remove --force` command and add a tricky-condition test for `HiddenTrackedEdits` detection + two-command `Display` output + multi-worktree grouped `Display` output (including locked vs unlocked).
   - Update `mj-cli/tests/golden/move-dry-run.txt` and `mj-cli/src/main.rs` golden test (`golden_move_dry_run_reports_snapshot_blockers`).
   - Extend `mj-tui/src/dialogs/tests.rs` and `mj-tui/tests/golden/dashboard-dialogs.txt` with a blocked `SuspendSession` golden case (including multi-worktree collapse + `v` expansion).

### Milestone 2: Safe one-click blocker remediation (`resolve-suspend-blockers`)

Goal: Let the user clear `skip-worktree`/`assume-unchanged` index flags and move eligible unlocked external worktrees inside `<clone>/.mj/worktrees/<name>` safely in one action across local, container, and SSH targets, from TUI, Web UI, and CLI.

Edits:

1. In `mj-checkpoint/src/snapshot.rs` (or a sibling submodule under `mj-checkpoint/src/`):
   - Define `SafeBlockerResolution`:
     - `pub cleared_hidden_paths: Vec<String>`
     - `pub moved_worktrees: Vec<MovedWorktree>` (`from: PathBuf`, `to: PathBuf`)
     - `pub skipped_worktrees: Vec<String>` (e.g., locked or `!can_move_inside` worktrees that were intentionally left untouched)
   - Implement `pub fn resolve_safe_suspend_blockers(runner: &dyn GitCommandRunner, repositories: &[RepositoryInventoryInput]) -> Result<SafeBlockerResolution>`:
     - For each non-raw repository (and raw repositories without an active operation, matching `inventory_blockers` gating):
       - **Hidden tracked edits**: inspect the main checkout, each discovered in-clone linked worktree, and each initialized submodule (recursing via `inspect_snapshot_checkout` / submodule discovery). For each `(checkout_dir, relative_paths)` where `hidden_tracked_paths(runner, &checkout_dir)` is non-empty, run TWO separate `git update-index` commands in `checkout_dir`:
         1. `git update-index --no-skip-worktree -- <paths...>`
         2. `git update-index --no-assume-unchanged -- <paths...>`
         (Chunking path args if huge, e.g., 100 paths per call, to stay well below `ARG_MAX`.) Record each cleared path in `cleared_hidden_paths`.
       - **Unrepresented worktrees** (only when `!input.raw`): run `StructuralInspection::inspect` to obtain `unrepresented_worktrees`. For each entry:
         - If `entry.can_move_inside && !entry.locked`:
           - Determine clone root from `input.checkout` (canonicalized).
           - Create `<clone>/.mj/worktrees` (`fs::create_dir_all`).
           - Pick a non-colliding directory name from `entry.path.file_name()` (falling back to `"worktree"`), appending `-2`, `-3`, ... if `<clone>/.mj/worktrees/<name>` already exists.
           - Run `git worktree move -- <entry.path> <dest>` in `input.checkout` (using `runner`).
           - Record `MovedWorktree { from: entry.path, to: dest }`.
         - Else record a concise explanation in `skipped_worktrees` (e.g. locked or contains submodules).

2. In `mj-core/src/suspend_consent.rs`:
   - Define `WorkerResolveBlockersSpec` (with `repositories: Vec<WorkerPreflightRepo>`), `MovedWorktreeSummary { from: String, to: String }`, `WorkerResolveBlockersResult { cleared_hidden_paths: Vec<String>, moved_worktrees: Vec<MovedWorktreeSummary>, skipped_worktrees: Vec<String> }`, and HTTP response type `ResolveSuspendBlockersResponse { cleared_hidden_paths: Vec<String>, moved_worktrees: Vec<MovedWorktreeSummary>, skipped_worktrees: Vec<String>, preflight: SuspendPreflight }`.

3. In `mj-worker/src/checkpoint.rs`, `mj-worker-bin/src/main.rs`, and `mj-session-runtime/src/checkpoint_transfer.rs`:
   - Add `WorkerCommand::ResolveSuspendBlockers { spec: PathBuf }` (`resolve-suspend-blockers --spec -`), calling `mj_worker::checkpoint::resolve_suspend_blockers_from_spec_file`.
   - Add `resolve_suspend_blockers_command` in `mj-session-runtime/src/checkpoint_transfer.rs` mirroring `suspend_preflight_command`.

4. In `mj-controller/src/controller/lifecycle.rs`:
   - Add `pub async fn resolve_suspend_blockers(&self, session_id: &str, executor: &(impl CommandExecutor + Sync)) -> Result<ResolveSuspendBlockersResponse>`:
     - Builds the exact same repository list as `checkpoint_preflight`.
     - For `LocalBare`, calls `mj_checkpoint::snapshot::resolve_safe_suspend_blockers` directly via `ExecutorGit`.
     - For remote/container locators, runs `worker resolve-suspend-blockers --spec -` (with the same staged-worker retry fallback when stderr contains `unrecognized subcommand 'resolve-suspend-blockers'` as `suspend_preflight` uses).
     - Immediately runs `self.suspend_preflight(session_id, false, false, false, executor)` and returns `ResolveSuspendBlockersResponse`.

5. In `mj-server`, `mj-client`, and `mj-cli`:
   - Add `POST /api/v1/sessions/{id}/suspend-resolve-blockers` route in `mj-server/src/server/api/routes.rs`, handler in `mj-server/src/server/api/turns.rs` and `mj-server/src/server/handlers.rs` (running off the event loop under daemon task admission).
   - Add client methods in `mj-client/src/http/terminal_operations.rs` and `mj-cli/src/api_client.rs`.
   - Add `--fix-blockers` flag to `SuspendArgs` in `mj-cli/src/api_commands.rs`: when set, calls `resolve_suspend_blockers` first, prints a concise summary of cleared flags / moved worktrees, and proceeds with suspend (or fails cleanly with remaining blockers).

6. In `mj-tui` and `mj-server/src/web/viewer.js`:
   - TUI: add a `resolving_blockers: bool` state (or `preflight: None` with a status label `"Fixing safe blockers…"`) on `Confirmation::SuspendSession`, show button `"Fix safe blockers"` when `preflight.has_auto_fixable_blockers()`, dispatch `DashboardAction::ResolveSuspendBlockers { session_id }` off the render loop in `mj-cli/src/dashboard/actions.rs`, and feed the refreshed `SuspendPreflight` + notice back into the existing dialog (reusing `apply_suspend_preflight` so if blockers are now gone and there are no warnings, it offers normal suspend immediately).
   - Web UI: in `confirmSessionSuspension`, when any blocker is `hidden_tracked_edits` or `unrepresented_git_state`, show `"Fix safe blockers & retry"` button that POSTs `/api/v1/sessions/{id}/suspend-resolve-blockers`, displays a brief summary of what was fixed, and re-renders the dialog body with `response.preflight`.

### Milestone 3: "Ask session to fix" action (TUI + Web UI)

Goal: Let the user hand any suspend blockers directly to the session's agent with one click/keypress from the blocked suspend modal.

Edits:

1. In `mj-core/src/suspend_consent.rs`:
   - Implement `pub fn format_blocker_cleanup_prompt(blockers: &[SuspendBlocker]) -> String` producing a concise, actionable prompt listing each blocker's title, paths, and fix guidance, asking the session to inspect each item, preserve/push any wanted work, resolve the blockers, and report back when ready to suspend.

2. In `mj-tui`:
   - In `Confirmation::SuspendSession`, track `can_prompt: bool` (true when the target session is in a running/live/parked-promptable state, i.e., not `Stopped`/`Failed`/`Lost`).
   - When `preflight.has_blockers()` and `can_prompt`, include `"Ask session to fix"` in `confirmation_buttons`.
   - Activating `"Ask session to fix"` dismisses the modal and returns `DashboardAction::PromptSession { session_id, text: format_blocker_cleanup_prompt(&preflight.blockers) }`.
   - In `mj-cli/src/dashboard/actions.rs`, handle `DashboardAction::PromptSession` off the render loop by submitting `ControllerAction::Prompt` (via the daemon API client, matching how live prompts submit) and surfacing a notice (`"Sent suspend-blocker cleanup request to <session>."`) or error notice on failure.

3. In `mj-server/src/web/viewer.js`:
   - Add `formatBlockerCleanupPrompt(blockers)` matching the Rust helper.
   - In `confirmSessionSuspension`, when `blockers.length > 0` and the session is promptable (not `stopped`/`failed`/`lost`), add an `"Ask session to fix"` button that calls `sendSubmission({ action: 'prompt', session_id: session.id, text })`, closes the modal, and reports status.

### Milestone 4: "Suspend with session history only"

Goal: Let the user suspend a session saving a real verified checkpoint archive of its conversation history and native harness state while skipping workspace Git capture/blockers, and resume cleanly into a fresh workspace checkout with conversation history intact.

Edits:

1. In `mj-core/src/suspend_consent.rs`:
   - Add to `SuspendConsent`:

         #[serde(default, skip_serializing_if = "std::ops::Not::not")]
         pub history_only: bool,

   - Update `SuspendConsent::validate()` (or check in controller/API) to refuse when both `without_checkpoint` and `history_only` are true.
   - Ensure `SuspendPreflight::shown_consent()` sets `history_only: false`.

2. In `mj-store/src/database.rs` and `mj-store/src/database/schema.rs`:
   - Bump `SCHEMA_VERSION` from `158` to `159`.
   - Add breaking migration 159 (O(1), no DDL needed) raising `minimum_compatible_version` to `159`, with reason: `"breaking: sessions.closing_consent JSON may include history_only, which older daemons reject via deny_unknown_fields during crash recovery"`.
   - Add/update schema compatibility tests in `mj-store` verifying migration 158 -> 159 and refusal of older daemons.

3. In `mj-controller/src/controller/checkpoint/lease.rs`, `latched.rs`, and `lifecycle.rs`:
   - Add `CheckpointExportPolicy::HistoryOnly` to `lease.rs`.
   - In `lifecycle.rs`:
     - Update `checkpoint_preflight` / `suspend_preflight` to accept `history_only: bool`: when true, skip workspace Git inventory (return empty blockers/untracked/operations, keeping session-level checks like `interrupting` and `owning_children_to_suspend`).
     - In `suspend_session_with_consent`: validate `!(consent.without_checkpoint && consent.history_only)`. When `consent.history_only` is true, select `export_policy = CheckpointExportPolicy::HistoryOnly` instead of `explicit_suspend_export_policy_with_executor`.
     - Ensure `preserves_session_checkpoint` is `false` for `HistoryOnly` (so `record.checkpoint = Some(artifact.metadata)` IS persisted!), `publication` is `None` (no workspace git changes were published), and `is_origin_delta` is `false` (so `OriginDeltaSuspend` preflight/inventory/quiesce blockers do NOT run).
     - On teardown of a `ManagedWorktree::Clone` with `CheckpointExportPolicy::HistoryOnly`, pass `CloneRetirementPrerequisite::None` to `retire_managed_worktree` so retirement does not require the legacy `base_commit` check.
   - In `latched.rs`:
     - When `export_policy == CheckpointExportPolicy::HistoryOnly`, rewrite every repository in `layout.repositories` whose capture is not `CheckpointRepositoryCapture::PlainDirectory` to `CheckpointRepositoryCapture::MetadataOnly`, and pass `recovery_copy` unchanged (false for explicit suspend).

4. In `mj-controller/src/controller/worktree.rs`, `network_git.rs`, and `resume.rs`:
   - In `worktree.rs`: add `CloneSeedMode::FreshHead` (identical to `FreshBranch` except `git switch --no-track -C <branch> HEAD` instead of using `checkout.base_commit`), and add `restore_managed_clone_fresh_head(executor, worktree)` (or pass a `fresh_head: bool` option to `restore_managed_worktree`).
   - In `network_git.rs` (`bundle_from_manifest_for_session`): permit `metadata.review_refs_metadata_only && !metadata.origin.is_empty()` alongside `metadata.remote_workspace || origin_backed_delta` so a history-only managed clone can also resume onto a container workspace.
   - In `resume.rs`:
     - Detect `let history_only_archive = !archive_manifest.repositories.is_empty() && archive_manifest.repositories.iter().all(|r| r.metadata.review_refs_metadata_only || r.metadata.plain_directory);`.
     - Skip `ensure_legacy_worktree_cross_target_base_on_origin` when `history_only_archive` is true.
     - When `!origin_delta && previous_managed_worktree.is_some()` and `history_only_archive && worktree.kind == ManagedCheckoutKind::Clone`: recreate the retired clone using `CloneSeedMode::FreshHead` instead of requiring `base_commit`.
     - When `history_only_archive` is true and the resumed checkout is managed (not an in-place attached raw checkout), append a clear resume notice: `"Resumed from a history-only checkpoint into a fresh workspace checkout; prior workspace Git changes were not saved."`.

5. In `mj-server`, `mj-client`, `mj-cli`, `mj-tui`, and `mj-server/src/web/viewer.js`:
   - Add `#[serde(default)] pub history_only: bool` to `SuspendRequest` and `SuspendPreflightQuery` in `mj-server/src/server/api/turns.rs`, thread through `mj-server/src/server/handlers.rs`, `mj-client/src/http/terminal_operations.rs`, and `mj-cli/src/api_client.rs`.
   - In `mj-cli/src/api_commands.rs`: add `--history-only` to `SuspendArgs` (`conflicts_with = "without_checkpoint"`), pass `history_only: true` in `SuspendConsent` and preflight dry-run.
   - In `mj-tui/src/dialogs.rs` and `mj-tui/src/dialogs/render.rs`:
     - Add `confirming_history_only: bool` to `Confirmation::SuspendSession`.
     - When `preflight.has_blockers()` and `!confirming_history_only`, include button `"Suspend (history only)…"` in the blocked dialog.
     - Selecting `"Suspend (history only)…"` toggles `confirming_history_only = true` in place, showing a clear warning (`"Discard workspace Git changes and save session history only? Resuming will start from a fresh checkout with your conversation history intact."`) and buttons `["Back", "Suspend with history only"]`.
     - Confirming dispatches suspend with `SuspendConsent { history_only: true, ..Default::default() }`.
   - In `mj-server/src/web/viewer.js`:
     - In `confirmSessionSuspension`, when blocked, add `"Suspend (history only)…"` button. Clicking it swaps the dialog controls to an explicit confirmation prompt with the same warning and `Back` / `Suspend with history only` buttons; confirming resolves with `{ history_only: true }`, which `runSessionAction` posts in the suspend body.

## Concrete Steps

All commands run from the repository root `/home/jonathan/Projects/mjolnir/.mj/clones/ab6d48be4383eb18b952c4c795c35ce2` using `mbx` (never plain `cargo` on an `mbx` workspace). Test invocations run outside the restricted sandbox because loopback TCP and Unix sockets are exercised.

After each milestone, run targeted tests and clippy on the affected crates:

    mbx test -p mj-checkpoint <targeted_filters>
    mbx test -p mj-core <targeted_filters>
    mbx test -p mj-store <targeted_filters>
    mbx test -p mj-worker <targeted_filters>
    mbx test -p mj-controller <targeted_filters>
    mbx test -p mj-tui <targeted_filters>
    mbx test -p mj-server <targeted_filters>
    mbx test -p mj-cli <targeted_filters>
    mbx clippy -p <affected crates> --all-targets -- -D warnings

For `mj-chat` or TUI goldens, run under `TZ=UTC` if timestamps are rendered, and verify per-filter test counts are non-zero.

## Validation and Acceptance

1. **Readable blockers (Milestone 1)**:
   - Preflighting a checkout with `HiddenTrackedEdits` and 7 external `UnrepresentedWorktree`s returns `SuspendBlocker.kind` values `"hidden_tracked_edits"` and `"unrepresented_git_state"` (zero Rust `Debug` tokens like `Scoped {` or `CheckoutKey {`).
   - `UnrepresentedGitState` remedy emits `git -C '<clone>' worktree remove --force ...` (never `--work-tree '.../.git'`), collapses multiple unlocked worktrees for the same clone into one summary while keeping all 7 paths in `blocker.paths`, and calls out locked worktrees individually with `worktree unlock`.
   - `HiddenTrackedEdits` remedy emits two separate `git update-index` commands (`--no-skip-worktree` and `--no-assume-unchanged`).
   - TUI golden `golden_dashboard_dialogs`, CLI golden `golden_move_dry_run_reports_snapshot_blockers`, and Web UI blocker test verify structured headings, bulleted paths with collapse/expand, and fix lines.

2. **Safe blocker remediation (Milestone 2)**:
   - Tricky-condition test in `mj-checkpoint`: creates a clone with BOTH `skip-worktree` (`S`) and `assume-unchanged` (`s`) files in the main checkout, an in-clone linked worktree, AND an initialized submodule, plus two unlocked external worktrees (`can_move_inside = true`) with colliding basenames and one locked external worktree. Calling `resolve_safe_suspend_blockers` clears every hidden flag across all three indexes (verified via `git ls-files -v`), moves both unlocked external worktrees under `<clone>/.mj/worktrees/` without collision, leaves the locked worktree untouched and reported in `skipped_worktrees`, and a subsequent `session_inventory` reports zero `HiddenTrackedEdits` and only the locked worktree (and captures the moved worktrees cleanly).
   - Controller/HTTP integration test exercises `POST /api/v1/sessions/{id}/suspend-resolve-blockers` and verifies the returned refreshed `SuspendPreflight` has no blockers.

3. **"Ask session to fix" (Milestone 3)**:
   - TUI and Web UI tests verify that the blocked suspend dialog shows `"Ask session to fix"`, builds a prompt naming every blocker and path, and dispatches `DashboardAction::PromptSession` (TUI) / `POST /api/v1/actions` with `action: "prompt"` (Web).

4. **"Suspend with session history only" (Milestone 4)**:
   - Store migration test verifies revision 158 -> 159 sets `minimum_compatible_version = 159` and round-trips `SuspendConsent { history_only: true, .. }` through `sessions.closing_consent`.
   - Controller integration test creates a managed clone session with conversation history AND workspace blockers (`skip-worktree` file + external worktree + uncommitted edit), suspends it with `SuspendConsent { history_only: true, .. }` (succeeding without resolving blockers), verifies the installed checkpoint archive is valid, carries `CanonicalSession` + `review_refs_metadata_only: true`, and has no Git bundle/patch payloads, then resumes the session into a fresh clone and verifies the conversation history is restored and the fresh-checkout notice is emitted.
   - TUI, Web UI, and CLI (`mj suspend --history-only`) tests verify the confirmation step and consent wiring.

## Idempotence and Recovery

- `resolve_safe_suspend_blockers` is idempotent: running it twice on an already-clean checkout succeeds with zero changes. If a `git worktree move` fails mid-way, earlier completed moves remain valid in-clone worktrees and retrying resumes cleanly from the remaining external worktrees.
- `SuspendConsent.history_only` is persisted in `sessions.closing_consent` before close begins, so a daemon crash mid-suspend recovers as a history-only suspend rather than switching to a full workspace checkpoint.
- Store migration 159 is O(1) metadata-only and does not touch large tables.

## Artifacts and Notes

Reference reconnaissance reports from this session (read-only exploration):

- `.mj/agents/63cab7584bab92018d394a90be0f3412/blocker-map.md`
- `.mj/agents/63cab7584bab92018d394a90be0f3412/actionable-map.md`
- `.mj/agents/c4ea94abed5afb059bea1cd10782afbe/suspend-checkpoint-map.md`
- `.mj/agents/ad7a606fb3c9820727bc9c0ff84f1586/design-recon.md`

## Interfaces and Dependencies

In `mj-checkpoint/src/snapshot.rs`:

    impl SnapshotRefused {
        pub fn kind_code(&self) -> &'static str;
    }

    pub struct MovedWorktree {
        pub from: PathBuf,
        pub to: PathBuf,
    }

    pub struct SafeBlockerResolution {
        pub cleared_hidden_paths: Vec<String>,
        pub moved_worktrees: Vec<MovedWorktree>,
        pub skipped_worktrees: Vec<String>,
    }

    pub fn resolve_safe_suspend_blockers(
        runner: &dyn GitCommandRunner,
        repositories: &[RepositoryInventoryInput],
    ) -> Result<SafeBlockerResolution>;

In `mj-core/src/suspend_consent.rs`:

    impl SuspendBlocker {
        pub fn title(&self) -> &str;
        pub fn is_auto_fixable(&self) -> bool;
    }

    impl SuspendPreflight {
        pub fn has_auto_fixable_blockers(&self) -> bool;
    }

    pub fn format_blocker_cleanup_prompt(blockers: &[SuspendBlocker]) -> String;

    pub struct SuspendConsent {
        #[serde(default)]
        pub discard_untracked: UntrackedConsent,
        #[serde(default)]
        pub abort_operations: OperationConsent,
        #[serde(default)]
        pub without_checkpoint: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub confirm_no_checkpoint: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub history_only: bool,
    }

    pub struct WorkerResolveBlockersSpec {
        pub repositories: Vec<WorkerPreflightRepo>,
    }

    pub struct MovedWorktreeSummary {
        pub from: String,
        pub to: String,
    }

    pub struct WorkerResolveBlockersResult {
        pub cleared_hidden_paths: Vec<String>,
        pub moved_worktrees: Vec<MovedWorktreeSummary>,
        pub skipped_worktrees: Vec<String>,
    }

    pub struct ResolveSuspendBlockersResponse {
        pub cleared_hidden_paths: Vec<String>,
        pub moved_worktrees: Vec<MovedWorktreeSummary>,
        pub skipped_worktrees: Vec<String>,
        pub preflight: SuspendPreflight,
    }

HTTP endpoints (`mj-server/src/server/api/routes.rs`):

- `POST /api/v1/sessions/{id}/suspend-resolve-blockers` -> `200 OK` JSON `ResolveSuspendBlockersResponse`
- `GET /api/v1/sessions/{id}/suspend-preflight?dry_run=&without_checkpoint=&history_only=` -> `200 OK` JSON `SuspendPreflight`
- `POST /api/v1/sessions/{id}/suspend` body `SuspendRequest` adds `#[serde(default)] pub history_only: bool`

Milestone 1 implementation note (2026-11-25): Completed readable blocker codes,
titles, structured CLI/TUI/web presentation, path expansion, grouped worktree
guidance, and separate hidden-index flag commands. Validation: checkpoint
snapshot filter ran 47 tests (45 initially passed; two text expectations fixed
and each failing test rerun successfully); all 36 TUI and 10 CLI goldens ran,
with the corrected blocked-dialog fixture subsequently regenerated and its test
verified. Controller preflight: 6; worker preflight: 1 and restore stdin: 1;
core wire schema: 1; server suspend: 1; web viewer: 40. Seven affected crates
passed dev clippy with all targets and warnings denied. Detailed logs and exact
commands are in the implementation worker report directory. Milestones 2–4
remain unimplemented. No commit or push was made, as requested.
