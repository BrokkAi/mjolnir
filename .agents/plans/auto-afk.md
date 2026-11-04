# Add lightweight automatic AFK

This ExecPlan is maintained under `.agents/PLANS.md`. The daemon is the background controller shared by the terminal user interface (TUI) and web user interface (WUI). AFK means the operator is away; existing AFK behavior asks agents to continue useful work rather than immediately waiting on the operator.

## Purpose / Big Picture

Users can configure automatic AFK after a whole number of minutes without input in either interface. Input in any connected interface resets one shared inactivity timer. Automatic AFK uses the existing explicit return action: scrolling or typing while AFK is visible does not dismiss it. Returning explicitly starts a full new interval. The feature defaults off and activity is never recorded in the database.

## Progress

- [x] (2026-10-27) Investigated existing AFK ownership, input routing, configuration and tests; obtained user decisions.
- [x] (2026-10-27) Assigned disjoint implementation slices for setting, daemon, terminal and web reporting.
- [x] (2026-10-27) Implement settings and transport contracts.
- [x] (2026-10-27) Implement and validate browser activity reporting.
- [x] (2026-10-27) Implement the shared in-memory timer and terminal reporter; terminal coalescing/gating tests pass (2/2).
- [x] (2026-10-27) Review integrated behavior, concurrency, and lightweight operation; both timing findings corrected and independently rechecked.
- [x] (2026-10-27) Run full touched-crate Rust suites, workspace Clippy, and format checks; only two stale settings goldens need correction.
- [x] (2026-10-27) Correct two golden fixtures and rerun only their failing tests plus unreached doctests; all passed.
- [x] (2026-10-27) Prepare the validated result for commit on the current branch and delivery to `origin/master`.

## Surprises & Discoveries

The TUI has an AFK modal while the WUI has an AFK indicator and explicit End AFK action. Both already use the same daemon-owned durable presence. Neither currently transports generic human activity. Existing `set_operator_away` serializes persistence through `config_mutation` and then synchronizes workers; that expensive path must not run for activity signals.

Independent review found two timing defects in the initial draft: explicit return reset the deadline before the database commit, and a queued TUI generation could survive a disabled/away boundary. Reset only after successful commit and discard pending generations at producer-side gating boundaries, including boundaries coalesced by the watch channel. Reusing the TUI connection also avoids acquiring startup file locks for every report.

## Decision Log

Decision: Use `advanced.auto_afk_minutes: u32`, with zero disabled and omitted when saving defaults. Rationale: a single existing numeric settings field is enough to enable and configure the feature. Date: 2026-10-27.

Decision: Keep timestamps and deadlines exclusively in memory and start a full interval after daemon startup. Rationale: the user explicitly requires a lightweight feature with no database recording of activity. Existing AFK persistence remains unchanged. Date: 2026-10-27.

Decision: Keep explicit dismissal for manual and automatic AFK. Rationale: the user wants the existing return interaction. A successful AFK-off transition starts a new inactivity interval. Date: 2026-10-27.

Decision: Count keys, paste/input and all delivered mouse/pointer input, including motion and scrolling, across interfaces. Ignore background feeds, rendering, resize and passive visibility changes. Coalesce before transport with prompt leading delivery, at most one report per second, and a trailing report. Rationale: honor any user input while bounding work independently of input frequency; approximately one-second timing granularity is acceptable. Date: 2026-10-27.

Decision: Commit on the current branch and push its configured upstream after validation. Rationale: repository rules require committing completed changes and the user explicitly requested pushing when complete. Date: 2026-10-27.

Decision: Integrate upstream before final validation and use daemon protocol 65. Rationale: four concurrent upstream commits already use protocol 64 for daemon-owned session names. The current branch was fast-forwarded to `07044810f`; saved auto-AFK changes were reapplied and both protocol comments and all existing management transcripts were retained. No branch was created or changed. Date: 2026-10-27.

Decision: Merge upstream after the first push was rejected and advance the combined protocol to 66. Rationale: concurrent project-removal changes already use protocol 65. Preserve both actions and update the inactivity coordinator in the new project-removal config-install path. Validate the affected daemon/protocol paths without repeating unrelated completed suites. Date: 2026-10-27.

## Outcomes & Retrospective

Implementation and validation are complete and accepted. Full WUI validation passed: 57 Node tests and 113 deterministic Playwright tests, with three unconfigured lab cases skipped. Existing test dependencies were installed locally without tracked package changes. Chromium requires running outside the restricted sandbox on this host. No build storage or configuration was changed. No live daemon, store, or instance was used for testing this change.

The lock investigation confirmed normal active compilation under mbx's managed target directory, not a stale lock. Targeted TUI coalescing/gating tests pass (2/2), final source review found no remaining blocker, and full Rust validation ran sequentially after all source changes settled.

Integrated protocol-65 validation completed: core, client and CLI full suites passed; controller passed 2159 tests with 17 ignored and one stale settings golden; TUI passed 563 with two ignored and one stale settings golden. Workspace Clippy, formatting and diff checks passed. All three backend auto-AFK tests, HTTP activity transport and frozen management compatibility tests passed. The only corrections are expected help text occurrences and TUI setting row ordering/padding; rerun only these two comparisons and doctests not reached after the library failures. No implementation source changed after final validation.

Both corrected settings golden comparisons passed their targeted reruns. Controller and TUI doctest targets also completed successfully (neither contains doctests). All touched-crate suites therefore pass, using the original full runs plus only the two failing-case reruns. No successful suites or Clippy were repeated for fixture-only corrections. The parent reviewed the final fixture diff and integrated source before acceptance.

After the concurrent upstream merge, protocol-66 checks also passed: three auto-AFK controller tests, project-removal RPC, frozen management compatibility, the full client suite (42 unit and one integration), two CLI reporter tests, workspace Clippy, formatting and diff checks. Independent review confirmed the new project-removal config-install path shares the same clock owner and changes the interval only when its configured value changes.

## Context and Orientation

Configuration types live in `mj-core/src/config/ui.rs`; editable shared settings live in `mj-client/src/settings/schema.rs`. The terminal event loop is in `mj-cli/src/dashboard.rs` and batches ready events, so capture must occur for every event before routing consumes it. Browser input is handled in `mj-controller/src/web/viewer.js`; document capture listeners must see input consumed by nested controls.

The daemon's runtime state and lifecycle live under `mj-controller/src/daemon/`. `mj-controller/src/daemon/snapshot.rs::RuntimeState::set_operator_away` persists the actual AFK transition and publishes it to clients and workers. Local commands use `mj-client/src/daemon.rs::DaemonAction`; browser actions pass through `mj-controller/src/server/` and `server_runtime/`. Browser snapshot fields are defined in `server/viewer_types.rs`.

## Plan of Work

First expose the timeout through the existing configuration and shared settings machinery. Extend existing serialization and settings integration/golden coverage rather than adding tests that restate a default. A new configuration is disabled. Zero disables an existing timer without automatically leaving AFK. Enabling or changing a timeout starts a fresh interval, visible to both clients without extra polling.

Next add `DaemonAction::RecordOperatorActivity` and the authenticated browser action `record-operator-activity`. Add `auto_afk_minutes` to the viewer snapshot; the TUI can use its existing configuration feed. Bump only the daemon protocol and preserve the frozen management handshake. These actions update the same in-memory owner and do not write the database, publish snapshots, or synchronize workers.

Implement one supervised, cancellable background timer alongside the runtime. One owner serializes activity, configuration changes, explicit transitions and expiry decisions. Recheck expiry under that ownership before performing the existing AFK transition; a stale timer must not undo an accepted activity reset. Do not hold an independent copy of presence. Use the existing AFK transaction and worker delivery on an actual transition, avoid recursive locking, and keep expensive delivery outside the timer decision where feasible. Timer waiting never holds upgrade admission. Actual automatic transitions use labeled deferrable admission and report failures.

Finally add terminal and browser reporters. Each is a bounded single-flight background sender with a dirty/latest indication, prompt leading report and at most one-second cadence plus trailing report. No input path awaits network work and no input event spawns a separate task. No reports occur when disabled, already AFK, or idle. A failed background send is reported with useful context without flooding or building retry queues. AFK input never automatically turns AFK off.

## Concrete Steps

All commands run from the repository root. Keep the existing Cargo/mbx configuration and build output locations. During implementation run only tests affected by that round. Run every `cargo test` outside the restricted sandbox. After the final changes, run full suites for touched crates, for example:

    cargo test -p brokk-mj-core -p brokk-mj-client -p brokk-mj-controller -p brokk-mjolnir -p brokk-mj-tui
    cargo clippy --all-targets -- -D warnings
    npm test --prefix tests/e2e/web

These package names are confirmed against Cargo manifests. Preserve isolated test configuration/data directories and use `--instance auto-afk-test` for any new binary/daemon/TUI invocation. Never aim a test build at the default instance. Run format checks and `git diff --check` after integration.

## Validation and Acceptance

Tests must demonstrate activity from multiple clients resetting the same timer; expiry only after no accepted activity; input racing expiry; explicit return granting a full interval; runtime setting enable/change/disable; and a new daemon starting a fresh timer while retaining existing AFK state. Integration coverage must verify activity requests do not change database state or emit snapshot revisions. Browser coverage must exercise trusted global input, synthetic-input exclusion, coalescing and disabled/away gating. Preserve existing explicit dismissal and settings golden coverage.

A manual isolated scenario, if needed after automated checks, configures one minute, interacts with the TUI and WUI, observes no AFK while either is used, then observes existing AFK UI after inactivity. Explicit return removes AFK and starts a full interval. Stopping or upgrading the daemon remains responsive. Record decisive validation results here; only rerun failures after fixes.

## Idempotence and Recovery

No database migration or activity history exists. Setting zero disables the feature, and removing the new setting restores existing manual-only behavior. Preserve unrelated edits, stage only changed files, and commit on the existing branch. Push to its configured upstream; if rejected for remote movement, fetch and merge upstream without rebasing, resolve conflicts, and push the merge commit.

## Artifacts and Notes

Implementation child reports provide detailed per-slice evidence; the parent reviewed and accepted the final result. Validation summaries are recorded above. Git history and the final upstream comparison record delivery; the commit subject is `Add automatic AFK after configurable UI inactivity` and the authorized destination is `origin/master`.

## Interfaces and Dependencies

Use existing Rust async runtime, subprocess/build helpers and authenticated transport machinery. New contracts are `advanced.auto_afk_minutes: u32`, local `RecordOperatorActivity`, browser `record-operator-activity`, and viewer snapshot `auto_afk_minutes: u32`. No worker protocol change or new dependency/crate is needed. Activity never reaches the database, relay journal, per-event logging, or worker synchronization path.
