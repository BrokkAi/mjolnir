# Compact web controls, quota overview, and restart notices

This living ExecPlan follows `.agents/PLANS.md` and must be updated as work proceeds.

## Purpose / Big Picture

Make the phone viewer useful at a glance: tap session cards to open conversations, remove empty queue chrome, use a paperclip attachment control, and see all six configured subscription quotas without scrolling. Both terminal and browser conversations should show only the latest of consecutive session-restart notices. Detailed quota information and independent session actions remain available.

## Progress

- [x] (2026-09-05) Read repository guidance and inspected current web quota rendering and live quota data. Seven profile cards (six subscriptions and one API profile) occupy 2,230 pixels at 390px width.
- [x] (2026-09-05) Delegated independent card/composer/queue work and Rust restart-notice presentation work to Luna agents; main owns quota and integration.
- [x] (2026-09-05) Implemented compact quota overview, persistent expansion/focus, and visible refresh failures. Four synthetic Chromium quota tests and 14 Node unit tests pass.
- [x] (2026-09-05) Live data with local asset overrides confirms a 370px quota overview (6.0x smaller), all seven profiles fit at 320x568 and 390x844. Whole-card opening worked; automatic primary-workspace read receipts were blocked.
- [x] (2026-09-05) Reviewed integrated changes and passed focused restart tests, full Clippy, rustfmt check, and two card/paperclip Chromium checks. Corrected CSS to respect image-support visibility. Updated the browser lab to include quota fixtures and use the new card/disclosure interactions.
- [x] (2026-09-05) Full `cargo test` completed successfully, including the long journal-chaos test; full Clippy and formatting passed.
- [x] (2026-09-05) Built the final binary, restarted the authorized shared daemon, and passed all nine layout/quota browser tests against the installed viewer. Read-only live checks confirmed lifecycle wording, card navigation, empty-control hiding, and no adjacent restart entries.
- [x] (2026-09-05) Corrected isolated test provisioning by seeding its own XDG managed-harness cache with the existing Python fake and expected manifest. No Node, real provider, or live credentials are introduced.
- [x] (2026-09-05) Corrected the browser-cache path and stale TUI footer focus probe. The complete isolated browser/TUI lab passes with two clients, one SSE reconnection, zero leaks, and all nine layout/quota tests. Fixture-only corrections are validated for their separate commit.
- [x] (2026-09-05) Committed requested UI/transcript changes on master as `8fbc9d0d`; no push performed.

## Surprises & Discoveries

The configured quota windows have different ordering and some profiles report only Week. Build columns from the reported labels rather than assuming every provider reports both windows. The seventh profile is API billed and must not acquire a fabricated percentage.

Incremental conversation responses need a reset boundary when a newer restart replaces an already delivered notice, even if unrelated older messages remain. The server also applies this boundary to cached projections. Tests must cover a user-message prefix followed by two restart notices and verify that polling after the newest notice no longer resets.

The isolated browser/TUI lab failed before conversation opening: its fixture restricts PATH to fake adapters, but current managed Codex provisioning bypassed the fake adapter and invoked a Node entrypoint. Controller diagnostics reported `env: 'node': No such file or directory`; no fake ACP log was created. This is distinct from the passing installed-live and synthetic browser tests. A correction must keep the adapter fake, not add Node and accidentally run a real provider.

## Decision Log

Decision: Use dense disclosure rows with aligned percentage-remaining columns and one shared set of window labels. Keep reset times, meters, refresh controls, and provider details inside expandable content. Rationale: a 44-pixel touch target for each profile permits all seven rows in roughly 308 pixels, reducing the current overview by about seven times without shrinking controls. Date/author: 2026-09-05, main agent.

Decision: Card text selection is not a goal, per explicit user clarification. Independent action buttons retain their actions; all other card space opens the conversation. Date/author: 2026-09-05, user/main agent.

Decision: Collapse restart notices in presentation, not in durable history. Rationale: preserve event ordinals, read cursors, and journal evidence. Date/author: 2026-09-05, main agent.

Decision: Render durable state `running` as `live` in web lifecycle badges. Rationale: the user observed Quicksilver as both running and idle. Read-only live snapshot confirms lifecycle `live`, state `running`, `is_idle: true`, and activity `[idle]`; the lifecycle describes an alive session, not active work. Keep the wire state unchanged and the activity indicator authoritative. Date/author: 2026-09-05, main agent.

## Outcomes & Retrospective

The requested product changes are implemented and installed in the live viewer. Full Rust tests, Clippy, formatting, 14 Node tests, and nine browser layout/quota tests pass. Live quota height is 370px versus 2230px before, with all seven profiles visible at 320x568. Primary-workspace reads were protected by aborting all API writes, including automatic read receipts. The complete isolated fake-provider browser/TUI workflow also passes after correcting its managed cache setup and stale footer probe. It reports two clients, one SSE reconnection, and zero leaked processes. Refresh an existing web tab for new assets; relaunch an already-running TUI to load its new rendering code. No new push has been requested for this round; the preceding merge and push was completed before this task.

## Context and Orientation

`mj-controller/src/web/viewer.js` renders the browser interface from a server snapshot. `viewer.html` contains the composer and route containers; `viewer.css` supplies phone styles. `service-worker.js` versions cached static assets. `renderQuota` currently creates large cards with usage bars, provider identity, reset text, and refresh controls. Quota data includes named windows (a provider's allowance over a period), percentages used, stale/error flags, and reset text. The new overview will show the complement, percentage remaining, as the terminal does. Do not infer missing readings as zero.

`mj-chat/src/hel_chat/transcript.rs` builds conversation presentation from durable events; `mj-tui/src/ingest.rs` and related terminal rendering consume the shared conversation model. Find the restart projection there and share its presentation behavior across browser and terminal. Do not delete or rewrite persisted events.

`tests/e2e/web` contains Node unit tests and Playwright browser tests. Chromium is installed. The shared running viewer can be authenticated with the current code parsed privately from `./target/debug/mj daemon status`; never print codes or daemon tokens. Primary workspace activity must remain read-only. Browser routes must block API writes, including automatic read receipts, during this audit. No provider task is needed for these changes.

## Plan of Work

First, make `renderQuota` build one disclosure per profile, with shared column labels and an accessible summary naming each value and warning. Preserve expanded rows across snapshot updates. Reuse existing detailed quota rendering inside disclosures. Add CSS with minimum 44-pixel summaries and readable aligned numeric values; allow unfamiliar or long labels to wrap instead of overflowing. Increment the static cache version after integration.

In parallel, implement whole-card navigation with keyboard access and independent nested action controls. Hide empty queue and shell subsections, but keep active-shell control visibility. Replace the composer Images label with an accessible inline SVG paperclip. Investigate the terminal's shell equivalent for the user explanation.

Also implement adjacency coalescing of restart notices in the shared presentation pipeline, retaining the latest notice and preserving notices separated by visible content. Add behavior tests for both surfaces and event boundaries.

## Concrete Steps

From `/home/jonathan/Projects/hel`, run `npm --prefix tests/e2e/web run test:unit` and the focused Playwright spec selected with `MJ_BROWSER_SPEC`. Synthetic browser fixtures should exercise actual rendering functions with local data, without provider authentication or workspace writes. For Rust changes, run `cargo test` outside the restricted sandbox and `cargo clippy --all-targets -- -D warnings`, followed by `cargo fmt --all -- --check`. Run only one Cargo invocation at a time: this NFS build directory does not provide Cargo's expected lock protection. Do not edit build inputs while a Cargo command is running.

Build `cargo build -p brokk-mjolnir` if needed for live deployment after checks. Restart the shared daemon using the already authorized `MJ_DEV_RESTART_STALE_DAEMON=1 ./target/debug/mj daemon restart`, keeping tokens private. Authenticate Chromium privately, block non-read API requests, and inspect quota and card behavior at 320x568 and 390x844. Do not refresh live quotas merely to test a control; validate refresh dispatch with a browser fake.

## Validation and Acceptance

Six subscription rows plus the API row fit on a 390x844 phone with no scrolling when collapsed; target 320x568 too. Every summary is at least 44 pixels high. Percentage labels clearly mean remaining, not used; unknown and failed readings are explicit. Expanding a row exposes every original window, resets, warnings, and refresh action, and updates do not unexpectedly close it. Long identifiers and unusual window labels do not cause horizontal overflow.

Clicking card background, title, or preview opens its conversation; keyboard activation works; nested actions never accidentally open it. Empty queue and shell UI disappears, while actual queued prompts and active shells retain their controls. The paperclip has an accessible attachment name. Consecutive restart notices render once with the latest notice metadata, but an intervening visible entry keeps the separate notices. Required tests and lint checks pass.

## Idempotence and Recovery

Rendering changes require no migration. Repeat tests safely with fixtures and read-only live requests. Preserve unrelated work and stage explicit owned files only. If live deployment fails, report the failure and retain the tested source changes without changing primary sessions. Do not use resets, branch changes, or force pushes.

## Artifacts and Notes

Baseline browser measurement at 390x844: quota container height 2230px. Quota labels seen: Week and 5H. Profiles with no subscription window remain explicit API or unknown rows. Store private live screenshots only in ignored `target/`, not in public tickets or committed artifacts.

## Interfaces and Dependencies

Reuse `ViewerQuota` and `ViewerQuotaWindow` JSON fields, native HTML `details`/`summary`, existing `band` color thresholds, and `refreshRow` request handling. Use no new browser libraries. The shared Rust transcript representation must remain compatible with existing event/read ordinals. Agents must not run concurrent Cargo commands or commit shared-worktree changes independently.

Plan created 2026-09-05 to record this new UI round and its read-only live validation constraints.

Updated 2026-09-05 with passing browser/unit evidence, measured live density, and incremental restart-reset requirement discovered during review.

Updated 2026-09-05 after integration review and focused checks; full Rust suite and installed-binary live validation remain.

Updated 2026-09-05 with full Rust validation success and the user's Quicksilver lifecycle-label finding. The display-only lifecycle wording change passed Node tests and will be checked in the final browser run.

Updated 2026-09-05 with installed-live success and the diagnosed fake-adapter fixture failure, separating product evidence from unfinished integration validation.

Updated 2026-09-05 after committing the product changes and correcting only the isolated fake managed-harness cache setup in `tests/e2e/reliability_lab.py`; integration rerun remains in progress.

Updated 2026-09-05 after final integration success. `tests/e2e/browser_lab.py` now preserves the caller's installed Chromium cache and sizes the terminal to retain its current pane-specific footer, rather than assuming an obsolete footer string at a width that hides it. Final artifact directory: `target/reliability-artifacts/browser-tui-convergence-seed-20260905-1128611`. All requested work and validation are complete.
