# Keep agent questions inside their chat pane

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` are maintained throughout the work in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Agent elicitation forms currently behave as a dashboard-wide modal. They can hide the navigator and other panes, steal keyboard and mouse routing from the rest of the application, and lose partially entered answers when the user attaches a different session. After this change, an agent question or plan review occupies only the active session's transcript and composer area. The navigator, pane headers, pane sizing controls, and other sessions remain usable. A user can click a question, tab through its fields and buttons, use arrows/Space/Enter, scroll long plan text, and use F6/Shift-F6 to move between panes without losing drafts.

The behavior is visible by opening two sessions with pending questions: each session retains its own answers and reading position while the user switches panes or resizes the dashboard. Small terminals keep the focused form control visible, while plan text and option lists use their available space and scroll independently.

## Progress

- [x] (2026-09-04) Read `AGENTS.md` and `.agents/PLANS.md`; identified the existing elicitation, chat, TUI, and dashboard routing paths.
- [x] (2026-09-04) Created this self-contained implementation plan and recorded initial decisions.
- [x] (2026-09-04) Added session-scoped in-memory draft state, complete request matching, reviewer source identity, and deferred reviewer restoration.
- [x] (2026-09-04) Rendered questions in session content bounds with explicit surfaces, focus styles, stable logical scroll state, and small-area control budgeting.
- [x] (2026-09-04) Routed mouse and keyboard input by pane/question focus while preserving existing modal/config behavior and added F6/Shift-F6 focus cycling.
- [x] (2026-09-04) Preserved drafts over asynchronous session attachment and rejected stale late results using request/source checks.
- [x] (2026-09-04) Added focused renderer coverage for scoped clearing, compact six-row layouts, wrapped custom input visibility, exact wrap-boundary resizes, indented paragraphs, stable logical anchors, and footer shortcut hints.
- [x] (2026-09-04) Added full-projection pending question data to `SessionDetail`, per-session “Needs input” indicators, minimized navigator aggregation, and shortcut discoverability tests.
- [x] (2026-09-04) Reconciled cached drafts against fresh full projections, preserved deferred reviewer snapshots, filtered removed reviewer roles, and cleared reviewer forms when the daemon closes a review.
- [x] (2026-09-04) Ran elevated focused validation: 318 `brokk-mj-chat` tests, 284 `brokk-mj-tui` tests (2 ignored), and the dashboard question-click and forward/reverse F6 tests in `brokk-mjolnir` all passed.
- [x] (2026-09-04) Root reviewed the implementation, added independent behavior regressions, and passed `cargo test` and `cargo clippy --all-targets -- -D warnings`. Final focused checks covered the last reviewer lifecycle edits.

## Surprises & Discoveries

- Observation: `ElicitationDialog` already owns field values, option cursors, custom-answer activation, and message scrolling, but its renderer uses `frame.area()` and a centered 82%/78% rectangle.
  Evidence: `mj-chat/src/hel_chat/elicitation.rs::render_elicitation`.
- Observation: the active chat keeps only one `ElicitationDialog`, and `sync_elicitation` currently compares only the request ID when deciding whether to rebuild it.
  Evidence: `mj-chat/src/hel_chat.rs::sync_elicitation` and `ElicitationDialog::id`.
- Observation: `mj-tui/src/combined.rs` receives the dashboard-wide frame and `mj-cli/src/dashboard.rs` uses `prompt_has_focus` and selection replay to route events, so the session-area bounds and question focus need to be explicit across crate boundaries.
  Evidence: `mj-tui/src/combined.rs`, `mj-tui/src/lib.rs`, and `mj-cli/src/dashboard.rs`.
- Observation: the repository has unrelated working-tree changes in `.agents/docs/claude-autonomous-turns.md` and `src/hel_worker.rs`; implementation must leave them untouched.
  Evidence: `git status --short`.
- Observation: `MaterializedSession` already carries complete pending elicitation requests, while the lightweight stored summary does not.
  Evidence: `src/hel_state.rs::MaterializedSession::pending_elicitations` and `MaterializedSessionSummary`.
- Observation: minimized Sessions has no spare status row, so its title is the stable location for the aggregate pending-question count; expanded and collapsed rows have room for the per-session label.
  Evidence: `mj-tui/src/render.rs::render_sessions_grid` and `sessions_block`.

## Decision Log

- Decision: Keep draft cache in the TUI process, keyed by session identity and matched against complete request equality, without changing ACP wire or persisted schemas.
  Rationale: session switching must preserve local edits while external answers and changed requests must invalidate stale edits.
  Date/Author: 2026-09-04, Luna.
- Decision: Store logical message scroll rather than a wrapped row offset, and recompute the visual offset after a resize.
  Rationale: terminal width changes alter wrapping; preserving a row offset changes the user's reading location.
  Date/Author: 2026-09-04, Luna.
- Decision: Keep reviewer role/source metadata beside the local question state and restore it only with the matching request identity.
  Rationale: reviewer forms share the elicitation UI but their answer route is different from an agent-originated request.
  Date/Author: 2026-09-04, Luna.
- Decision: Source pending-question indicators from full `MaterializedSession` projections and leave the persisted summary shape unchanged.
  Rationale: the complete projection already has the durable pending request list, while adding a wire or database field to the startup summary would widen the schema without improving the live indicator path.
  Date/Author: 2026-09-04, Luna.
- Decision: Advertise F6 and Shift-F6 together on the single `CycleFocus` command and route the reverse direction through the same extracted global event branch used by the dashboard loop.
  Rationale: one registry entry keeps help, palette, footer, and global key matching aligned, while the extracted branch makes reverse behavior directly testable.
  Date/Author: 2026-09-04, Luna.
- Decision: Clear only the question's supplied session-content bounds before rendering, and retain the logical message anchor until the user scrolls again.
  Rationale: the question must hide the replaced transcript rectangle without erasing neighboring dashboard panes, while repeated redraws and width changes must not progressively move a reader through a wrapped paragraph.
  Date/Author: 2026-09-04, Luna.
- Decision: Use a one-row answer-control band in compact panes and share the input wrapper with ratatui's trimmed message wrapping for logical anchor translation.
  Rationale: a six-row content area otherwise spends a blank button row and leaves either the plan text or focused answer inaccessible; using trim-aware wrapping keeps exact boundary and indentation behavior consistent with the rendered paragraph.
  Date/Author: 2026-09-04, Luna.

## Outcomes & Retrospective

The pending-question indicator milestone is implemented and covered by elevated focused tests for full projection propagation, external removal clearing, expanded/minimized rendering, help/palette shortcut text, and the dashboard's forward/reverse F6 event path. The bounded renderer correction now clears exactly the supplied question rectangle, keeps custom input and controls visible in compact panes, maps logical anchors through trim-aware wrapping, and exposes F6/Shift-F6 in the focused footer. Drafts now carry complete request and reviewer source identity, survive a deferred reviewer stream, and are reconciled only against accepted full projections fresh enough to invalidate them. Root completed review, full workspace tests, and clippy without warnings. One unrelated Grok subprocess test reported transient `Text file busy` on the first full run; its focused retry and the subsequent full suite passed. Root owns commit, upstream integration, and the authorized push.

## Context and Orientation

The workspace is a Rust Cargo project with three relevant crates. `mj-chat/src/hel_chat/elicitation.rs` defines `ElicitationDialog`, the local form state machine, mouse handling, and renderer. `mj-chat/src/hel_chat.rs` owns the active chat projection and synchronizes pending ACP requests. `mj-chat/src/hel_chat/active.rs` draws an active chat, handles chat input, and routes reviewer answers. `mj-chat/src/hel_chat/input.rs` is the chat-local keyboard adapter.

`mj-tui/src/combined.rs` composes the navigator, chat panes, and overlays into one terminal frame. `mj-tui/src/lib.rs` owns dashboard pane selection, pane sizes, mouse routing, and global shortcuts. `mj-tui/src/actions.rs` registers command/key behavior and `mj-tui/src/help.rs` describes the user-facing key hints. `mj-cli/src/dashboard.rs` translates terminal events into dashboard actions, starts or queues asynchronous chat attachment, and decides whether prompts have focus. `mj-cli/src/dashboard/io.rs` applies `ChatOpened` results, which is the point where an out-of-order attach can otherwise replace the current chat.

An ACP `ElicitationRequest` is the complete question identity and form definition received from an agent. Its ID alone is insufficient because a request can be replaced in place with changed fields or message text. A draft is the local field values, option cursors, active custom-answer fields, focused control, and logical reading position associated with one request. Drafts are process-local and are never sent over the wire until the user submits.

The existing dashboard also has genuine application modals, such as setup/configuration pickers. Their precedence must remain unchanged. The new question surface is a chat-pane surface: only the active session's content region is exclusive to the question, while navigator and other pane controls keep their normal selection and click behavior.

## Plan of Work

Extend `ElicitationDialog` with a process-local draft snapshot retaining the complete request. Capture and restore only when complete request equality holds; retain field values, option cursors, custom-answer activation, focus, and the logical reading anchor. No serialization or request fingerprint is needed. Represent message position logically (the source line and a column/anchor where practical) and derive the wrapped row at render time. Update `hel_chat.rs` synchronization to compare the complete request, preserve a snapshot before replacing a dialog, and clear it when an answer, cancellation, external removal, changed request, or session removal makes it stale. Keep reviewer role/source identity in the active dialog state and include it in the cache key/value.

Next change active-chat rendering to accept the session content rectangle from the dashboard composition. Render the question and plan review inside that rectangle, leaving pane headers and controls outside. Register explicit message/body/question surfaces with bounds and a focused flag so selection and mouse routing can distinguish question text from hidden transcript text. Reflow the form on resize: plan text receives the remaining room above answer controls, option lists scroll enough to keep the focused option visible, and the current logical plan anchor is restored after wrapping changes. Draw a strong border and keyboard hints for the focused question; inactive questions show `Click to answer · F6 to change pane`. Preserve cursor placement and make the question text selectable while keeping other panes selectable.

Then update `mj-tui` and `mj-cli` event routing. A click in a question focuses that question; a size-button click does not mutate question keyboard focus. While a question is focused, Tab/Shift-Tab traverses its fields and buttons, arrows choose options, Space toggles, and Enter advances or submits. F6 and Shift-F6 cycle panes globally even from inside a question. Outside a question, existing Tab behavior and Alt-Z/Alt-G sizing, help, and command palette behavior remain intact. The genuine modal dispatch path still wins. Mouse events route by location so navigator and other pane controls remain clickable.

Finally harden asynchronous attachment. Snapshot the outgoing chat before queuing an attach and again immediately before replacement. Retain the existing `opening_chat_session` check and queued selection behavior for late attach results; no new generation mechanism is needed. Restore drafts by session identity; restore the matching pending request draft after attach and discard drafts for removed or answered requests. Ensure late results cannot mix a draft from one session into another. Add focused unit/render tests at the owning modules and extend dashboard tests for event routing and attach races.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`.

1. Inspect the current public structs and test helpers in the files named above. Implement the complete-request draft snapshot and reconciliation first, keeping all changes local to `mj-chat` and proving them with colocated tests.
2. Thread explicit session-area bounds through active chat and combined rendering. Add surface/focus metadata without removing existing frame surfaces used by transcript selection.
3. Adjust dashboard selection and event dispatch, then add tests for two pending sessions, clicks, F6 traversal, local Tab behavior, pane sizing, and preserved focus.
4. Reuse the existing late-result guard in `mj-cli/src/dashboard/io.rs` and retain draft capture and restoration in `mj-cli/src/dashboard.rs`. Pair cached drafts with their capture ordinal; prune only against a sufficiently fresh full pending-request projection. Stored summaries must not advance that full-projection ordinal. Verify session removal and external answers invalidate obsolete drafts.
5. Format changed Rust files with rustfmt. Run focused crate tests while iterating, then from the repository root run the required full `cargo test` and `cargo clippy --all-targets -- -D warnings` with the repository's elevated test permission requirement. Do not redirect Cargo output into `/tmp`.

Expected successful validation ends with Cargo reporting all tests passed and clippy completing without warnings. `git status --short` should show only the implementation files and this plan, plus the two pre-existing unrelated modifications.

## Validation and Acceptance

The feature is accepted when behavior tests show that two sessions retain distinct drafts and logical scroll positions while switching sessions and resizing; an externally answered or changed request does not resurrect the old draft; a removed session clears its entries; and a late attachment result cannot replace the current session or import its draft.

Rendering tests must prove that the question rectangle is limited to the session content area, long plan text and option lists remain usable at small, standard, maximum, and minimum sizes, and the focused question has the strong border and hints while inactive questions expose the click/F6 hint. Input tests must prove local Tab/Shift-Tab, arrows, Space, Enter, click focus, size-button focus retention, global F6/Shift-F6, and preserved existing Tab/Alt-Z/Alt-G/help/palette behavior. Existing configuration/setup modal tests must continue to pass.

Run focused tests from the repository root as they become available, for example `cargo test -p brokk-mj-chat elicitation` and `cargo test -p brokk-mj-tui`, then run `cargo test` and `cargo clippy --all-targets -- -D warnings`. Every Cargo test command must run with elevated permissions as required by `AGENTS.md`.

## Idempotence and Recovery

All edits are additive or local state transitions. Re-running formatting and tests is safe. Do not reset or overwrite the unrelated existing changes. If an implementation experiment fails, revert only files changed for this plan or use the focused tests to repair it; do not use destructive repository-wide commands. Draft cache is process-local, so restarting the TUI intentionally starts with empty drafts while server-side pending requests remain available.

## Artifacts and Notes

The primary artifact is the code and behavior tests in the existing `mj-chat`, `mj-tui`, and `mj-cli` crates. The living plan records progress and evidence here; no separate product documentation is required.

## Interfaces and Dependencies

The implementation continues to use the existing `agent-client-protocol` elicitation types, `ratatui` layout and rendering, Crossterm key/mouse events, and `FrameSurfaces`/`SurfaceFrame` selection infrastructure. The final state should expose, within the existing crate visibility boundaries, complete-request draft snapshots on `ElicitationDialog`, explicit question bounds/focus in the active-chat renderer, and draft capture/restore around the existing session-identity-checked attachment flow. No ACP protocol type, persisted session schema, or external dependency changes are expected.

Plan revision note (2026-09-04): created after source inspection to capture the complete scope, initial architecture decisions, and validation requirements before implementation.
Plan revision note (2026-09-04): recorded the completed pending-question indicator and shortcut-discoverability milestone, including the decision to consume pending requests from full materialized projections and the evidence from elevated focused tests.
Plan revision note (2026-09-04): recorded the renderer correction milestone and its focused elevated test evidence; dashboard draft reconciliation and final workspace validation remain with the parent review pass.

Plan revision note (2026-09-04, root review): recorded successful full validation and corrected the design narrative to match the implementation: complete request equality, existing attach guards, and independently tracked full pending-request ordinals. The implementation and automated review are complete; delivery proceeds on the current branch.
