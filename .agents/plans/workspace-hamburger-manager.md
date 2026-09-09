# Replace F3 with a workspace hamburger and focused manager flows

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be updated as work proceeds. Maintain this document according to `.agents/PLANS.md`.

## Purpose / Big Picture

The terminal dashboard currently hides workspace management behind F3 and presents every operation in one crowded dialog. After this change, the right edge of the workspace-tabs pane contains a visible `☰` button. Mouse users can click it and keyboard users can focus and activate it to open a manager whose list, name editing, deletion confirmation, and draft recovery are distinct views. F3 no longer opens or advertises workspace management.

The existing controller and daemon operations remain unchanged. They already load workspace data and perform create, rename, delete, and draft-recovery work in supervised background tasks. This change reorganizes the terminal state and rendering around those operations.

## Progress

- [x] (2026-09-09 03:45Z) Inspected workspace rendering, input routing, command metadata, background operation handling, documentation, and PTY coverage.
- [x] (2026-09-09 03:45Z) Agreed on a pinned hamburger, removal of F3, and a list-and-action-bar manager with separate subviews.
- [x] (2026-09-09 04:53Z) Implemented pinned hamburger geometry, local keyboard focus, safe mouse press/release activation, and tab-width reservation.
- [x] (2026-09-09 04:53Z) Replaced the combined manager with list, create, rename, delete-confirmation, and drafts views backed by the existing asynchronous actions.
- [x] (2026-09-09 04:53Z) Removed the F3 binding and hints, updated both workspace and terminal-surface documentation, and revised PTY/tmux coverage.
- [x] (2026-09-09 04:53Z) Ran formatting, focused and broad Rust tests, clippy, Python syntax checks, and the mouse-driven workspace scenario.
- [x] (2026-09-09 04:53Z) Reviewed the integrated diff and prepared only task-owned files for the required current-branch commit and upstream push.

## Surprises & Discoveries

- Observation: Workspace management already runs through `DashboardAction` and `mj-cli/src/dashboard/io.rs`, with generation checks that reject stale results.
  Evidence: `begin_workspace_manager`, `finish_workspace_management`, and the four `spawn_workspace_*` helpers already separate UI state from filesystem, process, and daemon work.

- Observation: The workspace pane currently records tab and whole-pane rectangles outside the shared surface-control form.
  Evidence: `mj-tui/src/workspaces.rs::workspace_tab_click` performs direct hit testing, while `mj-tui/src/surface_controls.rs` owns button press/release semantics for other dashboard controls.

- Observation: Removing the F3 footer item changes which lower-priority hints fit at narrow widths even though their definitions did not change.
  Evidence: the 200-column footer now also fits `r restart`, and the 32-column footer fits `F4 web`; snapshot expectations were updated to the renderer's existing priority behavior.

- Observation: The real-terminal mouse scenario can see the manager frame before its asynchronous workspace snapshot arrives, while mutation buttons are intentionally disabled.
  Evidence: the first run opened the hamburger menu but clicked Rename during loading. Waiting for the rendered `Current` marker before interacting made the complete rename/create/delete scenario pass.

- Observation: Five worker relay tests timed out only when the original full suite ran with parallel test threads.
  Evidence: `cargo test -p brokk-mj-worker --lib -- --test-threads=1` passed all 109 enabled tests, and the later serial workspace run reached and passed the worker test set. The final affected CLI binary rerun passed all 205 tests after removing its stale F3 expectation.

## Decision Log

- Decision: Pin a three-cell ` ☰ ` control to the right edge of the workspace pane and reserve its width before laying out tabs.
  Rationale: The control remains discoverable and does not scroll away when workspace names overflow.
  Date/Author: 2026-09-09 / Codex and user.

- Decision: Remove F3 from the command registry, help, footer hints, chat hints, docs, and tests while keeping the Workspaces command in the command palette and first-run actions.
  Rationale: The requested hamburger replaces the function-key entry point, while the command palette continues to provide keyboard access from any focus.
  Date/Author: 2026-09-09 / Codex and user.

- Decision: Represent manager screens explicitly as list, create, rename, delete-confirmation, and drafts views.
  Rationale: A single shared name field and six-button row obscure which action the field belongs to and put destructive work beside routine actions.
  Date/Author: 2026-09-09 / Codex and user.

- Decision: Use the workspace pane's existing `Focus::Workspaces` plus a local boolean or enum for whether tabs or the hamburger owns keyboard focus, rather than adding a new top-level `Focus` variant.
  Rationale: Workspace switching and the menu are parts of one pane; the persisted workspace view should continue to store pane-level focus without persisting a transient button selection.
  Date/Author: 2026-09-09 / Codex.

- Decision: Make reverse Tab from Sessions land on the hamburger, then on the workspace tabs, mirroring the forward order from tabs to hamburger to Sessions.
  Rationale: Treating the pinned control as a real keyboard stop makes traversal reversible and predictable without changing the dashboard's persisted pane focus.
  Date/Author: 2026-09-09 / Codex.

- Decision: Hide the Drafts action when the selected workspace has no detached drafts.
  Rationale: The manager should expose recovery only when it has something recoverable, keeping the primary action row focused on available work.
  Date/Author: 2026-09-09 / Codex.

## Outcomes & Retrospective

The workspace pane now reserves its rightmost three content cells for a visible ` ☰ ` control. It participates in the same mouse gesture engine as other buttons, and keyboard traversal proceeds workspace tabs → hamburger → Sessions, with the reverse order under Shift-Tab. Workspace tabs remain independently clickable and keep the active Unicode label visible without entering the reserved cells.

The manager is now a workspace list plus concise actions, with isolated create and rename inputs, an always-separate delete confirmation, exact-name protection for destructive deletion, and a draft-only recovery view. Background operations still use the controller's existing supervised tasks and generation guard; loading and busy states disable duplicate submission, stale results are ignored, and refreshes preserve view identity by workspace ID rather than row index.

Validation completed with `cargo fmt --all -- --check`, focused workspace and PTY tests, all 407 enabled TUI tests (two ignored), all 205 CLI binary tests, the serial 109-test worker rerun, `cargo clippy --all-targets -- -D warnings`, Python syntax checks, and `python3 tests/e2e/tui_mouse_commands.py`. The initial parallel full-suite run exposed five contention-only worker timeouts; each passed in the serial worker rerun. A later serial full-suite pass reached the final CLI binary with one stale F3 assertion, which was corrected and followed by a clean 205-test binary rerun. No product limitation remains from those test-only issues.

## Context and Orientation

`mj-tui/src/workspaces.rs` owns workspace tabs, workspace-manager state, input handling, and rendering. `mj-tui/src/lib.rs` stores the dashboard's focus and hitbox state and routes keyboard and mouse events. `mj-tui/src/surface_controls.rs` implements dashboard buttons with the shared `Form` interaction engine, which tracks press and release so a drag does not accidentally activate a control. `mj-tui/src/actions.rs` contains the Workspaces command used by the command palette and currently assigns F3 as its global chord.

`mj-cli/src/dashboard/io.rs` launches workspace operations asynchronously and installs their results. Creating a workspace closes the manager and selects the new workspace; other operations refresh the manager. Keep this concurrency and generation behavior intact. `mj-cli/tests/termination_pty.rs` proves the manager stays inside the existing alternate terminal screen and remains responsive.

The terminal renderer requires at least 80 columns. The workspace tabs pane is three rows high, with one inner content row. A control's visible rectangle must be rebuilt every frame so mouse routing always matches what was rendered.

## Plan of Work

First, extend the workspace pane's transient state so it knows whether keyboard focus is on the tabs or hamburger and where the hamburger was drawn. Render ` ☰ ` as a registered shared surface button at the final three inner cells. Give the remaining inner width to the existing tab clipping algorithm and continue keeping the active tab visible. A click on a tab focuses tabs and selects it. A click on the hamburger runs `CommandId::Workspaces`. When `Focus::Workspaces` owns the keyboard, Left and Right switch workspaces while tabs are selected; Tab or Right at the appropriate boundary can move to the hamburger, Enter or Space opens it, and normal forward and reverse Tab traversal remains deterministic between Workspaces and Sessions. Clear stale hamburger geometry whenever the pane is absent.

Next, replace `WorkspaceManager::confirming_delete` with a view enum. The list view displays one row per workspace with its name, a Current marker for the active ID, active-session count, and draft count. It has New workspace above or alongside the list and an action row containing Open, Rename, Delete, Drafts when applicable, and Close. It initializes selection from the active workspace, scrolls through a constrained-height choice list, disables selected-workspace actions for an empty list, and leaves loading or busy work visible without allowing duplicate submission.

The create view owns an empty 64-character text input plus Create and Cancel buttons. The rename view snapshots the selected workspace identity and begins with its existing name plus Save and Cancel. This identity must remain stable even if a background refresh reorders entries. Submitting an empty trimmed name leaves the view and input intact with an error. Server validation errors do the same.

The delete view snapshots the workspace identity, name, session count, and draft count. Every delete is confirmed in this view. An empty workspace may be deleted using a plainly labeled Delete button. A workspace containing sessions or drafts explains that sessions will be destroyed and drafts discarded, shows both counts, requires the exact workspace name, and enables only a `Force delete` button after the input matches. Cancel returns to the list. No destructive button receives initial focus.

The drafts view belongs to the selected workspace and lists source, saved time, and optional session ID. It provides Recover and Back. Recover uses the selected draft ID through the existing background action. A successful recovery refreshes the list; if the recovered item disappears, clamp selection safely and show a short success indication. Escape returns from any subview to the list, preserving the loaded workspace snapshot and selection; Escape from the list closes the manager.

Then remove F3 from `CommandId::Workspaces.keys` and `GLOBAL_CHORDS` while retaining the command specification itself so the palette, onboarding Workspaces button, and hamburger dispatch the same action. Remove stale F3 language from module comments, help keys, combined footer text, embedded chat hints, docs, and unit tests. Change the PTY test to open the command palette, choose Workspaces, and verify the same in-place terminal behavior; the new lower-level tests cover direct hamburger activation.

Finally, review the complete diff for event-loop safety and state consistency. Update this plan's living sections, format the changed Rust files, run validation outside the sandbox as required, commit only the files from this task, and push the current branch to upstream.

## Concrete Steps

Work from `/home/jonathan/Projects/hel4`.

Inspect and edit the files described above, then format Rust code:

    cargo fmt --all -- --check

If the check reports formatting changes are needed, run `cargo fmt --all` and inspect the resulting diff. Run focused tests while iterating, followed by the required repository checks outside the restricted sandbox:

    cargo test
    cargo clippy --all-targets -- -D warnings

Review the final work and record the commit:

    git diff --check
    git status --short
    git diff --stat
    git add <only files changed for this task>
    git commit -m "Redesign workspace management menu"
    git push

## Validation and Acceptance

At 80 columns and at a wider terminal, the workspace pane must show the hamburger at its right edge without overlapping the active tab. Long and Unicode workspace names must clip on valid display-cell boundaries. Clicking the hamburger must open the manager; clicking a tab must still focus and select that workspace. Keyboard focus must reach the hamburger, visually identify it, activate it with Enter or Space, and return cleanly through forward and reverse traversal.

F3 must do nothing workspace-specific and must not appear in the command registry, footer, help, embedded chat hint strings, published workspace documentation, or PTY comments. Workspaces must remain available from the command palette and first-run Workspaces button.

The list manager must open on the active workspace, switch on Enter or Open, create a workspace with an isolated empty form, rename through an isolated prefilled form, confirm every deletion, require exact-name confirmation when sessions or drafts make deletion destructive, browse and recover drafts in their own view, and handle empty lists and short terminals without panic. Errors must stay visible in the view where they occurred, background work must disable repeat submission, and stale results must not mutate a closed or newer manager.

`cargo test` and `cargo clippy --all-targets -- -D warnings` must exit successfully. The PTY manager test must prove opening management does not leave and reopen the alternate terminal screen and that termination stays responsive while the manager owns input.

## Idempotence and Recovery

Formatting and tests are safe to rerun. Workspace operations in tests must use test-owned storage and must not touch the user's database. If an edit or test exposes a design mismatch, keep the existing asynchronous actions and generation guards, revise this plan's Decision Log, and change only the TUI state around them. Do not reset or discard unrelated worktree changes. If the upstream advances before push, fetch and integrate according to the repository's current-branch rule without creating or changing branches.

## Artifacts and Notes

The intended workspace pane is conceptually:

    ┌ Workspaces ───────────────────────────────┐
    │ active tab   another tab              ☰ │
    └───────────────────────────────────────────┘

The intended manager list view is conceptually:

    ┌ Workspaces ────────────────────────────────────────┐
    │                                    [New workspace] │
    │ › Project alpha   Current   3 sessions   2 drafts  │
    │   Project beta              1 session              │
    │ [Open] [Rename] [Delete] [Drafts (2)] [Close]      │
    │ ↑↓ Select · Enter Open · Esc Close                 │
    └─────────────────────────────────────────────────────┘

These sketches specify grouping and priority, not exact box width. The renderer must adapt to the available terminal area using existing theme and component primitives.

## Interfaces and Dependencies

Do not add a crate or third-party dependency. Keep the public `DashboardAction::{LoadWorkspaceManagement, CreateWorkspace, RenameWorkspace, DeleteWorkspace, RecoverWorkspaceDraft}` variants and `WorkspaceManagementEntry` data shape intact. Add private TUI-only enums or fields for workspace-pane control focus and manager view state. Route the hamburger through `DashboardState::dispatch_command(CommandId::Workspaces)` or `run_available_command` so all visible entry points use the same command behavior.

Revision note: Initial plan written after fast-forwarding to upstream commit `4106f8dc`; it incorporates the user's choice to remove F3 and replace the existing combined manager with explicit action views. Updated after implementation and validation to record the reversible local focus order, conditional Drafts action, asynchronous E2E readiness condition, and test outcomes.
