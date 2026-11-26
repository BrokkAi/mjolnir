# Select multiple hourly usage windows

This living ExecPlan follows `.agents/PLANS.md`. Keep its progress, discoveries, decisions, and outcome current.

## Purpose / Big Picture

The Usage by session dialog currently filters its table to one clock hour. Enable Ctrl-click to toggle individual hours and Shift-click to select an inclusive range, with every selected bar highlighted and every cost and quota total counting exactly those hours. Enter restores the full 24-hour graph. A normal click selects one hour.

## Progress

- [x] (2026-11-25) Map the dialog, request, store accounting, quota enrichment, and existing tests.
- [x] (2026-11-25) Set interaction and shared request semantics; assign disjoint implementation ownership.
- [x] (2026-11-25) Implement the shared query, exact accounting, quota selection, and HTTP compatibility coverage.
- [x] (2026-11-25) Implement modifier clicks, range anchor, rendering, and regenerated golden coverage.
- [x] (2026-11-25) Run targeted boundary tests and affected-crate clippy through the build owner; five focused tests and clippy passed.
- [x] (2026-11-25) Commit the integrated feature as ef4afc441 and request review.
- [x] (2026-11-25) Fix accepted review findings: reconcile selections when graph hours roll forward and clear keyboard cursor on Enter; affected TUI golden and clippy passed.
- [x] (2026-11-25) Review the integrated diff, commit, obtain required review, merge upstream without conflicts, and push to origin/master (0a7e71dd3).

## Surprises & Discoveries

The graph's bars contain only hourly cost summaries. They cannot supply session, model, role, or profile breakdowns for an arbitrary selection. The request must therefore express selected hours and the existing store aggregation must filter them. No database migration is necessary.

The daemon adds weekly quota estimates using the report's scalar time bounds. Those bounds enclose gaps for a discontiguous selection, so quota estimation must also use only the selected intervals. Active turns similarly affect partial coverage only when they overlap a selected interval.

The human documentation still describes a removed period selector. Replace that short passage while documenting the graph's actual selection gestures.

The full dashboard requires 60 columns. The hidden-bar test renders the actual modal directly at 28 columns to reach the mapping that omits older graph bars. Golden verification passed after correcting this fixture. The whole-workspace formatter also reports unrelated baseline differences in suspend_consent.rs and daemon source/tests; all eight feature Rust files pass the pinned formatter independently.

## Decision Log

Normal click selects a singleton and sets the range anchor. Ctrl-click toggles the clicked hour and sets the anchor. Shift-click replaces selection with the inclusive range from the anchor; it keeps that anchor stable for repeated extension. Ctrl+Shift-click adds that range. With no anchor, a modifier click establishes one at the clicked hour. The reason is familiar multi-selection behavior without losing discontiguous selection support. Enter resets to all and clears the anchor. Keyboard arrows retain single-hour navigation.

Represent the selected set in `SessionUsageQuery.hour_starts_ms: Option<BTreeSet<i64>>`. None, together with an absent legacy scalar field, means all graph hours. Some(empty) means no selected hours and must remain distinct from all. Encode a present set as one comma-separated query value so the existing reqwest query serializer and Axum extractor agree; an empty string represents the empty set. Keep `hour_start_ms` working for existing clients; the new set takes precedence when present. Normalize and deduplicate hour timestamps in the shared selection interpretation rather than separately in consumers.

Keep the graph's 24 bars independent of the selected set. Apply the selection only to report rows and cost, model, role, profile, coverage, and quota totals. Scalar report bounds can describe the enclosing span, but cannot be used alone to count usage within gaps. Preserve graph refresh and request-generation cancellation. Avoid changing generic form controls, stored schemas, build paths, or go/.

Accepted review fixes: a refreshed graph is the owner of which hours are selectable. Reconcile the explicit set with its hour starts when accepting a response; if hours expire, advance the existing request generation and reload the filtered report instead of displaying totals for hidden hours. Clear expired anchors/cursors, and clear the cursor on Enter to restore navigation from the latest bar. This keeps rollover within the existing request lifecycle, with no new background mechanism.

The second review's sole finding requested skipping long historical API gaps in the SQL read. Defer that existing performance issue: baseline ae3157fad already scans and prices the same enclosing history for one historical scalar hour, while the requested dialog uses and reconciles only the current 24 graph hours. Record the review override and mark 6e0fc708f as the reviewed baseline before merging upstream. No data correctness finding remains.

The backend child owns non-TUI code and its boundary tests; the TUI child owns mj-tui changes and the small human documentation passage. Neither builds while both edit. A single validation child will own builds after both slices are ready. The parent owns this plan, final design decisions, integrated review, commits, review requests, and push.

## Outcomes & Retrospective

Completed and pushed to origin/master. Ctrl-click toggles hours, Shift-click selects an anchored range, Ctrl+Shift adds a range, and Enter restores all hours. Both accepted review findings are corrected and verified: expired selections trigger a fresh request without showing obsolete totals, and Enter resets the navigation cursor immediately. The golden covers rollover, stale-response rejection, replacement acceptance and expiry to empty. Store and quota checks prove gaps do not count. HTTP checks preserve legacy single-hour requests and distinguish omitted and empty sets. The remaining historical API scan optimization was explicitly deferred as pre-existing and outside the visible-chart feature.

## Context and Orientation

`mj-tui/src/session_usage_report.rs` owns the dialog state, handles mouse and keyboard events, and renders the hour chart. `Form::handle` provides the clicked visible-column index; the original mouse event contains modifiers. `hidden_hours` maps narrow-terminal visible bars back to graph hours. `mj-tui/src/dialogs/tests.rs::golden_dashboard_dialogs` already exercises and renders single-hour selection, with expected output in `mj-tui/tests/golden/dashboard-dialogs.txt`.

`mj-core/src/usage/report.rs::SessionUsageQuery` is shared between terminal, HTTP client, HTTP server, and store. The terminal sends a cloned query through `mj-cli/src/dashboard/usage_report.rs`, then the API client's GET /usage/report query serialization. `mj-server/src/server/api/turns.rs` extracts it. `mj-daemon/src/server_runtime/api.rs::usage_report` asks the store to aggregate usage, then adds quota estimates from `mj-controller::quota_telemetry`.

`mj-store/src/database/usage_report.rs::load_with` reads one SQLite snapshot, aggregates completed turns by parent and other categories, creates graph bars, and marks incomplete coverage from active turns. Its existing retained-tree integration case and `mj-server/src/server/api/tests.rs::usage_report_http_contract_preserves_filters_and_requires_authentication` are the starting boundary coverage. The read-only investigation is in `.mj/agents/e5aa307e9e1afcf072ab1bbd65ef1e60/usage-by-session-multiselect-map.md`; the implementation must not rely on that untracked report to understand this plan.

## Plan of Work

First extend the query contract and implement a shared interpretation of selected intervals, retaining the old scalar request. Filter the existing store aggregation and active-turn coverage using it. Extend quota estimation so gaps are excluded and adjacent selected hours do not introduce false missing-coverage warnings at artificial boundaries. Extend the existing HTTP and store tests with empty, deduplicated, normalized, and discontiguous selections where those protect real boundary behavior.

In parallel extend the dialog with a range anchor, keeping the query as the single selected-set owner. Read modifiers locally around Form handling, resolve visible bars through hidden_hours, and update request generation whenever selection changes. Highlight every member while retaining one keyboard cursor. Render a concise selection label, explain the gestures in the dialog hint, and extend the existing golden sequence with toggle, range, stable anchor, empty, reset, and narrow-display behavior as applicable.

Once both slices finish, one child formats and runs targeted tests and clippy. Fix failures through the owning child, then rerun only failures or tests affected by a new edit. Parent inspects decisive diffs and evidence, commits the coherent feature, calls request_review, resolves or overrides findings, and pushes current master to origin/master. If upstream moved, fetch and merge rather than rebase or create a branch.

## Concrete Steps

Run commands from the repository root. Use mbx for Rust work; never change target directories or use plain Cargo without approval. All Rust tests must run with elevated sandbox permissions. The actual successful commands were `TZ=UTC MJ_UPDATE_GOLDEN=1 mbx test -p brokk-mj-tui golden_dashboard_dialogs`, then the same command without MJ_UPDATE_GOLDEN; `mbx test -p brokk-mj-store usage_report_preserves_retained_trees_and_filters_completed_usage`; `mbx test -p brokk-mj-accounts weekly_consumption_clips_spans_and_never_turns_unmeasurable_into_zero`; `mbx test -p brokk-mj-server usage_report_http_contract_preserves_filters_and_requires_authentication`; and `mbx test -p brokk-mjolnir usage_report_query_round_trips_through_http_form_encoding`. Each ran one matching test and passed.

Run focused store, HTTP, quota, and TUI tests, then `mbx clippy -p <affected-package> --all-targets -- -D warnings` for affected behavior crates. Use existing isolated configuration/data directories in automated tests. Any manually started new binary must use `--instance usage-hour-multiselect-test`, and its daemon must be stopped afterward. No live daemon or store changes are authorized or needed.

## Validation and Acceptance

The extended dialog golden must show two separated bars selected after Ctrl-click, a contiguous range after Shift-click, and the existing full graph after Enter. State assertions must verify the outgoing query and stable anchor. The store integration case must prove that an unselected gap contributes no cost, tokens, category totals, or active-turn coverage. The HTTP contract case must prove comma-separated and empty selection extraction plus existing scalar compatibility. Quota tests must prove estimates exclude gaps and preserve coverage semantics. Formatting and affected-crate clippy must pass, or report concrete pre-existing failures with decisive evidence.

## Idempotence and Recovery

These edits require no stored-data migration or installation. Tests use isolated state. Builds may be rerun after failures without altering build layouts. Preserve unrelated concurrent edits, stage only this task's explicit paths, and never use git add -A. Keep detailed logs in child report directories; summarize decisive results here.

## Artifacts and Notes

Initial checkout was clean on master at ae3157fad. The combined dev-profile clippy passed with `mbx clippy -p brokk-mj-core -p brokk-mj-store -p brokk-mj-accounts -p brokk-mj-server -p brokk-mj-daemon -p brokk-mjolnir -p brokk-mj-tui --all-targets -- -D warnings`. All eight changed Rust files passed `rustup run 1.99.1 rustfmt --edition 2024 --check`; git diff --check passed. Detailed backend/clippy logs are in `.mj/agents/e5aa307e9e1afcf072ab1bbd65ef1e60/` and TUI golden logs in `.mj/agents/e2b42a643e499be2e989ec893c504443/`. These untracked logs are supplementary; all decisive outcomes are recorded above.

Review fixes passed `TZ=UTC mbx test -p brokk-mj-tui golden_dashboard_dialogs` (1 passed, 627 filtered) and `mbx clippy -p brokk-mj-tui --all-targets -- -D warnings`. The expected golden did not change. Supplementary logs are `rollover-golden-test.log` and `rollover-clippy-passed.log` in the TUI child's report directory. Passed backend checks were reused.

Feature commits are ef4afc441 and 6e0fc708f. Upstream advanced to 8f19b312c during review; it merged without conflicts in 0a7e71dd3. The push hook recognized the clean merge of reviewed work, and origin/master accepted 0a7e71dd3. No tests were repeated for the upstream-only merge.

## Interfaces and Dependencies

Use the existing MouseEvent.modifiers, KeyModifiers, Form hit testing, BTreeSet, serde, reqwest query serialization, Axum extraction, SQLite report loader, and quota-history estimator. Add `SessionUsageQuery.hour_starts_ms: Option<BTreeSet<i64>>` with string serialization suitable for an HTTP query. Preserve `hour_start_ms`. Share normalization/interval interpretation between consumers and keep routine helper details near existing usage types. No new dependencies or crate are expected.

Revision note: Initial plan records the agreed behavior and ownership before implementation.

Revision note: Both implementation slices are ready; one build owner now runs the focused validation. Shared interpretation merges adjacent intervals and quota reads one bracketing history to avoid rounding or coverage changes at artificial hour boundaries.

Revision note: All targeted tests, affected-crate clippy, feature formatting and diff checks passed. Record the narrow-modal fixture constraint and the unrelated whole-workspace formatting limitation; proceed to commit and required review.

Revision note: Initial review accepted two interaction fixes. Use the existing request generation to reconcile rolling chart selection and restore Enter's prior keyboard-cursor behavior; rerun only the affected TUI checks.

Revision note: Both accepted review fixes pass the focused TUI golden and clippy. The existing form selection is updated immediately so the next key event is correct even before redraw.

Revision note: Record final review disposition and successful origin/master delivery. This final plan-only update changes no implementation or validation result.
