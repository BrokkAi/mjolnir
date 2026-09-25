# Standardize safe workspace deletion and visibility

This ExecPlan is a living document maintained under `.agents/PLANS.md`.

## Purpose / Big Picture

The terminal currently offers different Close, Delete, and Force delete actions for workspaces. A user should see one ordinary Delete action that suspends work safely before removing the workspace. The terminal and browser should show only workspaces published by the daemon, while stopped histories remain available for resumption.

## Progress

- [x] (2026-09-24) Inspected the daemon deletion paths, workspace feed, terminal manager, and browser snapshot.
- [x] (2026-09-25) Unify and simplify workspace removal.
- [x] (2026-09-25) Make the published workspace feed authoritative in both projections.
- [x] (2026-09-25) Update behavior tests and validate the full change. `cargo test` and `cargo clippy --all-targets -- -D warnings` pass. Browser and e2e tests were not run.
- [x] (2026-09-25) Commit the validated result on the current branch.

## Surprises & Discoveries

- The daemon's Close action already suspends independent sessions and sub-agents, supports cancellation, and uses a final transactional guard.
- The browser adds tabs from session workspace IDs when its published list omits them. This restores tabs for deleted workspaces with stopped history.

## Decision Log

- Decision: Keep Close and Delete protocol actions as compatible entry points to the same safe operation, but expose one Delete control in the manager and retain the Close shortcut as an alias. Rationale: users need one ordinary meaning for removing a workspace. Date/Author: 2026-09-24 / Codex.
- Decision: Remove workspace Force delete across the UI, client, daemon, and database. Rationale: destroying active sessions as part of workspace removal is hazardous and was explicitly rejected by the user. Date/Author: 2026-09-24 / Codex.

## Outcomes & Retrospective

Implemented as planned. Open question: `selectedWorkspaceId()` in `mj-controller/src/web/viewer.js` can still fall back to a session's workspace id, which may select a deleted workspace for a sub-agent route.

## Context and Orientation

`mj-controller/src/daemon/close_workspace.rs` owns safe suspension and removal. `mj-controller/src/daemon/actions.rs` maps client requests to operations. `mj-controller/src/daemon/state.rs` publishes a watch feed of workspace records. `mj-tui/src/workspaces.rs` handles the manager state and confirmation; `mj-controller/src/server_runtime/snapshot.rs` projects the browser snapshot.

## Plan of Work

Route the old Delete daemon action through Close, remove Force delete and its implementation, and make the manager's Delete action open the existing suspension confirmation. Remove the separate Close button while retaining its shortcut. Serialize workspace refresh, make API creation use that refresh, and read the watch feed for both runtime and web snapshots. Remove the session-derived browser tab fallback.

## Concrete Steps

Edit the files above and their focused tests. From the repository root run relevant browser tests, `cargo test` outside the restricted sandbox, and `cargo clippy --all-targets -- -D warnings` on the dev profile. Review `git diff --check` and commit only changed files on the current branch.

## Validation and Acceptance

Deletion must confirm before suspending, remain cancellable, preserve stopped histories, and never remove a workspace with active sessions after a suspension failure or race. Both UIs must present the same workspace IDs and names after create, rename, delete, reconnect, and restart. A stopped history that references a deleted workspace must not create a tab.

## Idempotence and Recovery

The database's existing transaction and resume admission guard prevent deletion of an occupied workspace. On failure the user can retry the same safe operation; completed suspensions are not reversed.

## Artifacts and Notes

The worktree was clean at the start of this implementation. The current branch is `hel3`.

## Interfaces and Dependencies

Keep the existing `DaemonAction::CloseWorkspace` and `DaemonAction::DeleteWorkspace` request shapes. Remove `DaemonAction::ForceDeleteWorkspace` and advance the daemon protocol version. No schema migration, new crate, or package dependency is required.

Revision note (2026-09-24): Created this plan for implementation of the user's revised deletion and workspace visibility intent.
