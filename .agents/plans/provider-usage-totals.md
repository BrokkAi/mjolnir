# Show provider totals in the usage report

This living ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

The terminal's Usage by session dialog currently shows one estimated API-dollar total above its hourly graph. Add provider totals immediately beneath that total so the user can see which provider accounts produced the usage. The user also requested percentages of quota; the calculation must name what it measures because dollars and provider quota units are different quantities.

## Progress

- [x] (2026-11-25 04:32Z) Locate the dialog, accounting report, per-turn profile attribution, provider discovery, and existing quota representations.
- [x] (2026-11-25 04:32Z) Ask which percentage is intended: quota consumed in the displayed period, current overall quota used, or share of dollar usage.
- [x] (2026-11-25 04:38Z) Add per-profile usage totals to the shared report and store, preserving filtering and unknown/partial usage.
- [x] (2026-11-25 04:50Z) Render provider totals immediately below Total, preserving short and narrow layouts and interactions.
- [x] (2026-11-25 04:50Z) Implement the chosen interval-consumption percentage and weekly/all-account-use label.
- [x] (2026-11-25 04:50Z) Validate affected store/API boundaries, terminal goldens, and clippy using mbx in the dev profile.
- [x] (2026-11-25 04:36Z) User chose quota consumed during the displayed 24-hour interval or selected hour.
- [x] (2026-11-25 04:38Z) Implement profile totals and additive report fields; backend usage-report tests pass (6 store, 1 server) and core/store/server clippy passes.
- [x] (2026-11-25 04:50Z) Measure observed weekly quota consumption over that interval and display the pooled provider percentage.
- [x] (2026-11-25 04:51Z) Review the integrated diff, commit on the current branch, and request pre-push review.
- [x] Address the three accepted review findings: partial quota coverage, weekly boundary readings, and tall-dialog provider height; targeted tests and affected clippy pass.
- [ ] Complete pre-push review of the fixes.
- [x] Address second-review quota details: weekly history bounds, stale reset readings, and the inclusive last second; affected backend tests and clippy pass.
- [ ] Push the reviewed implementation to origin/master.

## Surprises & Discoveries

The parent session's profile does not identify all usage in its child tree. The store already reads the retained profile for every recorded turn, so totals must be accumulated there. See `mj-store/src/database/usage_report.rs`, `REPORT_TURNS_SQL` and `load_with`.

The provider lookup for Codex and OpenCode can read files. Rendering must use the dashboard's existing off-thread `ProviderDiscovery` results, never call `HarnessProfile::quota_provider()` itself.

Provider quota limits are in native units, and subscription price is a weighting proxy rather than an API-dollar capacity. There is no supported direct conversion between the cost total and quota percentage. The read-only investigation is in `.mj/agents/634d873827897706db6f1162cafd47c2/report.md`; its essential conclusions are recorded here so this plan does not depend on that untracked file surviving.

## Decision Log

Decision: Add profile-level totals from retained per-turn attribution, then group by the configured quota provider in the terminal. Rationale: child trees can use several profiles, and the terminal already owns cached provider discovery. Date/author: 2026-11-25, parent agent.

Decision: Preserve unknown and partial accounting rather than present incomplete costs as exact. Rationale: `CostUsdTicks` distinguishes an unknown price from a known lower bound, and existing totals display `?` and `+`. Date/author: 2026-11-25, parent agent.

Decision: Implement provider dollars independently while the percentage definition is pending. Rationale: this is authorized and useful under each interpretation, and avoids inventing a dollar-to-quota conversion. Date/author: 2026-11-25, parent agent.

Decision: Measure quota consumption over the report interval from stored snapshots of unscoped weekly quota, including when an hour is selected. Pool accounts using subscription-price weights consistent with the existing quota pools, including idle accounts in capacity. Label the result as estimated observed weekly consumption covering all account use. Rationale: the user explicitly chose interval consumption. Probes include outside usage and have sampling gaps, so this is an account measurement rather than attribution of the displayed dollars. Date/author: 2026-11-25, parent agent.

Decision: The daemon's `quota_usage` list owns the account membership for measurement. Include idle enabled subscription profiles and unavailable subscription readings, exclude known usage-priced API profiles, and apply the selected profile filter there. Rationale: mixing API accounts into a provider's subscription capacity would turn a measurable percentage into unavailable data; rebuilding membership from current terminal configuration could diverge from the report. Weekly means an unscoped seven-day duration, or a recognized weekly label when duration is absent. Other periods are unavailable in this feature rather than mislabeled weekly. Date/author: 2026-11-25, parent agent.

Decision: Carry incomplete time coverage as `ProfileQuotaUsage.partial` and show `(partial)` on a pooled percentage if any member has incomplete coverage. Use one shared Rust weekly-reading interpretation for both interval measurement and boundary lookup. Derive provider-summary height from the actual header, chart, gaps, footer, and one reserved detail row. Rationale: independent review found that approximation alone did not distinguish a partly observed interval, short-only probes could hide weekly boundary readings, and a fixed height reserve did not account for taller charts. All three findings are accepted. Date/author: 2026-11-25, parent agent.

Decision: Decline per-turn historical provider identity machinery. Provider grouping uses the current configuration of the retained profile, as existing quota pools do. Reconfiguring a profile to another provider within the displayed interval can therefore reattribute earlier turns. Rationale: retained records do not contain historical provider identity; adding it would expand this display request into a persisted accounting-format change. Record this limitation for a separate accounting feature. The second-review finding was explicitly overridden before any subsequent commit. Date/author: 2026-11-25, parent agent.

Decision: Restrict weekly boundary searches and interpolation to one actual weekly allowance period, seven days, on each side of the displayed interval. Treat a probe whose known weekly reset has already elapsed as unreadable in the shared weekly-reading interpretation. Convert the report's inclusive end to an exclusive end before reducing timestamp precision. Rationale: no-weekly profiles otherwise scan their entire lifetime, observations separated by multiple weekly cycles cannot resolve intervening consumption, stale reset counters do not establish a current allowance, and flooring an inclusive hour end discards its final second. These changes need no migration, index, cache or new daemon state. Date/author: 2026-11-25, parent agent.

## Outcomes & Retrospective

Implementation and validation, including first-review fixes, are complete. The initial implementation was committed as `c6d8678d0`. The dialog shows provider dollars followed by estimated weekly quota consumed during its selected interval, including an individual clicked hour. The label states that quota includes all account use. Unknown pricing stays unknown, partial dollar totals keep `+`, and missing quota history displays `n/a`. Incomplete quota history is explicitly marked `(partial)`, including when an idle account's reading makes the pooled percentage partial. Idle subscription profiles contribute their capacity to the weighted provider measurement. Boundary lookup shares the consumer's definition of a readable weekly probe. Provider height derives from the actual header, chart, hints and footer geometry, retaining at least one detail row even when summaries overflow. Account aliases are counted the same way as the existing quota pools; this feature does not introduce account deduplication. Sampling, reset losses, and interpolation make quota percentages estimates rather than dollar attribution. Final review and delivery remain.

## Context and Orientation

`mj-core/src/usage/report.rs` defines the report serialized between the daemon, which serves accounting reads, and clients. Costs are unsigned integers in ten-billionths of one USD, or `UnknownCost` when pricing is unavailable. `add_cost` checks overflow and propagates an unknown component. `mj-store/src/database/usage_report.rs` reads retained turn usage and builds this report. An optional selected hour narrows the rows, while the graph still shows its 24 clock hours. Profile and suspended-session filters apply to the report.

`mj-tui/src/session_usage_report.rs` renders the terminal dialog. Its Total line is followed immediately by a clickable graph and a scrolling breakdown of agent roles and session trees. Provider rows will be inserted between Total and that graph. `mj-tui/src/render/quota_pools.rs` uses `DashboardState.quota_providers` to map configured profiles to providers without file reads on the render loop. Missing or unresolved mappings must remain visible and honestly labeled.

`mj-tui/src/dialogs/tests.rs` constructs the dialog scenarios, with whole-screen expected output in `mj-tui/tests/golden/dashboard-dialogs.txt`. These are golden tests: they compare the actual terminal rendering to checked-in text. Existing store tests cover mixed-profile trees and filtering. `mj-server/src/server/api/tests.rs` and `mj-cli/src/api_client.rs` also construct the shared report and may require updated fixtures.

## Plan of Work

First extend `SessionUsageReport` with a serde-defaulted `profile_usage` list and define `ProfileUsageEstimate` with the retained optional profile, cost, and per-profile coverage needed for the lower-bound marker. Accumulate these entries in `load_with` alongside the existing role totals, only for turns included in the report's row time range. Reuse accounting helpers and existing coverage semantics. Extend existing mixed-profile integration coverage to verify correct attribution and filtering.

Then group these entries in the terminal using cached provider discovery. Sum costs with existing unknown/overflow semantics and preserve incomplete coverage. Give unconfigured and unresolved profiles visible labels, sort deterministically, and draw the provider breakdown directly beneath Total. Adjust graph position and row geometry together so click targets and keyboard selection still correspond to what is drawn. Extend existing golden scenarios rather than add feature-example unit tests.

For quota consumption, add a minimal store reader for interval snapshots with bracketing samples rather than an arbitrary lookback or materializing full history. Each boundary cursor walks outward until the caller's shared Rust weekly-reading predicate accepts a probe, so nearby short-only, scoped-only, or unreadable probes cannot hide a useful boundary. Reuse the existing used-percent and reset interpretation in `mj-accounts/src/quota_telemetry.rs`. Sum observed positive deltas of unscoped weekly quota, clip boundary intervals to the displayed interval as an estimate, and handle resets without charging pre-window use to the window. Track measured time portions; if they do not cover the entire interval, mark the measurement partial. Missing usable history must remain unavailable, rather than zero. Fill the report from the daemon's current profile configuration and cache identity on a background blocking task. There is no new polling, persistent state, or migration.

The backend child owns the core report and store usage aggregation and initially owns builds. The quota child owns accounts quota measurement, the store quota-history reader, and daemon/API enrichment. The terminal child owns mj-tui and waits for build ownership to transfer. The parent owns this plan, design, integrated review, commits, review rulings, and delivery. Children do not commit.

## Concrete Steps

Run from the repository root, using mbx for Rust caching and the dev profile. Do not redirect Cargo targets. All Rust tests must run outside the restricted sandbox. The assigned builder will translate these Cargo commands through the installed mbx interface and record exact successful commands and nonzero test counts here:

    TZ=UTC mbx test -p brokk-mj-store usage_report
    TZ=UTC mbx test -p brokk-mj-store quota_history
    TZ=UTC mbx test -p brokk-mj-accounts weekly_consumption
    TZ=UTC mbx test -p brokk-mj-server usage_report
    TZ=UTC mbx test -p brokk-mj-tui golden
    mbx clippy -p brokk-mj-core -p brokk-mj-server --all-targets -- -D warnings
    mbx clippy -p brokk-mj-client -p brokk-mj-store -p brokk-mj-accounts -p brokk-mj-daemon -p brokk-mj-tui --all-targets -- -D warnings

Run affected server/client report tests as needed for the serialized field. Rerun only failures or tests affected by subsequent changes. Do not start a development daemon against live data; any executable scenario requires a named isolated instance and must stop its daemon afterward.

The backend builder ran `mbx test -p brokk-mj-store usage_report` and the affected server HTTP contract test, with 6 store and 1 server tests passing. `mbx clippy -p brokk-mj-core -p brokk-mj-store -p brokk-mj-server --all-targets -- -D warnings` passed. The installed mbx accepts `test` and `clippy` directly, not `mbx cargo`; package names have the `brokk-` prefix.

The terminal child completed remaining validation: 3 quota-history tests, 1 weekly-consumption timing test, 1 server report test, and all 36 terminal goldens passed. Clippy passed over client, store, accounts, daemon, and terminal with all targets and warnings denied. Golden regeneration used `TZ=UTC MJ_UPDATE_GOLDEN=1 mbx test -p brokk-mj-tui golden_dashboard_dialogs`; the final fixture-only scoping correction reran that one affected golden (1 passed). The decisive receipt is `.mj/agents/8568433d9a722742998daf248f1b69d8/validation-receipt.md`, with commands and outcomes preserved in this paragraph. No test daemon or live store was used. Earlier automatic approval refusals in the quota child were resolved by the authorized dedicated validation child; no check remains blocked.

## Validation and Acceptance

The existing mixed-profile tree test must show separate totals for the actual turn profiles even when their parent uses a different profile. The totals must follow selected hour, profile, and suspended filters and preserve unknown pricing and incomplete usage.

Terminal goldens must show provider dollars and the estimated percentage of weekly capacity consumed immediately under Total. Narrow and short scenarios must retain usable chart and row interaction geometry. A known total, an unknown component, mixed providers, and incomplete coverage must be represented in meaningful existing scenarios. Quota timing tests must exercise selected-hour boundaries, resets, gaps, and missing data. A selected profile narrows the accounts measured, while the suspended filter affects dollar rows only; the quota label makes its all-account-use scope clear.

## Idempotence and Recovery

This is an additive report field with a deserialization default; it needs no schema migration or live store edits. Existing daemon payloads without provider totals remain readable. Do not infer provider totals from parent sessions when the field is absent. All edits stay on the current branch, and only changed files are staged. If upstream advances, fetch and merge origin/master rather than rebase.

## Artifacts and Notes

The initial lookup found the literal `estimated API USD` in `mj-tui/src/session_usage_report.rs` and the dashboard dialog goldens. Validation receipts belong in child report directories, with decisive commands and outcomes recorded in this plan before completion.

## Interfaces and Dependencies

Use the existing `SessionUsageReport`, `CostUsdTicks`, `add_cost`, `UsageCoverage`, and `ProviderDiscovery` representations. Add `ProfileUsageEstimate` in `mj-core/src/usage/report.rs` and a serde-defaulted `Vec<ProfileUsageEstimate>` field on `SessionUsageReport`. Add a second serde-defaulted `quota_usage: Vec<ProfileQuotaUsage>`, whose entries contain `profile: String`, `consumed_basis_points: Option<u64>`, serde-defaulted `partial: bool`, and `monthly_price_usd: Option<u32>`. Ten thousand basis points means 100% of weekly capacity consumed. Values can exceed a week's capacity if a reset occurs within the interval. Missing measurements are `None`. `weekly_consumption` in accounts returns `Option<WeeklyConsumption { basis_points, partial }>`. The store's `load_quota_history_bracketing` takes a readable-probe predicate so weekly interpretation remains in accounts. Do not create a crate, provider-name heuristic, alternate pricing system, migration, or render-loop I/O path.

Initial plan recorded 2026-11-25 while the provider implementation and quota feasibility check proceed independently.

Revised 2026-11-25 after the user's interval-consumption choice: specify stored-probe measurement, selected-hour support, weekly capacity denominator, all-account scope, and separate quota implementation ownership.

Revised 2026-11-25 after backend validation and integrated inspection: record successful checks and make daemon report membership authoritative, with API billing excluded from subscription capacity.

Revised 2026-11-25 after final validation: record successful test counts, exact mbx commands, final display semantics, and sampling/account-alias limits.

Revised 2026-11-25 after the first pre-push review: accept partial-coverage, boundary-selection, and tall-layout findings; document shared reading interpretation and explicit coverage metadata.

Revised 2026-11-25 after review-fix validation: 3 quota-history tests, 1 weekly-consumption timing test, 1 server-report test and the affected dashboard-dialog golden pass. The golden includes a 70x30 terminal with twelve long retained-profile summaries that preserves its five-row chart, footer and a visible detail row. Clippy passes over core, client, store, accounts, daemon, server and terminal with all targets and warnings denied. The decisive receipt is `.mj/agents/8568433d9a722742998daf248f1b69d8/partial-validation-receipt.md`; all checks used the earlier exact mbx commands and no broad rerun or live daemon was needed.

Revised 2026-11-25 after the second review: record the declined historical-attribution expansion and the accepted bounded weekly-history and timestamp corrections. Provider grouping remains a view over retained profile names and current provider configuration.

Revised 2026-11-25 after second-review validation: store quota-history tests pass (3), accounts weekly-consumption timing test passes (1), and client/store/accounts/daemon all-target clippy passes with warnings denied. The unchanged terminal interface required no golden rerun. The decisive receipt is `.mj/agents/8568433d9a722742998daf248f1b69d8/round2-validation-receipt.md`. Boundary SQL now limits searches to one documented weekly period per side, stale expired weekly readings are excluded by the shared predicate, and inclusive millisecond report ends are converted to exclusive seconds so the final second is measured.
