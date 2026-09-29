# Restore TUI behavior with local workspace tabs


This living ExecPlan follows `.agents/PLANS.md` and must be updated during implementation.

## Purpose / Big Picture


Restore useful session filtering, meaningful status colors, current previews, correct Quick New mouse editing, and settled stopped-chat animation. Replace the separate workspace screen with local tabs above Sessions and an F3 management modal. Sessions remain live globally; tabs only filter the view. Never create sessions automatically. Targets and Quota move together below the sidebar and conversation when the adjacent column cannot fit both full tables. Require an 80-column terminal and preserve the existing height guard.

## Progress


- [x] (2026-09-08) Reviewed revamps 6e0d0454 and ea3ce678, reproduced input/status regressions, and agreed on the design with the user.
- [x] (2026-09-08) Assigned independent input, animation, daemon presence, and workspace UI changes with separate ownership.
- [x] (2026-09-08) Implemented global daemon presence, protocol 15, explicit-only session creation, local tab switching, and workspace-scoped startup selection.
- [x] (2026-09-08) Integrated workspace management, per-workspace layout persistence, origin-preserving create/resume actions, deletion cleanup, and hidden-chat read receipt guards.
- [x] (2026-09-08) Restored semantic colors/current previews and adaptive support-pane placement with actual-render tests at 80 columns and at the measured threshold.
- [x] (2026-09-08) Updated user documentation; docs npm ci and npm run check passed with zero errors/warnings/hints.
- [x] Committed as 2f268023 "Pull workspace management into tabs" (2026-09-08).

## Surprises & Discoveries


Quick New registers a field without a cursor map, so every mouse click returns offset zero. The normal quota table still produces percentages but the new narrow allocation clips them; ordinary columns require about 75 cells including borders and the selection marker. Several existing rendering tests were redirected to a test-only wide renderer and bypass the sidebar. Daemon attachments previously contained workspace identity for presence/deletion bookkeeping, not ownership of session execution. Real-terminal startup tests caught a direct touch_workspace database write outside the daemon writer; launch now records the initially opened workspace through a daemon request, while tab changes remain local. All five real-terminal tests pass after that correction. Final review also added known-workspace guards so switching away from a deleted tab cannot requeue its forgotten pane-size save.

## Decision Log


Use one global feed and synchronous local tab filtering. Do not add an attachment switch actor, new switching mutex, network acknowledgement, or loading state. Make daemon attachments global client presence instead. This replaces an unnecessarily complex draft plan.

Use dedicated semantic shades in the new theme rather than reverting the whole theme. Preserve old status precedence: failure first, pending questions, idle, unread, ordinary activity.

Keep the sidebar width/minimized redesign deferred. Add tabs and adaptive Targets/Quota as agreed. Both support panes always relocate together, based on full contents and hypothetical maximized sidebar width. Preserve all percentages and reset fields. Full-width normal content is assumed to fit at the new minimum of 80 columns; do not add horizontal scrolling.

No automatic session creation, including empty tabs and startup. Preserve legacy startup.enabled parsing but ignore and hide it. F3 manages workspaces inside the dashboard and preserves destructive confirmations and draft recovery.

## Outcomes & Retrospective


Implementation in progress. Documentation checks pass. Chat, controller, and core suites passed (442, 714, and 867 tests respectively; existing opt-in tests remained ignored). The TUI suite passed 365 tests with two existing ignored captures. The input failures exposed a duplicate-coordinate tab boundary and a test using stale geometry after insertion; both were corrected. Two CLI test harness expectations were corrected before the final integrated run. Final integrated Rust validation and commit evidence remain pending.

## Context and Orientation


`mj-tui/src/lib.rs`, `actions.rs`, and `ingest.rs` own pure dashboard state and input actions. New `mj-tui/src/workspaces.rs` provides workspace tabs and management. `render.rs` and `combined.rs` render session/status tables and allocate rectangles. `mj-cli/src/dashboard.rs` and its `actions.rs`, `io.rs`, and `pane_sizes.rs` execute background work and preserve visible chat state. `mj-cli/src/main.rs` now opens the dashboard directly; the old standalone workspace selector has been removed. `mj-cli/src/daemon.rs` is the persistent process that owns sessions and database mutations; its client attachments become global presence. `mj-chat/src/components` and `hel_chat/input.rs` contain reusable text controls and wrapping. `mj-chat/src/hel_chat.rs` controls chat animation.

## Plan of Work


Milestone one fixes Quick New with shared grapheme-aware multiline rendering and two-dimensional cursor maps; render the real cursor rather than insert a wrapping-changing marker. Preserve prompt bytes including tabs, hard newlines, and Unicode. Closed primary sessions must not animate from stale work, while independent working reviews still animate.

Milestone two changes attachments to client ID/PID presence and advances the daemon protocol from 14 to 15 with its compatibility fixtures. Remove workspace-attached deletion gates but keep session/draft/lifecycle guards. Publish workspace changes through runtime revisions. Open directly in the requested or most recently opened workspace; mj workspaces opens F3 management. Keep tab order stable within a run. Filter session rows by active workspace, excluding archived and settled historical records; ongoing terminal transitions remain until their owners finish. Cache per-workspace selection, folds, offsets, and pane sizes. Tab changes capture existing drafts and reuse session-switching checks; read receipts apply only to visible chat. Existing asynchronous saves carry workspace IDs. Launch preparation retains its originating workspace and completion cannot focus another tab. F3 exposes create, rename, confirmed delete, and draft recovery using supervised existing daemon calls. Deleting the last workspace leaves an empty management-capable UI without creating a session.

Milestone three shares preview precedence between row widths: current reply, current tool/thought activity, latest user prompt, then empty. Restore dedicated red/error, amber/busy, bright-yellow/attention, and blue/idle session colors. Measure prepared table rows once and use the same widths for fit and rendering. If both Targets and Quota fit beside a maximized sidebar, retain adjacent layout; otherwise stack both across the full frame below sidebar/conversation. Update surfaces, controls, scrolling, and height allocation consistently. Keep existing full quota fields, including both percentages and reset values. Require 80 columns and test the real renderer.

## Concrete Steps


Work from `/home/jonathan/Projects/hel` on the current branch. Root integrates runtime/layout; Luna agents implement independent bounded changes. Run focused tests as each integration checkpoint becomes buildable, avoiding competing Cargo invocations. Finally run:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    git diff --check

Every cargo test runs with elevated permissions as required by AGENTS.md. Keep build output in the normal target directory. Run applicable documentation checks after updating user docs. Stage only task files and commit validated coherent changes to the current branch, without pushing.

## Validation and Acceptance


Use actual dashboard and modal rendering. Prove active/history/archive/transition filtering across two tabs; immediate local tab changes; draft and pane-size preservation; no automatic session creation even for legacy enabled=true; correct operation workspace capture and stale result behavior; management create/rename/delete/recover responsiveness; live workspace updates; and client presence independent of tabs.

Prove all status colors at normal widths and current preview chronology. Mouse tests must click and then insert into wrapped, multiline, scrolled and Unicode Quick New prompts; retain a larger-than-64KB case. Stopped chat must cease animation, while running/background/review work continues. At 80 columns, normal table fixtures must retain both exact percentages and reset fields across the full width. Test immediately below/at fit threshold, both sidebar sides, all pane sizes, resize, and minimum-size guard. Wide-only tests remain only for genuine wide previews.

## Idempotence and Recovery


No data migration or deletion is performed as part of development. Existing config remains readable and session workspace identities stay unchanged. Use isolated test state. Background operations retain existing error reporting and bounded cleanup. Preserve unrelated work; all edits here are task-owned.

## Artifacts and Notes


Pre-implementation review suites passed 358 TUI and 62 dashboard tests. Temporary probes reproduced missing compact question text and Quick New cursor zero; those probes were removed before implementation began.

## Interfaces and Dependencies


Use existing workspace records, daemon RPCs, Form controls, text wrapping, runtime projections, and session attachment helpers. No new crate or external dependency. Add UI actions for selecting and managing workspaces; selecting is local, mutations remain supervised. Public configuration keeps New defaults and accepts the obsolete startup.enabled field. Advance only the internal daemon protocol for presence/snapshot shape changes, preserving its frozen management subset.

Revision: Initial execution record incorporates the final simplified tabs design and 80-column minimum.

Revision (2026-09-08): Recorded implementation progress and documentation validation. Review clarified that adjacent Targets/Quota retain the existing vertical stack; fit compares the larger table width against the conversation column, and tabs occupy only the row above Sessions. Added handling for deleting a workspace with pending layout saves and for preventing hidden warm chats from advancing read receipts.

Revision (2026-09-08): Create and Resume actions now carry their initiating workspace through validation and confirmation, rather than capturing a later tab. Removed the obsolete standalone workspace-preview renderer and exercised session presentation through the real dashboard. Added scrolling Unicode-aware workspace tabs, preserved manager results behind Help, and made pending layout saves forget deleted workspaces. Final full-suite and Clippy validation are in progress.

Revision (2026-09-08): Committed the validated Quick New and animation checkpoint as bbf47f93. Updated PTY coverage for explicit creation and in-dashboard F3 management; all five tests pass. Final full Rust validation remains running. Corrected terminal-surface documentation for local tabs, active-only rows, and adaptive support panes; documentation checks again report zero errors, warnings, and hints.
