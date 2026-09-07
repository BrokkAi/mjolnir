# Show live expanded sessions in the workspace selector

This ExecPlan follows `.agents/PLANS.md` and must be maintained through implementation. The selector will show the same expanded session rows as the workspace dashboard, allowing users to identify a workspace from actual session content rather than project counts.

## Progress

- [x] (2026-09-06) Inspected selector, dashboard rendering, daemon snapshots, and projection ingestion; confirmed the CLI already depends on the TUI crate.
- [x] (2026-09-06) Confirmed user preferences: scrollable preview only, Enter opens workspace, live updates.
- [x] (2026-09-06) Shared expanded rendering; focused parity, scrolling and preview state tests passed (9 tests).
- [x] (2026-09-06) Extracted the read-only runtime feed and integrated asynchronous selected-workspace loading and cancellation.
- [x] (2026-09-06) Full workspace tests, final CLI regressions, Clippy with warnings denied, formatting and diff checks passed; reviewed integration and committed the renderer checkpoint.
- [x] (2026-09-06) Completed the validated CLI integration and execution record for the final commit on the current branch.

## Surprises & Discoveries

The existing `preview_lines` only emits project counts. `mj-tui/src/render.rs` already owns the desired four-line session rows and all their status formatting. `mj-cli/src/pollers.rs` reconstructs daemon-published projections from bounded SQLite transcript tails with ordinal/digest convergence checks; reuse that path rather than inventing a second interpretation.

The runtime snapshot includes inactive records as well as live ones. Resuming a stopped session must retain its record until the operation overlay makes it visible. Finished overlays must be removed before refreshing records, otherwise a completed resume can incorrectly retain Provisioning state. Project-resolution inputs can change without a configuration change, so generation invalidation also compares the checkout, managed worktree, target and bundle.

The initial real-terminal handoff test failed with `unexpected end of file` because its existing fixture deliberately stopped the daemon after the dashboard detached. The handoff test now uses normal persistent-daemon behavior and explicitly stops that daemon before removing its files. The resulting test passed and verified selector signal shutdown in less than one second.

## Decision Log

Use the existing TUI dependency, with no new crate or protocol migration. Keep the preview read-only and force expanded rows without dashboard action hints or selection carets. Reuse the read-only feed while keeping command forwarding and persistence actions dashboard-specific. These decisions preserve display fidelity without giving the picker conversation side effects.

## Outcomes & Retrospective

The selector now renders shared expanded rows and updates them from the same bounded, read-only runtime feed as the dashboard. It scrolls independently, retains content with visible refresh errors, reports daemon notices, and cancels obsolete subscriptions. It preserves management actions and never attaches a conversation or advances read status. SIGTERM exits directly instead of reopening a fallback dashboard; selector cancellation uses the existing disposable-work runtime shutdown path.

Validation passed: full `cargo test`; CLI tests after the lifecycle and terminal fixes; all 13 final selector tests after the summary-title fix; `cargo clippy --all-targets -- -D warnings`; `cargo fmt --check`; and `git diff --check`. The usual four external-harness import tests remain ignored by the suite. The PTY integration test verifies actual dashboard-to-selector handoff and live empty-state delivery; nonempty row fidelity is covered by renderer comparisons and controlled feed/state tests. Renderer checkpoint: `f40a60d7`.

## Context and Orientation

`mj-cli/src/main.rs::run_workspace_dashboard` currently fetches snapshots for every workspace before calling the synchronous picker in `mj-cli/src/workspace_selector.rs`. The picker owns terminal input and workspace management shortcuts. `mj-tui/src/render.rs::drawn_session_rows` formats the dashboard's expanded list from `DashboardState`, whose ingestion methods accept prepared materialized projections (derived session messages and status). `mj-cli/src/pollers.rs::spawn_remote_dashboard_worker_poller` reads live daemon records and SQLite projection tails, then publishes them to the dashboard's session manager. The renderer agent owns only TUI rendering changes; the primary owns CLI concurrency, integration, validation and commit.

## Plan of Work

First expose `render_sessions_preview(frame, area, dashboard, scroll_state)` and `SessionsPreviewState` from `mj-tui`. Share row construction with the dashboard. Preview rows are expanded, have no selected caret or project number controls, and use a bordered Sessions pane. Scroll state supports lines, pages, home and end and anchors by session identity across updates. Prove same content/styles at equal widths and safe small-terminal rendering.

Next extract a read-only feed from the existing daemon polling/projection path, retaining bounded reads, fingerprints and convergence retries. Send records/configuration/lifecycle/review updates before individual session details, report connection failures, and give consumers explicit cancellation ownership. The dashboard bridges this feed to its existing session manager and command forwarding. The selector applies the same presentation data without attaching chat, acknowledging messages or invoking record-persistence/diagnostic helpers. Resolve project sources and prepare summaries/details in supervised background tasks with bounded concurrency.

Finally make the picker asynchronous with EventStream and a one-second clock tick. Load only the selected workspace and its management metadata in background. Keep the 34/66 column split, existing operations and Enter behavior. Up/Down and j/k choose workspaces; PageUp/PageDown, Home/End, and wheel events over the preview scroll rows. Changing workspaces resets scroll and cancels old work; generation checks reject stale completions. Show loading, empty and failure states, retain rows with a stale notice on refresh failure, and retry. Cleanup must not wait indefinitely on daemon long polling or blocked work.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`. Run focused tests for the TUI renderer and selector/feed state transitions as each milestone completes. Run every `cargo test` outside the restricted sandbox. At integration run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Review `git diff --check` and the actual patch. Stage only changed files and commit on the current branch; do not push.

## Validation and Acceptance

Rendering tests compare expanded dashboard and preview content/styles while allowing only omitted controls/carets. Cover project/target disambiguation, active filtering, messages, clocks, pending input, reviews, operations, disconnects and scrolling. Controlled feed tests must prove loading before completion, updates while idle, stale-result rejection, retry after failures, session insertion/removal, and cancellation of delayed work. Verify preview reads do not change read cursors or create chat attachments. Existing rename/delete/recovery behavior remains valid. A terminal check should show the same sessions in picker and expanded workspace view and allow reaching the last session with End.

## Idempotence and Recovery

No migration, release or destructive data operation is required. Source edits and validation can be repeated. Cancel background subscriptions on every selector exit, including errors. Keep failures visible instead of substituting empty session lists. Revert only task-owned edits if necessary.

## Artifacts and Notes

`cargo check -p brokk-mjolnir --tests` and elevated `cargo test -p brokk-mj-tui -p brokk-mjolnir preview` passed.

The shared runtime feed lives in `mj-cli/src/pollers.rs`; injected poll/read functions allow deterministic tests in its `runtime_feed_tests` module. `WorkspacePreview` in `mj-cli/src/workspace_selector/preview.rs` owns a separate completion queue for each selected workspace. Replacing it cancels and discards the old queue, enforcing workspace identity without redundant generation tags on each message. Configuration-sensitive project resolutions additionally carry a generation number.

Blocking projection reads share four global slots. Message preparation and Git/SSH source resolution use separate bounded pools, so slow source resolution cannot delay message detail. Slots remain occupied until cancelled blocking work exits. A completed live detail supersedes a same-ordinal startup summary. Live titles remain in memory across record refreshes without writing session titles or read status.

The final CLI tests cover stopped-session resume visibility and completion, stale source results after record changes, one-time runtime notices, late same-ordinal startup summaries, retained stored titles without a live relay, metadata failures, removed sessions, and cancellation of delayed preparation. Feed tests cover ordered publication, unchanged projection skipping, failure/recovery, bounded overlapping loads, cancellation and digest/ordinal integrity.

## Interfaces and Dependencies

Use the existing `hel_tui::DashboardState` ingestion API and add public `SessionsPreviewState` plus `render_sessions_preview`. Use Tokio tasks/channels/cancellation, crossterm EventStream and existing daemon snapshot methods. Keep read-only feed types within `mj-cli`; do not expose CLI daemon types across crate boundaries. No new dependencies are expected.

Revision note (2026-09-06): recorded completed integration and subscription-ownership, concurrency-pool and summary-freshness decisions discovered during implementation.

Final review note (2026-09-06): recorded lifecycle/source freshness fixes, signal handling, PTY fixture correction and passing validation. No schema, dependency, release or protocol change was needed.
