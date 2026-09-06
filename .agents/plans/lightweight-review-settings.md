# Make review settings lightweight

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Opening review settings should fetch model and effort choices without inspecting repositories or testing review tools. Reopening the dialog should use cached choices immediately. Users can explicitly refresh and can save locally valid settings while discovery is pending or unavailable. Actual reviews retain their existing validation.

## Progress

- [x] (2026-09-06) Approved design and inspected current readiness and UI paths.
- [x] (2026-09-06) Replace controller readiness with single-worker option discovery and behavior tests; controller suite passes (654 tests, one ignored).
- [x] (2026-09-06) Add dashboard cache, Refresh choices, local save validation, and state tests; TUI suite passes (328 tests, two ignored).
- [x] (2026-09-06) Integrate supervised CLI events and test live in private tmux; seed 519 passes all 79 checks.
- [x] (2026-09-06 23:19Z) Complete Cargo tests, clippy, formatting and final review. Commit and upstream push are authorized publication steps.

## Context and Orientation

Before this change, `mj-controller/src/hel_review_settings.rs` started review adapters and also captured repository deltas and verified tooling across workers. It now performs choice discovery on one worker. `mj-tui/src/review_settings.rs` owns the editable dialog; `mj-tui/src/lib.rs` owns dashboard state and the cache. `mj-cli/src/dashboard/actions.rs` launches background work and `io.rs` delivers asynchronous replies. Each request has a generation number so obsolete replies cannot overwrite newer edits. Help wraps the underlying dialog in another Mode and allows matching replies through.

## Plan of Work

First replace readiness with discovery using only one connected active session worker. Prefer the selected session, otherwise sort session IDs. Stage the chosen profile without review MCP servers, start its adapter, obtain models, and apply an explicitly supported model only to obtain its effort choices. Do not apply effort, inspect repositories, validate reviewer assignment, or invoke tools. Send choices before bounded cleanup, retaining successful data if cleanup fails.

Next store successful choices in DashboardState by profile and optional model. Invalidate entries when that profile definition changes. Opening and profile/model changes reuse cache or request discovery; tier and effort changes do neither. Refresh clears that profile's cache and starts one request. Keep configured unsupported values visible. Save requires local configuration validity only; absence or failure of discovery does not itself block saving. Disabled review can save drafts.

Finally integrate CLI request and reply identities without effort. Keep cancellation and supervised cleanup. Exercise real dialogs and count fake adapter requests in the existing private tmux harness.

## Interfaces and Dependencies

Controller exposes ReviewDiscoveryRequest with profile, optional model and preferred_session; ReviewCapabilityChoices with model_choices, effort_choices and effort_capabilities_discovered; ReviewDiscoveryOutcome is Available with choices and optional cleanup_warning, or Unavailable. discover_review_settings takes session control, request, cancellation flag and a progress sender. TUI exposes corresponding ReviewSettingsChoices and ReviewSettingsDiscoveryResult. Dashboard actions DiscoverReviewSettings and CancelReviewSettingsDiscovery replace probes; replies carry generation/profile/model only. An unsupported explicit model still supplies known models but does not claim known efforts.

## Milestones

Controller completion is demonstrated by fake worker request sequences containing Start and Pause only, single-worker selection, cancellation cleanup and retained choices on cleanup failure. Dashboard completion is demonstrated by tests of cache hits, invalidation, Refresh, stale replies under Help, and Save during failed or pending discovery. Integration completion is demonstrated by live tmux navigation and the full required checks.

## Concrete Steps

From `/home/jonathan/Projects/hel4`, run `cargo test` outside the restricted sandbox, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all -- --check`. Build the live binaries with `cargo build -p brokk-mjolnir -p brokk-mj-worker`. Run `python3 tests/e2e/tui_components_tmux.py --seed 519` with elevated permissions and logs under target. Inspect its live-evidence.json and fake ACP request log. Stage only changed files, commit to hel4, and push to its upstream origin/master as explicitly authorized.

## Validation and Acceptance

Cold opening loads choices with a spinner; cached reopening performs no adapter discovery. Profile/model changes fetch once on a miss; tier/effort edits do not fetch. Refresh fetches once. Help and cancellation do not lose valid replies or accept stale replies. Save works while loading, unavailable, or failed, but enabled known unsupported selections are flagged. The existing Create, Resume and other dialog tests continue to pass.

## Idempotence and Recovery

Cache is memory-only and scoped to the dashboard. No data migration is needed. Tests use isolated temporary runtime directories and a private tmux socket, leaving personal sessions untouched. Retry a failed test only after inspecting its evidence; do not remove working files of a running process.

## Surprises & Discoveries

The existing adapter Start call reuses its process for model configuration in the same generation; repeated Start is not necessarily a new process. Current readiness performs repository and tooling work that settings do not need.

The worker already allows ten seconds for cleanup. A controller cleanup deadline must allow that operation to finish instead of cancelling its connection early. Progress and cleanup need separate UI state so retaining old choices during Refresh does not hide the loading spinner.

The first controller test run exposed a fixture race: RemoteSessionPublisher::publish queues an update without waiting for the managed session view to change. Worker-selection fixtures now wait for the expected connected state before asserting preference. Repeated pointer selection emits Select even for the current tab, so profile/model handlers explicitly compare values before clearing choices or requesting discovery.

## Decision Log

Use a separate effort-known flag because an unsupported selected model still yields useful model choices without validating efforts for that model. Publish success before cleanup, and report cleanup warnings separately so useful choices remain usable. Cache only accepted current-generation replies to prevent cancelled work from repopulating cleared entries.

## Outcomes & Retrospective

Implementation and validation are complete. Live seed 519 passed 79 checks, including adapter request counts, cached reopen/model navigation, Refresh under Help, immediate Save during Refresh, offline Refresh retaining choices, and offline Save dismissal. Evidence is in `target/reliability-artifacts/tui-components-seed-519-3412378/live-evidence.json`; CLI SHA-256 is `6fddeb49a422b9f1d13b518c46c48f301d4761765c8e7729696ac91e00f0853f`. Full Cargo execution passed chat, controller, core and TUI, then an unrelated worker harness lease cleanup test failed. The full worker suite passed on retry, and CLI tests, workspace documentation tests, clippy with warnings denied, formatting, Python compilation and diff checks passed. Logs are retained as `target/lightweight-review-*.log`.

Initial plan recorded for the approved implementation; prior readiness work remains documented in palette-review-discovery.md.

Updated after implementation and live acceptance to record test evidence and the remaining validation steps.
