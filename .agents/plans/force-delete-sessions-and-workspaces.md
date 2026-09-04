# Force delete sessions and workspaces

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

A coding session that is wedged in an active state (for example stuck in `provisioning`, or sitting in `error`, or one whose close keeps failing) can never be deleted today, and any workspace that contains such a session can never be deleted either. After this change, the user can explicitly confirm data loss and permanently remove such a session — live container, worker files, managed Git worktree, recovery archive, and database record — from the terminal dashboard, and can likewise force-delete a workspace from the workspace picker: every active session in it is force-destroyed, its detached drafts are dropped, and the workspace disappears, while stopped session histories remain globally resumable exactly as before.

The user-visible outcome: select a stuck session in the sessions pane, open the command palette, run "Force destroy session", type that session's 8-character short id, press Enter, and the session and everything it owns is gone. In the workspace picker, press `D` on a non-empty workspace, type the workspace's exact name, press Enter, and the workspace and its live sessions are gone.

This is deliberately an escape hatch. Everything else in the product refuses to destroy a session without first saving a recovery archive; force destroy is the one place where the user says "I accept losing this work".

## Progress

- [x] (2026-09-04) ExecPlan recorded from the approved design.
- [x] (2026-09-04) M1: `HelState::destroy_session_force` and `force_delete_workspace`/`_at` with state and database behavior tests.
- [x] (2026-09-04) M2: `Controller::force_destroy_session`/`_with` with four controller tests, including real end-to-end teardown of a local target, worktree, branch, and archive through `ProcessExecutor`.
- [x] (2026-09-04) M3: daemon protocol 8, `ForceDestroySession`/`ForceDeleteWorkspace` actions, `ForceDestroy` lifecycle kind, `preempt_active_lifecycle`, workspace force orchestration, dashboard/server kind mappings, and daemon tests.
- [x] (2026-09-04) M4: palette command, typed short-id confirmation with the new `InputFilter::AsciiHexLowercase`, action dispatch through the supervised lifecycle machinery, and TUI/chat tests.
- [x] (2026-09-04) M5: selector `D` confirmation state, `SelectorOutcome::ForceDelete`, main.rs handling, and pure-helper tests.
- [x] (2026-09-04) Validation: `cargo fmt --all -- --check`, full default-member `cargo test`, and `cargo clippy --all-targets -- -D warnings` all pass outside the sandbox; five scoped commits on the current branch.

## Surprises & Discoveries

- Observation: `execute_target_cleanup` in `mj-controller/src/hel_controller/lifecycle.rs` already implements exactly the abort rule force destroy needs: when a cleanup command fails it probes whether the target is really gone; confirmed gone continues with a warning, anything else returns the error so the durable record survives for a retry.
  Evidence: the function at `mj-controller/src/hel_controller/lifecycle.rs:499` and its three-way match on `cleanup_target_is_confirmed_absent`.

- Observation: `PROTOCOL_VERSION` is pinned by a test, so a bump is a two-file change until the constant and the assertion are updated together.
  Evidence: `the_daemon_rejects_a_client_one_protocol_behind_before_dispatch` in `mj-cli/src/daemon.rs` asserts `PROTOCOL_VERSION == 7` and failed until updated to 8.

- Observation: mj-cli had no `tokio` dev-dependency with `test-util`, which `#[tokio::test(start_paused = true)]` needs to advance the 8-second preemption timeout instantly; the workspace's other crates already declare it in `[dev-dependencies]`.
  Evidence: the timeout test failed to compile until `tokio = { version = "1", features = ["test-util"] }` was added to `mj-cli/Cargo.toml`'s dev-dependencies.

- Observation: invoking the `rustfmt` binary directly produces a flood of unrelated diffs because it defaults to edition 2015; `cargo fmt` (which reads edition 2024 from the manifest) matches the committed baseline exactly on a clean tree of this host.
  Evidence: direct `rustfmt --check` failed on let-chains and async fns repo-wide, while `cargo fmt --all -- --check` reported no diff on the untouched tree.

- Observation: clippy's `unused_assignments` catches dead state resets on early-return paths, which is how the selector's confirm-state cleanup was simplified before commit.
  Evidence: `cargo clippy --all-targets -- -D warnings` flagged `confirming = None;` immediately before a `return`.

## Decision Log

- Decision: Force destroy removes the record entirely rather than moving the session to the existing `destroyed-with-data-loss` state.
  Rationale: The user selected full permanent removal; the state stays available for historical databases but gains no new writer.
  Date/Author: 2026-09-04 / Ryan (product decision), Claude (design)

- Decision: Force-delete of a workspace destroys only its active sessions and detached drafts; stopped session histories are preserved globally.
  Rationale: Matches the safe-workspace-delete design from `.agents/plans/global-session-history-and-safe-workspace-delete.md`: inactive records are global resume history whose `workspace_id` is retained as metadata.
  Date/Author: 2026-09-04 / Ryan (product decision)

- Decision: Surfaces are the TUI dashboard and the workspace selector only. No new CLI subcommands, no web/remote viewer changes.
  Rationale: The controller's phone/web action protocol intentionally cannot express destructive force-cleanup (see its doc comment on `ControllerAction` in `mj-controller/src/hel_server.rs`), and `mj recover destroy` already covers untracked orphans from the CLI.
  Date/Author: 2026-09-04 / Ryan (product decision)

- Decision: When another lifecycle operation is in flight for the session, force destroy cancels it and waits (up to 8 seconds) instead of refusing.
  Rationale: A wedged in-flight create is precisely the scenario force destroy exists for; refusing would force the user through a separate cancel step. Waiting for the in-flight task to finish also prevents a cancelled create from re-persisting its database row after force destroy deleted it.
  Date/Author: 2026-09-04 / Claude

- Decision: Teardown reuses the existing close plan (`hel_targets::close_plan` via `execute_target_cleanup`), never the deferred Podman quiesce path.
  Rationale: The close plans stop the owning process group before removing files, are idempotent (`docker rm --force`, `podman rm --force --ignore`, `rm -rf`), and carry ownership guards. Force destroy removes everything immediately, so no deferred cleanup is scheduled.
  Date/Author: 2026-09-04 / Claude

- Decision: The workspace force-delete keeps both existing daemon guards — attached clients and an in-flight resume targeting the workspace — even in force mode.
  Rationale: Attached dashboards are live user interfaces, not wedges; the user can close them. An in-flight resume would recreate activity in a workspace being deleted.
  Date/Author: 2026-09-04 / Claude

- Decision: Confirmations name the thing destroyed. The session dialog requires the session's 8-character short id; the workspace picker requires the exact workspace name.
  Rationale: Force stop keeps a session resumable, so a fixed word ("STOP") is enough there. Force destroy is irreversible, so the confirmation should identify the exact row being destroyed — the same intent as `mj recover destroy`'s exact-ID rule at a typeable length.
  Date/Author: 2026-09-04 / Claude

- Decision: Pressing `D` in the workspace picker now always confirms before deleting, for both the empty and the force path.
  Rationale: Before this plan, `D` deleted with no confirmation at all; wiring the force path through the same state makes both paths safe and consistent.
  Date/Author: 2026-09-04 / Claude

- Decision: The force-delete name gate trims surrounding whitespace before comparing.
  Rationale: Leading or trailing spaces are accidental typing, not a different name; the name itself must still match exactly.
  Date/Author: 2026-09-04 / Claude

- Decision: The daemon's force-destroy task takes the same recovery reservation (`reserve_recovery_or_cancel`) the resume task takes.
  Rationale: Destruction must not race a recovery copy adopting or archiving the session mid-teardown; reservation is the existing mechanism for that exclusion.
  Date/Author: 2026-09-04 / Claude

## Outcomes & Retrospective

Implemented and validated on 2026-09-04. A wedged active session can now be permanently destroyed from the dashboard (palette → "Force destroy session" → type the 8-character short id), preempting whatever lifecycle operation holds it, and a non-empty workspace can be force-deleted from the selector by typing its exact name; stopped histories remain globally resumable. The daemon protocol advanced from 7 to 8 and older daemons are replaced automatically on first contact.

All planned automated tests landed and pass: core state and database behavior (3), controller force-destroy including a real end-to-end teardown and the target-survives abort (4), daemon preemption/enumeration/kind (5 plus the extended ownership and protocol tests), TUI confirmation gating, paste filtering, and palette availability (4), selector prompt and exact-name gating (4). The full default-member suite (`cargo test`) and `cargo clippy --all-targets -- -D warnings` pass outside the sandbox; `cargo fmt --all -- --check` is clean.

Known limitations, recorded rather than fixed: force destroy cannot rescue a store whose config fails `validate_against_config` (for example an active session referencing a deleted target template); the startup-only interrupted-close recovery tasks are outside the lifecycle map, so a narrow race can leave a stopped row whose archive is gone (self-healing: the resume dialog's destroy tolerates a missing archive); force-deleting several containers serially holds the selector's last frame for seconds, consistent with the selector's other awaited operations.

Manual interactive acceptance (creating a real session and destroying it from the palette, two attached dashboards refusing a workspace force-delete) was not run on this host; the state-machine, teardown, wire-shape, confirmation, and persistence boundaries are covered automatically as described above.

## Context and Orientation

Mjolnir (library crate name `hel`, package `brokk-mj-core`) is a session control plane for ACP coding agents. Terms the plan uses:

- A **session** is one durable coding-agent conversation. Its record lives in the `sessions` table of a single SQLite database; `session_contexts` holds its workspace membership; `SessionState` (`src/hel_state.rs`) is its lifecycle state. `SessionState::is_active()` is true for `Provisioning`, `Running`, `Disconnected`, `Checkpointing`, `Closing`, `Destroying`, and `Error`; only `Stopped`, `Lost`, and `DestroyedWithDataLoss` are inactive.
- A **workspace** is a named group of sessions (`workspaces` table). Stopped sessions are global resume history: their `session_contexts.workspace_id` survives the workspace's deletion as historical metadata (no foreign key).
- A **target** is where a session runs: a local directory, a Docker/Podman container, an SSH host, and so on (`session_targets` table, `hel_targets` module). Active sessions own a live target; teardown must stop the owning process group before removing files.
- The **daemon** (`mj-cli/src/daemon.rs`) is the persistent background process that owns the database writer and runs lifecycle operations (create, close, resume, force-stop, destroy-stopped, cleanup), one at a time per session, tracked in an in-memory `lifecycle` map of `ActiveLifecycle { kind, cancelled: Arc<AtomicBool>, result: watch::Receiver<Option<Result<DaemonLifecycleResult, String>>>, … }`. Clients (the TUI dashboard, the workspace selector) talk to it over a JSON protocol whose `PROTOCOL_VERSION` (currently 7) must be bumped whenever any non-management `DaemonAction` changes.
- The **controller** (`mj-controller`) loads authoritative state from SQLite (`Controller::load()`), validates it against the user config, and performs the actual lifecycle work; it is stateless between operations.
- The **recovery archive** (checkpoint) is the saved copy that makes a stopped session resumable. Destroying it is what makes force destroy irreversible.
- The **workspace selector** (`mj-cli/src/workspace_selector.rs`) is the terminal picker shown before a dashboard attaches; `D` currently deletes a workspace with no confirmation.

Key existing code this plan builds on:

- `Controller::destroy_session_controlled` (`mj-controller/src/hel_controller/lifecycle.rs:460`): permanently destroys an inactive session — retires the Git broker, removes the managed worktree and generated branch, unlinks the recovery archive (a missing file is tolerated; other errors abort before the database delete so the record stays retryable), deletes the database row, then removes the in-memory record. It refuses active sessions.
- `execute_target_cleanup` (`mj-controller/src/hel_controller/lifecycle.rs:499`): runs `hel_targets::close_plan` and, on failure, probes `cleanup_target_is_confirmed_absent`; confirmed-absent continues, otherwise the error propagates (record retained).
- `HelState::destroy_stopped_session` (`src/hel_state.rs:1298`): in-memory removal that re-refuses active sessions.
- `delete_workspace` / `delete_workspace_at` (`src/hel_database.rs:584` / `:591`): refuses a workspace with active sessions or detached drafts, else deletes the row in an immediate transaction.
- The daemon's delete-workspace handler (`mj-cli/src/daemon.rs`, `DaemonAction::DeleteWorkspace`) additionally refuses attached clients and in-flight resumes into the workspace.
- `force_stop_session` and `destroy_stopped_session` (`mj-cli/src/daemon.rs:1489` / `:1520`): the shape every daemon lifecycle method uses — `run_lifecycle` wraps a `spawn_blocking` task that loads a controller and runs it behind `DaemonStageReportingExecutor::new(CancellableProcessExecutor::new(cancelled), …)`.
- The typed-confirmation dialog pattern: `FORCE_STOP_CONFIRMATION = "STOP"` in `mj-tui/src/dialogs.rs:31`, handled by `handle_typed_confirmation_key` at `:1989` — buttonless, Enter gated on exact text, input filtered and capped, paste filtered at `mj-tui/src/lib.rs:855`.
- The command palette registry (`mj-tui/src/actions.rs`): `CommandId` entries with scope, availability predicate, label, and optional key binding.

## Plan of Work

Five milestones, each independently testable. M1 and M2 add the core and controller primitives; M3 exposes them over the daemon protocol; M4 and M5 are the two user surfaces. After each milestone, run the validation commands in Concrete Steps and keep the Progress section current.

### Milestone 1 — Core removal primitives (crate `hel`)

Add two small primitives the rest of the plan composes.

First, in `src/hel_state.rs`, next to `destroy_stopped_session`, add `destroy_session_force(&mut self, session_id: &str) -> Result<SessionRecord>` — the same lookup-and-remove, without the `is_active` refusal. Force destruction is its only caller, and by the time it runs every external artifact has been torn down or its loss accepted, so no state is refused. The existing `destroy_stopped_session` keeps its guard.

Second, in `src/hel_database.rs`, next to `delete_workspace_at`, add a writer-lane pair `force_delete_workspace(workspace_id)` / `force_delete_workspace_at(path, workspace_id)`. The `_at` variant opens an immediate transaction, counts active sessions for the workspace exactly as `delete_workspace_at` does (the `session_contexts`/`sessions` join filtered through `parse_session_state(...).is_active()`), refuses with `workspace is not empty ({active_count} active sessions remain)` if any remain (the daemon destroys them first; this re-check closes the window where one was created concurrently), deletes the workspace's `detached_drafts` rows, then deletes the `workspaces` row (refusing `unknown workspace {workspace_id:?}` when the row count is zero). Doing the draft deletion inside the same transaction as the guard matters because `detached_drafts.workspace_id` is a foreign key with no cascade: the workspace row cannot go while drafts reference it, and drafts must not be dropped unless the whole deletion is committed.

### Milestone 2 — Controller force destroy

In `mj-controller/src/hel_controller/lifecycle.rs`, next to `destroy_session_controlled`, add:

    pub fn force_destroy_session(&mut self, session_id: &str, executor: &impl CommandExecutor) -> Result<()>
    fn force_destroy_session_with(&mut self, session_id: &str, executor: &impl CommandExecutor, delete: impl Fn(&str) -> Result<()>) -> Result<()>

The public method delegates to the `_with` form passing `hel::hel_database::delete_session`, mirroring how `destroy_after_verified_checkpoint_with` injects persistence so tests can run without the global database writer. The `_with` body runs, in order: clone the record (`unknown session {session_id}` when absent, no state guard); `retire_git_broker` with context `stop the session's local Git broker` (first, so no live writer restarts against a target being removed); if the record has a target, `backend_locator` then `execute_target_cleanup` (no extra context, matching the force-stop path) — a target confirmed still present aborts here, keeping the record and archive for a retry, and a missing target simply skips teardown; if there is a managed worktree, `cleanup_managed_worktree` with context `remove managed raw-session worktree`; remove the recovery archive exactly as `destroy_session_controlled` does (tolerate `NotFound`, otherwise fail with `remove session recovery archive {path}`); call the injected `delete` with context `force destroy session in database`; finally `self.state.destroy_session_force(session_id)`.

The broker-first, teardown-before-database order keeps every failure visible and retryable: nothing durable is dropped until everything external has actually gone.

### Milestone 3 — Daemon protocol, kinds, and handlers

In `mj-cli/src/daemon.rs`:

- Bump `PROTOCOL_VERSION` from 7 to 8. The frozen management subset (`Ping`, `Status`, `Stop`) is untouched; an older running daemon is replaced automatically on the next `connect_or_start`.
- Add `DaemonAction::ForceDestroySession { session_id: String }` and `DaemonAction::ForceDeleteWorkspace { workspace_id: String }`, with `DaemonClient` wrappers `force_destroy_session` and `force_delete_workspace` that expect `DaemonReply::Done`.
- Add `LifecycleKind::ForceDestroy` and its runtime mirror `RuntimeLifecycleKind::ForceDestroy` (serialized `force_destroy`), plus the `From` arm. This is what makes every attached dashboard show the destroying overlay, mark the open chat retiring, and settle when the record disappears — no new event stream is needed. `lifecycle_owns_worker_target` already treats every non-`Close` kind as owning the worker target, so worker polling pauses during teardown with no change there.
- Add `preempt_active_lifecycle(self, session_id)`: under the lifecycle lock, find the entry for the session whose result watch still reads `None` (no entry or all completed → success); set its `cancelled` flag; outside the lock, wait on the result watch with `tokio::time::timeout(FORCE_DESTROY_PREEMPT_TIMEOUT, …)` where the constant is `Duration::from_secs(8)`. On timeout return `session {session_id} still has an operation that did not stop after cancellation; try again`; if the watch channel closes without a result, return `daemon lifecycle operation stopped without a result for session {session_id}`. Nothing destructive happens before the preempt succeeds.
- Add `RuntimeState::force_destroy_session(session_id)`: preempt, then check existence with a fresh `Controller::load` (return success idempotently when the record is gone, mirroring `destroy_stopped_session`), then `run_lifecycle` with the new kind, following the `force_stop_session` task shape (controller load inside `spawn_blocking`, `DaemonStageReportingExecutor` wrapping `CancellableProcessExecutor`, result `DaemonLifecycleResult::Done`, and the same recovery reservation the close task takes).
- Add a free function `active_sessions_for_force_destruction(controller, workspace_id) -> Vec<String>`: the ids of the workspace's sessions whose state `is_active()`, ordered by `SessionRecord::compare_by_creation` so partial-failure messages are deterministic.
- Add `RuntimeState::force_delete_workspace(workspace_id)`: first the two existing guards with their existing strings — `workspace still has attached clients` (any attachment maps to this workspace) and `workspace has a session resume in progress` (any in-flight lifecycle whose resume destination is this workspace); then load a controller, list the workspace's active sessions, and force-destroy each in order. If one fails, stop and return `force-destroying session {id} failed: {error:#}; {remaining} session(s) in the workspace remain` (already-destroyed sessions stay destroyed, so a retry is idempotent and makes progress). Finally run `hel::hel_database::force_delete_workspace` on the writer lane and `refresh_runtime_workspaces`.
- Dispatch both new actions in the request handler, returning `DaemonReply::Done`.

Two mapping updates outside `daemon.rs`: `mj-cli/src/dashboard.rs` maps `RuntimeLifecycleKind::ForceDestroy` to `SessionOperationKind::Destroying` (the existing destroying overlay), and `mj-cli/src/server.rs` adds `ForceDestroy` to the arm that maps lifecycle kinds to `ViewerOperationKind::Stop` so the remote viewer's generic stop labeling keeps compiling and behaving.

### Milestone 4 — Dashboard force destroy

The entry point is the command palette, deliberately without a dedicated key, mirroring `StopSession`:

- `mj-chat/src/hel_text_input.rs`: add `InputFilter::AsciiHexLowercase` (accept `0-9a-fA-F`, lowercase the letters) to the existing filter enum and `insert_filtered`. Session ids are 32-character lowercase hex, so this filter both protects the input and lowercases pasted capitals.
- `mj-tui/src/actions.rs`: add `CommandId::ForceDestroySession` with label `Force destroy session`, description `Permanently remove the selected session, its target, and its recovery archive.`, session scope, no key binding, and the same availability predicate the other session commands use (a selected, ready session). It stays available while another operation is in flight on purpose — preempting that operation is the feature. `dispatch_command` opens the confirmation with `expected` set to `session.id.get(..8).unwrap_or(&session.id)` and a `TextInput` capped at 8 characters with the new filter.
- `mj-tui/src/dialogs.rs`: add `Confirmation::ForceDestroy { session_id, expected, typed }`. It is buttonless like `ForceStop`: `confirmation_buttons` returns an empty slice, and `handle_typed_confirmation_key` gains an arm mirroring the force-stop one — Escape cancels, Enter fires `DashboardAction::ForceDestroy { session_id }` only when the typed text equals `expected`, any other key edits the input and rebuilds the dialog. The title is ` FORCE DESTROY · THE SESSION AND ITS RECOVERY ARCHIVE WILL BE LOST `; the body names the session id, states that the target, managed worktree, recovery archive, and session record are removed and that nothing from the session can be resumed or read afterwards, then prompts `Type {expected} (this session's short id), then press Enter:` followed by the red typed line with its cursor marker.
- `mj-tui/src/lib.rs`: add `DashboardAction::ForceDestroy { session_id }`; add the new confirmation to `text_input_focused`; extend `handle_paste` with an arm that filters pasted text to ASCII hex, lowercases it, and respects the 8-character cap, exactly like the force-stop paste arm.
- `mj-cli`: add `LifecycleSuccess::ForceDestroyed` in `src/pollers.rs`; handle the action in `src/dashboard/actions.rs` exactly like `DestroyStopped` (mark the destroying operation in flight, mark the active chat retiring, spawn the supervised background operation calling `daemon.force_destroy_session`); in `src/dashboard/io.rs` map the success result to the notice `Permanently destroyed session {short_id}`. Failures already flow through the generic `Destroying failed: {error}` notice.

The dashboard's render loop never touches the filesystem or the daemon directly; the spawned lifecycle operation does the blocking work, which keeps the repository's responsiveness rule.

### Milestone 5 — Workspace selector force delete

- `mj-cli/src/workspace_selector.rs`: add `SelectorOutcome::ForceDelete(String)`. Pressing `D` no longer returns immediately; it enters a confirm state holding the highlighted workspace's id, name, active-session count, and unrecovered-draft count (all already present in the snapshot the selector renders). When the workspace is empty the footer reads `Delete workspace {name}? Enter confirm · Esc cancel` and Enter returns the existing `SelectorOutcome::Delete`. When it is not empty the footer reads `Force-delete {name} ({active} active session(s), {drafts} draft(s))? Type the workspace name, then Enter: {input}` and Enter is gated — via a pure helper comparing the trimmed input to the exact name — on the typed name, returning `SelectorOutcome::ForceDelete`. Escape or Ctrl-C cancels and clears the input; a mismatched Enter stays in the confirm state; the standard notice footer still takes precedence while a notice is showing. Extract the gating and the prompt text into pure functions so they are testable without a terminal.
- `mj-cli/src/main.rs`: handle the new outcome next to the existing delete handling — await `daemon.force_delete_workspace`, on failure set the protected notice `Could not force-delete workspace: {error:#}` and stay in the selector, on success clear the notice, reset the selection, and refresh the workspace list.

## Concrete Steps

Work from the repository root (the git worktree the task started in). After each milestone, format the files you touched and run that crate's tests, then continue; the full validation runs at the end.

For a quick loop while implementing:

    cargo test -p brokk-mj-core hel_state
    cargo test -p brokk-mj-core hel_database
    cargo test -p mj-controller force_destroy
    cargo test -p mj-cli daemon
    cargo test -p mj-tui dialogs
    cargo test -p mj-cli workspace_selector

Final validation, run from the repository root and outside the restricted sandbox (the suite exercises loopback sockets and can fail or hang inside it):

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings

Expected: every command exits zero; clippy prints no warnings. The default workspace members exclude the optional `mj-desktop` crate, which cannot build on this host — that is expected and matches prior plans. Note this host's rustfmt baseline differs from the committed formatting for `mj-controller/src/hel_controller.rs` and `mj-controller/src/hel_controller/worker_binary.rs`; do not include unrelated reformats of those files.

Then update this document's living sections with the results, stage only the task-owned files, and commit on the current branch. Do not push.

## Validation and Acceptance

Automated behavior tests, per boundary:

- Core state (`src/hel_state.rs`): force removal removes a `Running` session and errors on an unknown id; the pre-existing stopped-only guard test still passes, proving the safe path kept its refusal.
- Database (`src/hel_database/tests.rs`): a workspace holding one stopped session (with checkpoint, materialized projection, and prompt history) and one detached draft force-deletes successfully — the draft row is gone, the stopped session and all its history still load, the workspace is gone from the listing; a workspace still holding an active session refuses with `1 active sessions remain` and the draft row survives the refusal.
- Controller (`mj-controller/src/hel_controller/lifecycle.rs` tests, using the existing test-support fixtures, a recording executor, and a recording delete closure): from `Running` with a local target, managed worktree, and archive on disk, force destroy removes the worker root, the worktree, the generated `mj/{session_id}` branch, and the archive file, empties the in-memory state, and records the database deletion; from `Provisioning` with no target and no archive it still succeeds; when every cleanup command fails (target confirmed present) it returns the error, keeps the record and the archive (retryable); a checkpoint path pointing at a missing file is tolerated.
- Daemon (`mj-cli/src/daemon.rs` tests): the preempt helper cancels a gated in-flight lifecycle and returns once it finishes; with the clock paused it times out and reports `did not stop after cancellation` without destroying; the enumeration helper returns only the workspace's active session ids, oldest first; the worker-target ownership test covers the new kind.
- TUI (`mj-tui/src/dialogs.rs`, `actions.rs`, and `mj-chat/src/hel_text_input.rs` tests): the confirmation dialog fires `DashboardAction::ForceDestroy` only for the exact short id (wrong input plus Enter does nothing); a render pass shows the title and the type-the-id prompt; pasted text is filtered to lowercased hex and capped at eight characters; the palette entry appears only with a selected ready session; the new input filter drops non-hex characters.
- Selector (`mj-cli/src/workspace_selector.rs` tests): the footer prompt distinguishes the empty and force cases with the right counts; Enter is allowed for an empty workspace without typing, and for a force delete only when the trimmed input equals the exact workspace name (padded or wrong names are rejected).

Manual acceptance, from a terminal:

1. Start `mj`, create a session, open the palette (F2), run `Force destroy session`, type the row's short id, press Enter. The row disappears, the footer shows `Permanently destroyed session …`, and `docker ps` / `podman ps` in a second terminal shows the container gone.
2. Wedge a session (for example by killing the daemon mid-provision and restarting), then force-destroy the stuck provisioning row: the in-flight overlay is preempted and the row disappears.
3. Attach a second dashboard to workspace A (two terminals), press `D` on A from the first: the selector shows `Could not force-delete workspace: workspace still has attached clients`. Close the second dashboard, retry, type the workspace name: A disappears and its stopped histories still appear in another workspace's Resume view.
4. On an empty workspace, `D` shows the simple confirm and Enter deletes it, as before.

## Idempotence and Recovery

Force destroy is safe to retry: every teardown plan is idempotent, a missing archive is tolerated, and a session that is already gone makes the daemon call a successful no-op. If a step fails midway, the durable record survives, so the state on disk always describes what still exists. The workspace path destroys sessions one at a time before touching the workspace row; a mid-sequence failure leaves the remaining sessions intact and the workspace in place, and the immediate-transaction re-check means a concurrently created active session refuses the final deletion rather than silently losing drafts. Normal (non-force) deletion semantics are unchanged throughout.

No schema migration and no new dependency is required. The daemon protocol bump replaces older daemons automatically on first contact.

## Artifacts and Notes

The refusal this plan exists to bypass:

    refusing to destroy active session {session_id}      (mj-controller/src/hel_controller/lifecycle.rs)
    workspace is not empty ({n} active sessions, {m} drafts)   (src/hel_database.rs)

The confirmation gate the user sees before the irreversible step:

    FORCE DESTROY · THE SESSION AND ITS RECOVERY ARCHIVE WILL BE LOST
    Type {short id} (this session's short id), then press Enter:

## Interfaces and Dependencies

In `src/hel_state.rs`:

    pub fn destroy_session_force(&mut self, session_id: &str) -> Result<SessionRecord>

In `src/hel_database.rs`:

    pub fn force_delete_workspace(workspace_id: &str) -> Result<()>
    pub fn force_delete_workspace_at(path: &Path, workspace_id: &str) -> Result<()>

In `mj-controller/src/hel_controller/lifecycle.rs`:

    pub fn force_destroy_session(&mut self, session_id: &str, executor: &impl CommandExecutor) -> Result<()>

In `mj-cli/src/daemon.rs`:

    pub(crate) const PROTOCOL_VERSION: u32 = 8;
    // DaemonAction::ForceDestroySession { session_id: String }
    // DaemonAction::ForceDeleteWorkspace { workspace_id: String }
    // LifecycleKind::ForceDestroy (runtime mirror serialized as "force_destroy")
    const FORCE_DESTROY_PREEMPT_TIMEOUT: Duration;
    impl RuntimeState {
        async fn preempt_active_lifecycle(self: &Arc<Self>, session_id: &str) -> Result<()>;
        pub(crate) async fn force_destroy_session(self: &Arc<Self>, session_id: String) -> Result<()>;
        pub(crate) async fn force_delete_workspace(self: &Arc<Self>, workspace_id: String) -> Result<()>;
    }
    impl DaemonClient {
        pub(crate) async fn force_destroy_session(&mut self, session_id: String) -> Result<()>;
        pub(crate) async fn force_delete_workspace(&mut self, workspace_id: String) -> Result<()>;
    }
    fn active_sessions_for_force_destruction(controller: &Controller, workspace_id: &str) -> Vec<String>;

In `mj-chat/src/hel_text_input.rs`: `InputFilter::AsciiHexLowercase`.

In `mj-tui/src/lib.rs`: `DashboardAction::ForceDestroy { session_id: String }`.

In `mj-tui/src/dialogs.rs`: `Confirmation::ForceDestroy { session_id: String, expected: String, typed: TextInput }` (buttonless).

In `mj-tui/src/actions.rs`: `CommandId::ForceDestroySession` with one registry entry.

In `mj-cli/src/pollers.rs`: `LifecycleSuccess::ForceDestroyed`.

In `mj-cli/src/workspace_selector.rs`: `SelectorOutcome::ForceDelete(String)` plus the pure confirm helpers.

No new crate and no third-party dependency. Existing helpers to reuse rather than reimplement: `retire_git_broker`, `cleanup_managed_worktree`, `execute_target_cleanup`, `backend_locator`, `DaemonStageReportingExecutor`, `CancellableProcessExecutor`, `run_lifecycle`, `refresh_runtime_workspaces`, `Notices::set_failure`, and the typed-confirmation dialog machinery.
