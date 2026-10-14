# Add Profiles usage by session

This ExecPlan follows `.agents/PLANS.md` and remains a living record of implementation and validation.

## Purpose / Big Picture

Users can open Profiles → Usage by session in the terminal dashboard and compare recorded token usage as estimated standard API dollars. Each parent session includes all its sub-agents, even destroyed children whose accounting remains. A profile dropdown starts on All; a period dropdown starts on Hourly and also offers Weekly and Monthly; an unchecked Include suspended sessions checkbox controls inactive parent rows. Each session displays its model breakdown and total, highest total first.

## Progress

- [x] (2026-10-14) Locate the CPU report, durable usage records, retained child accounting, and shared dropdown controls.
- [x] (2026-10-14) Implement report query, shared response types, and published standard price estimates, with no schema changes.
- [x] (2026-10-14) Add the Profiles command and cancellable background loading with generation checks and serial 30-second refreshes.
- [ ] Validate database accounting and HTTP filters (tests written, compiling); rendered output, filter interactions, and stale response handling passed the dialog golden, with a final rerun pending after scroll improvements.
- [ ] Run full tests for touched crates and workspace clippy, commit on hel4, and push to origin/master.

## Surprises & Discoveries

The reference report exists in the TUI as CPU by session. The existing usage store records model selections at turn start, excludes incomplete reports from totals, and retains subagent_accounting after operational child records are deleted. Claude input counts exclude caches, whereas the other adapters include caches in input; pricing must respect that distinction.

## Decision Log

The three periods mean trailing one hour, seven days, and thirty days, measured at query time. Profiles select the parent session's last profile; its whole descendant tree remains aggregated, including children using other profiles. All sums every listed root exactly once. This matches a session report and works with retained accounting that does not retain destroyed children's profile identity. The dialog states this interpretation. Standard token API rates are embedded with dated primary-source references, rather than adding a network dependency to opening the report. Unrecognized prices and missing usage remain visible as partial estimates. No schema or relay migration is required.

## Outcomes & Retrospective

The feature compiles, and workspace clippy passed with warnings denied. The expanded dashboard-dialogs golden passed the actual Profiles menu, profile and period dropdown interactions, suspension checkbox, load failure, and obsolete-response checks. The first retained-tree store integration test passed. Final store and HTTP tests, full suites, and commit/push remain in progress.

Both final usage integration tests and both dialog/menu goldens passed. A fetch found this branch 42 commits behind origin/master (including releases from 2.41.0 to 2.45.1). The validated feature is committed as a checkpoint before merging upstream on hel4. The superseded full-suite compilation is stopped so final validation can run against the merged result.

## Context and Orientation

`mj-controller/src/database/usage.rs` reads `session_turn_usage`, `session_turn_selections`, and retained `subagent_accounting`. `mj-core/src/storage.rs` owns usage response shapes. `mj-controller/src/server/api/turns.rs` and `subagent_backend.rs` expose background store reads to authenticated clients; `mj-cli/src/api_client.rs` is the terminal client. `mj-tui/src/session_cpu_report.rs` and `dialogs/render.rs` provide the reference modal. `mj-chat::components::ComboBox` supplies shared keyboard and mouse dropdown behavior. `mj-cli/src/dashboard/help_search.rs` demonstrates a task cancelled when the dialog changes; request generations prevent old responses replacing newer state.

## Plan of Work

First add a shared request and response contract for a report of parent sessions with token totals grouped by model and estimated USD ticks (integer fractions of a dollar). Implement one consistent read transaction that selects parent sessions and recursively gathers retained descendants, filters completed turns by timestamp, reuses model attribution, and sorts priced totals. Expose the report through a read-only HTTP route backed by a blocking-pool store read. Keep blocking work away from the terminal event loop.

Next add a Usage by session command to the Profiles title menu and command palette. Implement a modal with two shared dropdowns and a checkbox, immediately show loading, and retain session selection by identity while results refresh. The terminal owns one cancellable request task keyed by a monotonically increasing generation. Closing the modal or changing filters cancels the request, and responses must match the current generation. Provide Refresh and Close controls and report failures in the dialog and notice log. Document the filters and estimate limits in the existing human-facing `docs/src/content/docs/profiles.md` page.

Finally add integration coverage for time boundaries, incomplete reports, deleted children, model changes, suspension and profile filters, plus a TUI golden covering real rendering and keyboard transitions. Run targeted tests while developing, then the full suite for each touched crate once and workspace clippy. Review only owned diffs and commit on the current hel4 branch; push its configured origin/master upstream, merging upstream first if the push is rejected.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir4`, use normal mbx-wrapped Cargo without changing its target layout. Run cargo fmt --all -- --check, then focused cargo test filters and finally cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-tui -p brokk-mjolnir and cargo clippy --all-targets -- -D warnings. All cargo test invocations must run elevated. Any binary exercise must use --instance profile-session-usage and isolated configuration and data directories.

## Validation and Acceptance

Opening the Profiles menu reveals Usage by session. The dialog initially says Loading usage and shows All and Hourly. Recorded parent and child usage renders model lines and estimated dollar totals descending. Selecting a profile limits roots by parent profile; selecting Weekly or Monthly expands the trailing period; Include suspended sessions adds inactive roots. Destroyed child usage remains included once. Unknown rates show an unpriced marker, and missing reports show partial coverage instead of invented zero costs. A failed load offers Refresh, and Escape stays responsive during requests. A response from an older filter choice cannot replace the newest report.

## Idempotence and Recovery

All report operations are read-only. Repeat tests with their existing isolated stores. Cancel background reads by dropping their task owners; store work already dispatched may finish but cannot modify anything. Stage only files changed for this task, never unrelated working tree changes. No branch, live instance, installation, or release changes are authorized or needed.

## Artifacts and Notes

Primary rate sources checked on 2026-10-14: developers.openai.com/api/docs/pricing, platform.claude.com/docs/en/about-claude/pricing, docs.z.ai/guides/overview/pricing, api-docs.deepseek.com/quick_start/pricing/, and docs.x.ai/developers/models. Estimates use standard base token rates and default cache write duration, excluding request-specific premiums and tool charges.

## Interfaces and Dependencies

Use existing serde, rusqlite, chrono, tokio, reqwest, ratatui, and mj_chat components. Add no crate or new dependency. Define report request/response types in mj-core and a load_session_usage_report function in the controller store. HTTP GET /api/v1/usage/report exposes the report; the terminal loads it using ApiClient in a supervised, abort-on-drop task.

Revision note: initial plan records the user's default All profile, model breakdown, total-descending ordering, and existing TUI reference.

Revision note: implementation is complete, first compile and dialog golden passed; supervised loading reports reader panics/timeouts, and selectable model lines keep long breakdowns scrollable. Kimi standard rates were read from its primary Markdown pricing table because the web extractor omits its embedded table.

Revision note: workspace clippy passed, and human-facing profile documentation now explains the report. Integration coverage also ensures missing cache attribution cannot silently be priced as zero cache reads.

Revision note: upstream integration is necessary before publication. Keep hel4, commit the validated checkpoint, merge origin/master without rebasing, resolve only overlap, and perform final validation on the resulting code.
