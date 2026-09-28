# Unify conversation selection

This ExecPlan is maintained under `.agents/PLANS.md`.

## Purpose / Big Picture

The Sessions marker, active conversation, and ordinary session commands must identify the same session. Selecting a row activates its existing pinned pane or assigns it to Browse. Clicking a conversation selects it everywhere. Keyboard focus remains in the list during list navigation until Enter enters the composer. A filtered-out active session stays visible with an Outside filter label.

## Progress

- [x] (2026-09-28) Trace independent sidebar selection, pane assignments, startup restoration, filtering, and asynchronous attachments; agree on unified selection and retaining the active filtered row.
- [x] (2026-09-28) Replace independent selection with a private navigation owner and migrate transitions.
- [x] (2026-09-28) Make asynchronous attachment results validate assignment identity without changing navigation.
- [x] (2026-09-28) Add behavioral regressions, update documentation, and validate full workspace tests, Clippy and isolated real-terminal behavior. Final focused checks also pass after filter-count cleanup.
- [x] (2026-09-28) Complete final format and diff checks and commit validated changes on the current branch.

## Surprises & Discoveries

The sidebar reads `selected_session_id`, while chat rendering and prompt input read `pane_sessions[conversation_layout.focused()]`. Startup without a restored layout opens a session without selecting its row. `clamp_selections` independently picks another row after refresh. The existing attachment generation protects retries, but needs an assignment identity to reject A-to-B-to-A changes before another attachment starts.

## Decision Log

2026-09-28: The user chose unified selection rather than additional cues for independent list selection and composer focus. Pane assignments and active pane therefore own selection; no secondary selected-session field will be synchronized with them.

2026-09-28: The user chose to retain the active row outside a filter. Filtering or background status changes must not switch conversations. Empty panes have no selection; list navigation chooses a row explicitly.

## Outcomes & Retrospective

The independent sidebar selection and pending-browse queue are removed. Selected/current session and ordinary command targeting derive from the active pane. Background results validate the current assignment and cannot change navigation. Full workspace tests pass; the final focused run passes 851 TUI tests and 243 CLI tests (two TUI tests remain intentionally ignored). Clippy passes. Real tmux interaction verifies fresh startup, selecting an existing pin by clicking its conversation, filtering, and saved-layout restart. No schema, wire, or terminal-handoff format changed.

## Context and Orientation

`mj-tui/src/lib.rs` defines DashboardState. Its dashboard modules reduce input into state and actions without external I/O. `mj-tui/src/dashboard_conversation.rs` manages tiles and persistence; `dashboard_sessions.rs` orders and filters rows. `mj-cli/src/dashboard/session_state.rs` starts and follows conversation attachments, `drafts.rs` prepares them, and `io.rs` accepts background results. Browse is the unpinned conversation pane replaced by selecting an unpinned session; other panes retain pinned sessions.

## Plan of Work

First add a private navigation owner containing the tile layout, pane sessions, and transient assignment identities. Derive selected/current session from its active pane, migrate every field write to transitions, and remove pending browse and copied workspace selection. Keep persisted ConversationLayout unchanged. Restore selection from its active pane, and preserve the existing fresh-start choice policy.

Next make CLI attachment reconciliation follow assignments already made by the TUI. Assignments change synchronously, before any background result can arrive. Each result carries its assignment identity as well as the attachment attempt generation; completion only fills content. Retire outgoing chats with their draft, question and scroll state intact. Failed opens stay failed until explicit retry.

Finally update rendering and tests for the agreed navigation, filtering, loading and empty-pane behavior. Document the user-visible change in `docs/src/content/docs/terminal-surface.mdx`.

## Milestones

The navigation milestone removes two writable answers to selection, proved by TUI tests for clicking, list input, splits, restore, filters and session removal. The attachment milestone proves delayed responses cannot change selection or supply stale content, including A-to-B-to-A and workspace restoration. The delivery milestone completes full dev-profile validation and an isolated named-instance smoke check, then commits on the current branch without pushing.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir2`. Use the existing mbx Cargo configuration without changing target storage. Run focused package tests while iterating, then `cargo test` and `cargo clippy --all-targets -- -D warnings`. Every cargo test runs outside the restricted sandbox. Use `--instance selection-consistency` for manual new-build invocations and preserve existing isolated test directories.

## Validation and Acceptance

Assert selection and current conversation agree after every relevant input transition, before background attachment completion. Render distinctive session names/content alongside the marker to verify the advertised interface. Cover fresh startup and saved layouts, workspace/upgrade restoration, pinned navigation, keyboard focus retained in the list, filters that exclude the selected row, status changes, empty panes, session removal and subagents. Use controllable attachment completion to cover delay, cancellation, failure, supersession and A-to-B-to-A. Verify outgoing composer/question drafts and scroll positions survive. Full Cargo tests and clippy must pass before committing.

## Idempotence and Recovery

No database migration, wire change, or upgrade-handoff format change is needed. Tests must not use live session data. Preserve unrelated work, stage only this task's files, and commit on the existing branch. Stop any smoke-test processes before removing their working files.

## Artifacts and Notes

Validation output and discoveries will be recorded here as work proceeds.

## Interfaces and Dependencies

Add `ConversationNavigation` in `mj-tui/src/conversation_navigation.rs` with private layout, session assignments and assignment identities. Expose read-only layout/session access and controlled assign/focus/split/close/restore methods. Dashboard selected/current accessors both derive from its active pane. Expose an opaque `PaneAssignment` token to the CLI for asynchronous completion validation. Use existing attachment attempt generations for retries, existing supervised tasks for I/O and existing layout serialization for persistence. Do not add a crate or dependency.

Revision 2026-09-28: Lifecycle completion now releases the assigned pane and selection together. Test fixtures that represent already-open conversations assign them without falsely marking restored layouts as locally edited. The regression script `tests/e2e/tui_selection.py` uses the existing fake ACP lab with `--instance selection-consistency` and tests fresh startup, row clicks, split conversation clicks, filter exceptions, and restored startup.

Validation evidence (2026-09-28): `cargo test -- --quiet` passed across all default workspace packages and integration tests, including isolated daemon upgrades and PTY termination. `cargo clippy --all-targets -- -D warnings` passed. `cargo test -p brokk-mj-tui -p brokk-mjolnir --lib --bins -- --quiet` passed after the final cleanup (851 + 243 tests). `python tests/e2e/tui_selection.py` passed using the dev CLI/worker and named instance; evidence is under `target/reliability-artifacts/unified-selection-seed-1-638721/`, including fresh-start, conversation-click, filtered-active and restored-start captures. SQLite integrity is `ok`, with no foreign-key errors. Normal mbx target storage was retained throughout.
