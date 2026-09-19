# Add pane and workspace shortcuts for #1093

This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective throughout implementation.

## Purpose / Big Picture

Users can resize conversation splits with prefix+r followed by h/j/k/l or arrows until Escape, swap neighboring conversations with prefix+Shift+h/j/k/l, close a session with prefix+Shift+x, rename their workspace with prefix+Shift+w, and close their workspace with prefix+Shift+d. Both close commands always ask for confirmation. Closing stops sessions and retains resumable histories. Closing a workspace discards its drafts and removes the workspace, rather than leaving running sessions without a workspace.

## Progress

- [x] (2026-09-19) Read issue, inspected relevant implementation, and agreed on semantics with user.
- [x] (2026-09-19) Implement pane commands, input routing, rendering, and behavior tests.
- [x] (2026-09-19) Implement confirmed session close and direct workspace rename/close UI.
- [x] (2026-09-19) Implement supervised workspace close with concurrency protection and recovery tests.
- [x] (2026-09-19) Full dev-profile suite passed on the combined upstream implementation (4,104 passed, expected ignores retained).
- [x] (2026-09-19) Final TUI/CLI navigation tests passed (672 TUI and 142 CLI unit tests plus integration tests); formatting and clippy passed.
- [x] (2026-09-19) Committed implementation as 981532a2 on hel4 and pushed it to origin/master after integrating 46a98949.

## Surprises & Discoveries

Existing directional resize actions already work through the palette. Existing workspace deletion destroys active sessions; it must not implement the new Close action. The database's final force-delete transaction preserves stopped histories. Its implementation is shared with a new close-workspace finalizer that also clears legacy unsent input, without changing destructive deletion semantics. Detached drafts have workspace foreign keys, and per-client composer state cascades on workspace removal.

## Decision Log

User chose normal stopping with resumable histories for both close commands, followed by explicitly discarding workspace drafts ("drop them on the floor"). No recovery workspace or unworkspaced-session model is required. These decisions were made on 2026-09-19.

Resize and swap apply to conversation splits, not the fixed support panels. Swapping moves pane identities so focus and conversation-local state travel together. Existing stop/delete actions retain their behavior. New commands remain configurable and discoverable through the palette and help. Deferred edit-scrollback, copy-mode, rename-pane, and new-worktree keys remain unbound without promises of future reservation.

## Outcomes & Retrospective

All requested commands and lifecycle behavior are implemented and validated. The full suite passed after upstream integration; targeted TUI/CLI tests passed after the final navigation adjustment. Formatting and clippy are clean. Implementation commit 981532a2 is published on origin/master. All requested behavior is complete; no schema migration or live-store changes were required.

## Context and Orientation

`mj-core/src/config/keys.rs` defines configurable bindings; `mj-tui/src/actions.rs` registers commands and dispatches UI actions. `mj-tui/src/keybinds.rs` routes prefix sequences before composer input. `mj-tui/src/tile_layout.rs` owns the binary split tree, whose leaves are stable pane identities; `mj-tui/src/dashboard_conversation.rs` connects it to sessions and persistence. The footer and help render registered commands.

`mj-tui/src/workspaces.rs` contains the asynchronous workspace manager and rename/delete forms. `mj-cli/src/dashboard/actions.rs` and `io/spawn.rs` run background work and `io.rs` applies results with generation guards. `mj-client/src/daemon.rs` defines requests to the daemon (the background session-owning process). `mj-controller/src/daemon/close.rs` supervises ordinary stopping; `resume.rs` currently implements destructive workspace deletion. `mj-controller/src/database/workspaces.rs` removes empty workspaces transactionally while preserving stopped session records.

## Plan of Work

### Milestone 1: pane controls

Register resize mode and directional swap bindings. Add stable-leaf swapping using the same neighbor choice as focus navigation. Preserve ratios, focus identity, attached sessions, and drafts; report layout changes through the existing persistence path. Reveal zoomed splits for both operations. One-pane or missing-neighbor operations do not change layout. Resize mode accepts repeated h/j/k/l and arrows, consumes unrelated text and paste, exits on Escape or configured command dispatch, and displays a persistent footer hint. Workspace changes and modal entry end the mode. Colocated tests exercise routing, nested swaps, focus, and saved-layout round trips.

### Milestone 2: lifecycle commands and UI

Add a CloseSession command distinct from StopSession, always opening a cancel-default confirmation and then using the existing Close action. Capture the selected session identity and mention active children. Add direct workspace rename and close entry points using the manager's asynchronous load and generation guard. Add a Close manager action without changing Delete. Its confirmation shows the workspace name, sessions, and drafts and explains preserved histories and discarded drafts.

### Milestone 3: safe workspace closing

Add CloseWorkspace to the client/daemon protocol and supervised runtime implementation. Reject active resume ownership, stop independent session groups concurrently through ordinary close (children before parents), report all failures, and retain workspace/drafts on failure. Retry only remaining active sessions. Protect final removal from concurrent creation/resume and recheck active state in the transaction. Reuse final database removal only after stops finish; never invoke destructive runtime force-delete. Keep background operations supervised, errors visible, and quitting responsive. Continue working/Escape dismisses progress without cancelling; reopening the close command restores its in-flight view and cancellation control. Completion updates a reopened progress dialog using its current generation, while failures after dismissal become notices. Cancellation prevents further work where possible without promising rollback of stopped sessions. Refresh workspace state and reuse existing selected-workspace fallback and cache cleanup. No schema migration or new crate is planned. The new daemon requests require protocol revision 27; frozen management requests are unchanged.

## Concrete Steps

Work in `/home/jonathan/Projects/hel4`, on the current branch. Read local guidance and preserve unrelated changes. Implement each milestone and its focused tests, then run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. All cargo test invocations must run outside the restricted sandbox; use normal dev-profile build storage, never /tmp. Expected result is no failed tests and no clippy warnings. Stage only changed files explicitly and commit validated coherent changes; the user additionally authorized merging and pushing to origin/master on completion.

## Validation and Acceptance

Behavior tests must prove resize input never leaks into the composer, Escape/configured commands leave the mode, and the mode hint is visible. Nested swaps preserve focus/session association and ratios across saved-layout restoration. Test custom bindings and single-pane/no-neighbor behavior. Verify session close always confirms, cancellation does nothing, and confirmed closure uses normal stopping with child handling. Workspace tests cover direct rename, stale replies, close confirmation, asynchronous busy state, draft disposal, preserved resumable histories, failure/retry, concurrent ownership, and last-workspace removal. Database and runtime tests use isolated stores and never mutate the live store.

For a manual isolated dashboard check, open two conversation splits, enter prefix+r and resize with h/l, Escape, then prefix+Shift+h/l and observe the focused conversation move. Cancel both close confirmations once before accepting them against disposable sessions; Resume must retain stopped histories, and closing the workspace must remove its entry and drafts.

## Idempotence and Recovery

Stopping is not rolled back. Workspace close failures retain the workspace and drafts, name failures, and allow retry of remaining active sessions. A concurrent new session or resume must block final deletion. No data migration is authorized or required. Repeated test runs use isolated data. Closing the final workspace uses the existing empty-dashboard state.

## Artifacts and Notes

The full `env -u NO_COLOR cargo test --quiet` suite passed after incorporating origin/master at 46a98949. The concurrent-stop test uses a barrier to require two independent closes to run together, registers a new session while they stop, verifies final deletion refuses without discarding drafts, and retries successfully. Database tests verify checkpoint/history preservation, unsent-text disposal, and rejection of post-deletion creation. Runtime tests verify per-workspace resume exclusion without blocking unrelated workspaces.

## Interfaces and Dependencies

Add configurable actions for resize mode, four swaps, close session, rename workspace, and close workspace, mapping to the command registry. Add a `CloseWorkspace { workspace_id: String }` daemon request and `DaemonClient::close_workspace` method. Reuse existing Tokio supervision, lifecycle helpers, layout serialization, and database transaction support; add no external dependencies.

Initial plan recorded 2026-09-19 from the accepted conversational plan, including the user's later decision to discard workspace drafts.

2026-09-19 update: user authorized merge and push to origin/master. Integrated five upstream commits by fast-forwarding hel4 to 46a98949 with autostash, then resolved additive native-agent conflicts. CloseSession must refuse parent-owned native agents just like StopSession. Full tests initially exposed NO_COLOR=1 in the tool environment; run validation with `env -u NO_COLOR` so existing color tests exercise their expected rendering. Adapted viewport-dependent help/palette tests to the larger command list.

2026-09-19 review update: shared transactional finalization now clears unsent legacy input for normal workspace close; client caches are discarded when a workspace is removed so tab switching cannot resurrect its drafts. Added a per-workspace resume admission gate to protect the pre-registration interval and barrier-based concurrent-stop/retry tests. A final navigation adjustment allows dismissing and reopening the progress dialog without interrupting daemon-owned stops.

2026-09-19 validation: `env -u NO_COLOR cargo test --quiet` passed; after the navigation adjustment, `env -u NO_COLOR cargo test -p brokk-mj-tui -p brokk-mjolnir --quiet` passed. `cargo fmt --all -- --check`, `git diff HEAD --check`, and `cargo clippy --all-targets -- -D warnings` passed on the final source tree. Tests used isolated stores; no live-store migration was run.

2026-09-19 completion: committed `981532a2` (Add pane controls and confirmed session/workspace closing), including `Fixes #1093`, and successfully pushed `46a98949..981532a2` to `origin/master`. The branch remained hel4; integration was a fast-forward from the fetched master before the implementation commit. The final documentation-only checkpoint records this publication evidence.
