# Add Profiles usage by session

This ExecPlan follows `.agents/PLANS.md` and remains a living record of implementation and validation.

## Purpose / Big Picture

Users can open Profiles → Usage by session in the terminal dashboard and compare recorded token usage as estimated standard API dollars. Each parent session includes all its sub-agents, even destroyed children whose accounting remains. A profile dropdown starts on All; a period dropdown starts on Hourly and also offers Weekly and Monthly; an unchecked Include suspended sessions checkbox controls inactive parent rows. Each session displays its model breakdown and total, highest total first.

## Progress

- [x] (2026-10-14) Locate the CPU report, durable usage records, retained child accounting, and shared dropdown controls.
- [x] (2026-10-14) Implement report query, shared response types, and published standard price estimates, with no schema changes.
- [x] (2026-10-14) Add the Profiles command and cancellable background loading with generation checks and serial 30-second refreshes.
- [x] (2026-10-14) Validate database accounting, HTTP filters, rendered output, filter interactions, scrolling, and stale response handling. Both new integration tests passed on the merged store format, and the final dialog golden passed.
- [x] (2026-10-14) Run workspace clippy with warnings denied on the final implementation; complete the controller suite (1,995 passed and 11 ignored initially, with the one existing deadline test passing alone from the exact suite binary).
- [x] (2026-10-14) Finish full core/TUI/CLI suites: core 500 unit and 3 scenario tests; TUI 522 passes and 2 ignores initially, with its 3 affected command-count/layout goldens updated and passing on exact reruns; CLI 212 unit tests plus all enabled integration/PTY suites passed.
- [x] (2026-10-14) Commit final corrections as 9b3528446, merge the additional upstream commits without changing the usage implementation, and push 3b79cfa13 to origin/master.

## Surprises & Discoveries

The reference report exists in the TUI as CPU by session. The existing usage store records model selections at turn start, excludes incomplete reports from totals, and retains subagent_accounting after operational child records are deleted. Claude input counts exclude caches, whereas the other adapters include caches in input; pricing must respect that distinction.

## Decision Log

The three periods mean trailing one hour, seven days, and thirty days, measured at query time. All sums every listed root exactly once. The dialog states the profile interpretation. (Superseded 2026-10-23: profiles originally selected the parent session's last profile and aggregated its whole tree, because retained accounting did not keep destroyed children's profile identity. A Claude parent whose sub-agents spent a Codex profile's quota then hid that usage from the Codex profile. Revision 93 records each turn's profile at turn start; a profile now counts only its own turns anywhere in a tree, still grouped under the parent, and lists parents of that profile even when idle.) Standard token API rates are embedded with dated primary-source references, rather than adding a network dependency to opening the report. Unrecognized prices and missing usage remain visible as partial estimates. No schema or relay migration is required.

## Outcomes & Retrospective

The report is implemented and validated on upstream 2.45.1. Profiles exposes Usage by session with All/Hourly defaults, parent-profile and inactive-parent filters, complete retained sub-agent aggregation, model token/cost breakdowns, descending totals, cancellation, periodic refresh, and explicit unknown/partial data. Human-facing profile documentation explains its estimate limits.

Both new store and HTTP integration tests passed, as did the dialog/menu goldens. Full core, controller, TUI, and CLI suites ran. The controller's existing mbx deadline test failed under the parallel run and passed alone from the exact suite binary in 2.38 seconds. Three TUI discovery goldens required the expected extra command/count/layout changes; all passed after updating those snapshots. All other enabled tests passed, including CLI upgrade, isolation, and PTY suites. Workspace clippy with warnings denied, rustfmt, and diff checks passed on the final implementation.

Checkpoint 205cd5fa4, upstream merge bca114ec3, and final correction 9b3528446 are committed. Upstream's revision 82 stores semantic turn records containing raw_outcome; the report uses the existing database serialization helpers. A push rejection required a second clean upstream merge, 3b79cfa13; the usage implementation and its validated goldens remained identical across that merge, and the push to origin/master succeeded. No schema change, build-cache layout change, live instance operation, installation, or release was part of this feature.

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

Revision note: adopt upstream's persisted-turn interpretation helper after its schema revision 82 change; no fallback reader or new migration is introduced.

Revision note: distinguish a missing model token total from zero in the rendered breakdown. The final golden covers this partial-data case. Retrying an unchanged timing test uses the exact existing suite binary to preserve the original feature set and avoid redundant compilation.

Revision note: final validation completed; refresh only the three existing discovery goldens affected by the new command, then commit and publish the validated result.

Revision note: the authorized push succeeded after fetching and merging the concurrently advanced upstream. All feature work and validation are complete.

Revision note (2026-10-23): a user saw no `codex2` usage although Codex reported 9% used; 132 of 134 `codex2` turns that day belonged to sub-agents under parents on other profiles. Schema revision 93 (compatible) adds `session_turn_selections.profile`, written from the session's profile at turn start, backfilled from current session profiles, and renamed with profile renames. The report filters turns by it and keeps rows per parent.
