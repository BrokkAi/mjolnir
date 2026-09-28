# Show banked resets and consistent quota countdowns

This ExecPlan follows `.agents/PLANS.md` and must be maintained during implementation.

## Purpose / Big Picture

Claude and native OpenAI Codex profiles show positive provider-reported reset balances beside weekly reset times, for example `5d 7h [1]`. Custom providers using Codex do not receive a balance. Web quota times use the same abbreviations as the terminal and advance without waiting for a provider poll.

## Progress

- [x] (2026-09-28) Inspect provider protocols, quota transport, TUI formatting, and browser clock updates.
- [x] (2026-09-28) Implement provider counts and backward-compatible quota transport.
- [x] (2026-09-28) Share Rust formatting and add browser formatting with common test cases.
- [x] (2026-09-28) Validate Rust and web behavior, review, and prepare the final commit on the current branch.

## Surprises & Discoveries

Claude's installed client reads reset grants from the `cedar_ember` block of `/api/oauth/usage?cedar_ember=1&skip_spend=1`. A read-only check using `claude-cli/2.1.283 (external, cli)` returned an eligible grant with `resets_left: 1`; a different User-Agent returned an ineligible surface. Codex's local protocol exposes `rateLimitResetCredits.availableCount` independently of its quota buckets. Browser quota polls can be ten minutes apart, so server-formatted snapshot text alone cannot provide a live countdown.

## Decision Log

- Decision: Hide zero and unknown counts; show balances in both interfaces. Rationale: user choices during planning. Date: 2026-09-28.
- Decision: Use a shared Rust formatter plus a browser counterpart with common fixtures. Rationale: the user requested matching countdowns; local browser ticking avoids server pushes solely for clocks. Date: 2026-09-28.
- Decision: Keep the existing macOS Claude CLI path, returning unknown when its output lacks reset grants. Rationale: no new credential acquisition or Keychain access is needed. Date: 2026-09-28.

## Outcomes & Retrospective

Implementation is complete. All 58 web unit tests and all 5 deterministic quota browser tests passed outside the sandbox. The first complete Rust suite passed, including isolated startup and upgrade tests. The final-source controller suite (1,954 tests) and TUI suite (838 tests) passed. The final-source full run hit an unrelated 200 ms worker verdict timeout that passed in the first run and then passed on isolated rerun. The isolated package build also exposed a missing explicit Reqwest query feature, now declared by the controller and validated by the successful standalone build. Final `cargo clippy --all-targets -- -D warnings`, formatting, and diff checks passed.

## Context and Orientation

`mj-client/src/quota.rs` defines the shared quota report. `mj-controller/src/claude_usage.rs` and `codex_usage.rs` collect provider data; `quota.rs` selects the provider and constructs reports. `mj-tui/src/render/quotas.rs` currently owns countdown formatting. `mj-controller/src/server_runtime/snapshot.rs` projects reports into browser types defined in `server/viewer_types.rs`. `mj-controller/src/web/viewer.js` renders the quota page and already has a one-second clock timer.

## Plan of Work

Milestone 1 adds an optional unsigned banked reset count to reports and provider parsers. Completion is demonstrated by provider parsing, profile eligibility, cache replacement, and serialization tests. Claude sums remaining grants for eligible accounts, excluding expired or future grants without requiring immediate redeemability. Codex uses its reported total, restricted to profiles whose configuration actually selects native OpenAI. Missing metadata remains unknown; malformed metadata is reported without losing ordinary quota windows. Successful refreshes replace old balances.

Milestone 2 moves pure countdown and suffix formatting to the shared client module. Completion is demonstrated by the shared Rust/JavaScript cases and terminal and browser rendering tests. Send browser windows reset timestamps, the Rust-selected long/five-hour style, and a weekly-only count. Keep text fallback for windows lacking timestamps. The JavaScript equivalent updates only reset text nodes through the existing timer, using the server-adjusted clock. Maintain common JSON examples consumed by colocated Rust tests and browser tests.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir`. Preserve normal mbx/Cargo build storage. Run `cargo test` outside the restricted sandbox and `cargo clippy --all-targets -- -D warnings` on the dev profile. Focused browser validation is `./node_modules/.bin/playwright test quota.spec.js --project=deterministic` from `tests/e2e/web`, plus the countdown unit test. Review `git diff --check` and the final diff, then explicitly stage changed files and commit on the current branch without pushing.

## Validation and Acceptance

Provider tests cover counts, missing and malformed metadata, grant dates, totals versus detail lists, custom providers, and refresh/cache transitions. Serialization tests prove old reports still decode. Shared formatting examples cover days, hours, minutes, zero, expired timestamps, and banked suffixes. Rendering tests prove `5d 7h [1]`, unchanged short-window behavior, narrow layouts, and browser clock movement without snapshots or lost focus. Application invocations, if needed, use `--instance banked-resets-test` and isolated data; never run a new build against default live data.

## Idempotence and Recovery

No schema migration, reset redemption, harness upgrade, or deployment is involved. Existing unrelated files remain untouched. Tests use existing isolated fixtures. Unknown balance metadata must never recover a previously positive balance through reset-time cache merging.

## Artifacts and Notes

Common countdown examples will live under `mj-client/src/quota/` and include both expected display text and explicit time inputs so clock boundaries are deterministic.

## Interfaces and Dependencies

Add `ProfileQuota.banked_resets: Option<u64>` with serde default/omission. Introduce a serialized countdown style in `mj-client::quota`, expose pure formatting there, and extend `ViewerQuotaWindow` with optional reset epoch seconds and balance plus a defaultable style. No new dependency or crate is required.

Initial plan recorded on 2026-09-28 after user approval of the complete plan.

2026-09-28 update: implemented the approved behavior and recorded passing web validation. Custom-provider classification and credentials now use one configuration read; no cache-key change is needed.

2026-09-28 validation update: the standalone worker build includes the controller without workspace-wide feature unification. Declared Reqwest query support explicitly so the new Claude usage parameters compile in that configuration too. No package versions or lockfile entries changed.

2026-09-28 completion: implementation and review are complete. Positive balances appear on weekly windows, browser countdowns tick without rebuilding controls, and custom Codex providers remain excluded. The existing macOS Claude CLI limitation remains intentional. Validation includes a successful full Rust run, final-source quota coverage, the isolated retry of the unrelated worker timeout, final Clippy, and 63 web tests. Changes are committed on the existing branch without pushing.
