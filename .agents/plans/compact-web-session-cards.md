# Compact web session cards

This living ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

The web workspace dashboard will show three compact lines per session: name, target/workdir with profile, and live Turn/Step clocks or a truthful idle time. Project headers remain. Opening the page sorts groups and sessions by recent activity once; later updates preserve their positions. Long press and an accessible menu replace visible action rows.

## Progress

- [x] Approved design and inspected current dashboard and activity sources.
- [x] Implement runtime idle transitions, background completion persistence, and legacy timestamp handling.
- [x] Finish structured server activity data and seed durable activity before the first published snapshot.
- [x] Implement stable cards, clocks, ordering, and action menu.
- [x] Validate integrated behavior and review the code and narrow-screen screenshot.
- [x] Commit the validated changes on the current branch as the completed implementation checkpoint.

## Decision Log

Recovery cannot reconstruct process-local background state from a journal crash tail. On uncertain recovery, clear the idle timestamp and show Idle until a known transition; retain it across ordinary reopen and metadata-only warnings. Runtime callbacks now return persistence errors to their existing supervised runtime caller.

The HTTP snapshot endpoint refreshes server_time_ms on every response. The stored projection can remain unchanged for hours; using its creation time would reset browser clocks backwards on reload or repeated fetches. A regression test serves a deliberately old projection twice and verifies fresh response anchors.

Remove departed DOM siblings before inserting arrivals. Moving retained nodes merely to close a gap would lose focus; browser coverage now verifies focused sessions survive sibling and project removal/reappearance. Foreground work without a turn displays only Step, matching the shared TUI classification. Titles occupy one line of the three-row card, including when ellipsized.

On 2026-09-06 the user chose group ordering by newest member activity as well as session ordering. Ordering lives only for the browser document lifetime; navigation and reconnect do not reset it. New arrivals append and removed IDs retain remembered positions. Creation time substitutes for missing activity; stable IDs break ties.

The three-line layout preserves errors, input requests, and queue counts as compact accessible title indicators. Background activity and lifecycle operations occupy the third line when appropriate. Existing Stop confirmation and rename prompt remain. A 500 ms long press opens the capability-controlled menu, with movement beyond 10 px, scrolling, and cancellation aborting the gesture. A successful press suppresses navigation. The ellipsis button, right click, and keyboard context menu provide equivalent access.

## Surprises & Discoveries

The core test run passed 813 tests, with four ignored and one unrelated executable-file-busy failure in the worker teardown test. That test passed when rerun alone. New idle behavior tests passed. An optional check including every workspace member found missing host GTK/libsoup dependencies for the desktop crate; required validation uses the configured default members.

The web currently replaces every card on refresh, and only receives formatted activity text. The runtime already has turn and step clocks, but no retained idle-entry timestamp. The durable materialized session has a last-activity watermark independent of retained transcript rows. Generic session update time cannot describe idle entry.

## Context and Orientation

`mj-controller/src/web/viewer.js` renders dashboard cards, handles routing, and dispatches actions; adjacent CSS controls layout. `mj-controller/src/hel_server.rs` defines public viewer DTOs, and `mj-cli/src/server.rs` enriches them from background session projections. `mj-chat/src/usage_format.rs` classifies activity for the TUI. `src/hel_worker.rs` and its snapshot module own relay activity and persistence. Browser tests live in `tests/e2e/web`.

## Plan of Work

First add optional structured activity timestamps and display location to ViewerSession, with additive serialization defaults. Publish server time in milliseconds. Reuse project_target for the display location and materialized last_activity_at_ms for sorting. Share activity interpretation with the TUI; preserve existing formatted fields. Track idle transitions in the runtime and retain the timestamp in its snapshot, clearing it on resumed work. Unknown older-worker history remains timestamp-free Idle.

Next implement keyed group/card updates and document-lifetime ordering maps per workspace. Sort the first snapshot descending, groups by maximum child activity; append later arrivals. Keep each row single-line and truncate text without hiding the profile. Render clocks using the TUI units (36s, 43m36s, 1h43m, 2d03h), anchored to server time and updated once a second without replacing card nodes. Show idle times in local time, adding yesterday or a date when needed.

Finally relocate permitted actions into a keyboard-accessible menu. Preserve existing requests, confirmations, pending-action guards, and error reporting. Close or reconcile the menu when capability or session state changes. Update browser tests for the advertised layout and gestures rather than internal construction lists.

## Concrete Steps

Implement backend and browser changes independently with nonoverlapping ownership; root owns runtime idle tracking and integration. Run focused checks while integrating, then from `tests/e2e/web` run `npm run test:unit` and the self-contained browser specs selected through MJ_BROWSER_SPEC. The default reliability spec requires an external live lab and its MJ_BROWSER_* credentials. From the repository root run `cargo fmt --all -- --check`, elevated `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Review the diff, stage only task-owned files, and commit on the current branch.

## Validation and Acceptance

Verify initial group/session recency, deterministic ties and missing activity, no movement after live updates/navigation/reconnect, appended arrivals, and a fresh order after reload. Verify clocks for foreground/autonomous/background work and idle timestamps across completion, metadata updates, and worker reopen. Verify old serialized states deserialize without fabricating history. Browser tests must cover narrow screens, long names, press-versus-scroll, suppressed follow-up clicks, keyboard actions, Stop confirmation, and snapshots during a press. All required suites must pass before commit.

## Idempotence and Recovery

Changes are additive in existing crates; no new dependencies or database migration is intended. Missing optional timestamps display honest labels. Existing public activity fields remain supported. Tests use isolated fixtures. No deployment or push is part of this implementation request.

## Interfaces and Dependencies

ViewerSession adds display_location, optional last_activity_at_ms, and optional activity_details. Activity details carry kind (turn, step, background, idle, lifecycle), optional turn_started_at_ms, step_started_at_ms, background_started_at_ms, idle_since_ms, and label. ViewerSnapshot adds server_time_ms. RelayOperationalState adds optional idle_since_ms backed by runtime persistence. Shared activity interpretation stays in the existing crate boundary, with no web string parsing of formatted TUI clocks.

## Outcomes & Retrospective

The full default-member cargo test suite passed, including 814 core tests and all controller/chat/TUI/worker/CLI suites. The final controller rerun passed 653 tests with one ignored, including the HTTP response clock regression. Clippy and rustfmt checks passed. All 15 web unit tests and 23 self-contained browser checks passed. The new five-test compact-card suite covers clocks, initial ordering, appends/reappearance, reload, reconnect, navigation, focus, active presses, cancellation, keyboard menus, capability changes, Stop confirmation, and action errors. Visual inspection of `/tmp/compact-web-cards.png` confirmed a bounded 390 px layout with three rows, project headers, attention indicators, and local idle times.

No new crate, dependency, database migration, or deployment was needed. Older workers without a known idle start show Idle. The external live-lab reliability scenario was not run; it requires lab URLs and credentials. The optional desktop build remains unvalidated because this host lacks GTK/libsoup development libraries; desktop is outside the configured default members and this change's scope.

## Artifacts and Notes

Initial revision records the approved design and parallel ownership before implementation.

Final browser commands from `tests/e2e/web` were `npm run test:unit`, `MJ_BROWSER_SPEC=compact-cards.spec.js npx playwright test`, `MJ_BROWSER_SPEC='@(project-groups|new-session|plan-mode|quota).spec.js' npx playwright test`, and `MJ_BROWSER_SPEC=layout.spec.js npx playwright test --grep 'very long unbroken'`. Results were 15, 5, 17, and 1 passing checks respectively.

Updated on 2026-09-06 with integration corrections and validation evidence, including the distinction between three card rows and a single-line title.
