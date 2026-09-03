# Make session history global and workspace deletion recoverable

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

Deleting a workspace from the terminal selector currently exits Mjolnir when the workspace contains stopped session records, even though those records are durable history rather than live work. After this change, operational failures in the selector appear in the standard yellow notice row without closing the selector, stopped histories are offered from every workspace's Resume view in both terminal and web clients, and resuming a history attaches it to the workspace from which the user resumed it. A workspace may be deleted when it has no active sessions, attached clients, in-flight lifecycle work, or recoverable drafts; deletion preserves all stopped session data.

## Progress

- [x] (2026-09-03 14:53Z) Traced workspace deletion, selector error propagation, workspace filtering, daemon runtime snapshots, TUI resume discovery, web resume actions, and SQLite ownership metadata.
- [x] (2026-09-03 14:53Z) Recorded the approved design and implementation plan.
- [x] (2026-09-03 15:16Z) Changed database workspace counting/deletion and added guarded resume rebinding.
- [x] (2026-09-03 15:16Z) Made inactive histories globally visible through dashboard and daemon projections.
- [x] (2026-09-03 15:16Z) Kept selector operation failures inside the TUI notice machinery.
- [x] (2026-09-03 15:16Z) Carried the destination workspace through terminal, daemon, and web resume requests.
- [x] (2026-09-03 15:16Z) Added focused database, selector, daemon, dashboard, and web behavior tests.
- [x] (2026-09-03 15:16Z) Verified every changed Rust file with rustfmt, ran the full default-member test suite, and ran clippy with warnings denied.
- [x] (2026-09-03 15:16Z) Prepared the completed implementation for a scoped commit on the current branch.

## Surprises & Discoveries

- Observation: The reported exit is not a panic. `run_workspace_dashboard` propagates `DaemonClient::delete_workspace` with `?`, so the ordinary `anyhow::Result` reaches `main` and terminates the command.
  Evidence: `mj-cli/src/main.rs` handles `SelectorOutcome::Delete` with `daemon.delete_workspace(workspace_id).await?`.

- Observation: Session data already lives in one SQLite database, but every terminal dashboard discards records outside its workspace and the web Resume page filters histories by `workspace_id`.
  Evidence: `retain_workspace_sessions` calls `session_ids_for_workspace`, daemon runtime snapshots use the same set, and `renderResumable` in `mj-controller/src/web/viewer.js` compares `session.workspace_id` to the selected workspace.

- Observation: `session_contexts.workspace_id` is protected by insert/update triggers but is not a foreign key, so deleting a workspace can safely preserve an inactive record's last workspace identifier without violating SQLite foreign-key integrity.
  Evidence: `ensure_workspace_schema` adds the column with `ALTER TABLE` and creates validation triggers; it does not rebuild `session_contexts` with a workspace foreign key.

- Observation: A full `cargo check --workspace` reaches the optional desktop crate and cannot complete on this host because its GTK/libsoup/JavaScriptCore system packages are absent. The repository's required default-member checks do not include that crate.
  Evidence: the workspace check failed in native dependency discovery, while `cargo check`, `cargo check --tests`, the full `cargo test`, and `cargo clippy --all-targets -- -D warnings` completed successfully for the default members.

- Observation: The repository baseline contains two Rust files that this host's rustfmt would reorder or rewrap even though they are outside this change.
  Evidence: `cargo fmt --all` touched `mj-controller/src/hel_controller.rs` and `mj-controller/src/hel_controller/worker_binary.rs`; those unrelated mechanical changes were removed and every task-owned Rust file then passed a direct `rustfmt --check`.

## Decision Log

- Decision: Treat `SessionState::is_active()` as the existing definition of a session that occupies a workspace.
  Rationale: The method already controls the terminal dashboard partition and deliberately keeps transitional states visible until lifecycle work is safe.
  Date/Author: 2026-09-03 / Codex

- Decision: Preserve an inactive record's `workspace_id` as its last workspace, even if that workspace is deleted, and globally project inactive records instead of migrating them to a synthetic workspace.
  Rationale: This preserves history without a schema migration, avoids exposing a fake workspace, and uses the existing database layout as a common history store.
  Date/Author: 2026-09-03 / Codex

- Decision: Resume claims the selected destination workspace before provisioning and keeps that destination if the attempt rolls back to an inactive state.
  Rationale: The durable record and all clients need one unambiguous destination throughout a lifecycle attempt; a stopped history remains globally discoverable either way.
  Date/Author: 2026-09-03 / Codex

- Decision: Apply global resume behavior to the terminal and web viewer.
  Rationale: The user selected parity across both control surfaces, and divergent ownership semantics would make the same durable session appear differently depending on client.
  Date/Author: 2026-09-03 / Codex

## Outcomes & Retrospective

Workspace deletion now distinguishes live ownership from durable history. Stopped, lost, and destroyed histories no longer make a workspace non-empty, and deleting their last workspace leaves their session record, checkpoint, materialized transcript, and prompt history intact. Active sessions, attached clients, destination claims from in-flight resumes, and detached drafts still refuse deletion.

The workspace selector catches create, rename, delete, and draft-recovery failures at its interaction boundary. It keeps the attempted workspace selected and renders the standard protected yellow failure notice instead of allowing an ordinary daemon error to terminate the CLI.

Terminal and web Resume views now receive inactive records from the common database regardless of their last workspace. Both clients name the currently selected workspace in the resume request; the daemon claims and durably rebinds that destination before provisioning. The daemon protocol advanced from 6 to 7, and the service-worker shell cache advanced to v2.

Focused tests passed for deletion/preservation, guarded rebinding, selector notices and selection retention, daemon record projection, web global history rendering, and web destination submission. The full `cargo test` run passed, including 721 core tests, 273 TUI tests, 90 worker tests, 120 CLI tests, controller tests, integration tests, and doc tests, with only the repository's declared ignored tests skipped. `cargo clippy --all-targets -- -D warnings` passed. No schema migration or new dependency was needed.

Manual interactive acceptance was not run; the state-machine, wire-shape, JavaScript, persistence, and selector presentation boundaries are covered automatically.

## Context and Orientation

The root `brokk-mj-core` crate owns SQLite persistence in `src/hel_database.rs` and schema creation in `src/hel_database/schema.rs`. A `SessionRecord` is the durable logical coding session. Its state decides whether it is active on a dashboard or inactive history in Resume. `session_contexts` stores the session's bundle, creation time, and workspace identifier; the session, checkpoints, materialized transcript, and prompt history are stored in related common tables.

`mj-cli/src/daemon.rs` implements the persistent daemon and its JSON request protocol. It filters runtime records for terminal dashboards and executes lifecycle operations. `mj-cli/src/dashboard.rs` loads a controller snapshot for one terminal workspace. `mj-cli/src/workspace_selector.rs` renders the separate terminal picker. `mj-tui/src/resume.rs` already builds a Resume list from every inactive record it receives, so the host must stop filtering those records out.

`mj-controller/src/hel_server.rs` defines the phone/web action protocol and public snapshot. `mj-controller/src/web/viewer.js` renders the web Resume page and sends actions. The route already identifies the selected workspace, but Resume does not yet send it.

The standard terminal notice mechanism is `mj_chat::hel_chat::Notices`. A protected failure notice sanitizes its text, renders in yellow in the footer, and cannot be dismissed by an incidental key before its minimum display interval.

## Plan of Work

First, update core database queries so `WorkspaceRecord::session_count` counts active session states, and change workspace deletion to count and refuse active session records plus detached drafts while preserving inactive rows. Add a writer-lane operation that validates an existing destination and changes `session_contexts.workspace_id` only for a session state that the controller can resume.

Second, change terminal controller filtering and daemon runtime record construction to include all inactive records alongside active records belonging to the selected workspace. Only active records receive workspace-specific read-frontier loading. Workspace snapshots shown by the selector should describe active membership rather than global history.

Third, preserve a shared `Notices` value and selected workspace identity across selector invocations. Catch failures from create, rename, delete, and draft recovery after the blocking/async operation has returned, report them with `set_failure`, refresh what can safely be refreshed, and re-enter the selector instead of returning the error from the top-level command. The selector footer uses the notice when present and dismisses it with the standard timestamp rule on input.

Fourth, add `workspace_id` to the daemon and web resume request shapes. The terminal supplies `DashboardContext.workspace_id`; the browser supplies the workspace named by its current route. The daemon validates and records the destination on the lifecycle before background work begins, rebinds the inactive session on the ordered database writer, reloads it, and then runs the existing resume controller. Workspace deletion must reject a workspace targeted by an in-flight lifecycle operation. Bump the daemon protocol because its serialized request changed, and bump the web service-worker cache because `viewer.js` changed.

Finally, add behavior tests at each boundary, run the full required validation, update this document with results, and commit only the files changed for this task.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`.

After implementation, run:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings

Every `cargo test` invocation must use elevated permissions outside the restricted sandbox because the suite exercises local sockets. The expected result is that every command exits zero and clippy emits no warnings.

Inspect the final worktree with:

    git status --short
    git diff --check
    git diff --stat

Then stage only the task-owned files and commit on the current branch. Do not push.

## Validation and Acceptance

A database test must create two workspaces, persist both active and inactive sessions, and show that deletion refuses the active workspace but succeeds after only inactive histories remain. The deleted workspace must disappear while loading the stopped `SessionRecord`, checkpoint metadata, materialized transcript, and prompt history still succeeds. A detached draft must independently block deletion.

A terminal behavior test must feed a rejected selector operation into the shared notice state and render the picker with Ratatui's test backend. The result must retain the same selected workspace, show the error in the footer, and return to input handling rather than propagate an error.

Dashboard and daemon tests must show that a workspace receives all inactive records plus only its own active records, and that a resume destination becomes the record's active workspace. Another workspace must stop seeing the record once it becomes active.

Web tests must show that Resume lists stopped records regardless of their last workspace, includes the currently routed `workspace_id` in its action, rejects an unknown destination, and places a successfully resumed record under the destination workspace.

Manual acceptance is: create workspaces A and B; stop several sessions in A; delete A from the terminal selector; observe that the selector remains usable and A disappears; enter B; open Resume; resume one of A's histories; and observe the session become active in B. Attempting to delete a workspace with an active session or draft must instead show a yellow warning and leave the process running.

## Idempotence and Recovery

All database changes use the existing ordered writer and immediate transactions. A rejected deletion changes nothing. Deleting a workspace never deletes inactive session rows. Repeating a resume rebind to the same destination is a no-op. If resume fails after claiming a destination, the ordinary resume rollback leaves the record inactive and globally visible with the attempted destination as its latest workspace.

The implementation does not require a schema migration. If validation fails, fix forward from the current worktree; do not reset unrelated user changes. The ExecPlan and source edits can be safely rerun through formatting and tests.

## Artifacts and Notes

The original observed message was:

    workspace is not empty (4 session histories, 0 drafts)

The replacement active-session refusal should name active sessions and drafts, while caller context should produce a selector notice such as:

    Could not delete workspace: workspace is not empty (1 active session, 0 drafts)

## Interfaces and Dependencies

In `src/hel_database.rs`, provide a workspace deletion function whose contract preserves inactive histories and a guarded `reassign_resumable_session_workspace(session_id, workspace_id)` writer operation. Keep filesystem-independent data handling and use the existing SQLite writer lane.

In `mj-cli/src/daemon.rs`, extend `ResumeSessionRequest` with `workspace_id: String`, carry the destination in active lifecycle metadata, and bump `PROTOCOL_VERSION` from 6 to 7.

In `mj-controller/src/hel_server.rs`, extend `ControllerAction::Resume` with `workspace_id: String`, validate it against `ViewerSnapshot.workspaces`, and pass it to the daemon runtime. `mj-controller/src/web/viewer.js` must send `selectedWorkspaceId()` and stop filtering resumable histories by their last workspace.

No new crate or third-party dependency is needed.

Revision note (2026-09-03): Updated the living sections after implementation and validation, recording the preservation model, UI behavior, protocol/cache bumps, host-only desktop build limitation, formatting baseline difference, and final test evidence.
