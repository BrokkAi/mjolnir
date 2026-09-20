# Make help readable and searchable (#1103)

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture

The keyboard reference should be easy to skim on ordinary terminals and searchable by intent. Opening `?` will show task groups, wrapped descriptions, a persistent prefix explanation and search field. Typing after `/` filters immediately; after 400 ms without edits, Jev evaluates related shortcuts without an index. Offline users retain substring search.

## Progress

- [x] (2026-09-19) Inspected help, component editing, background jobs, and the hosted Jev proxy; agreed the implementation plan with the user.
- [x] (2026-09-20 02:30Z) Implemented shared search contracts and proxy endpoint; TypeScript checks, tests and dry run pass.
- [x] (2026-09-20 02:30Z) Implemented grouped help, text editing, wrapping, and semantic result state; all 23 focused help tests pass.
- [x] (2026-09-20 02:30Z) Connected supervised, cancellable background requests with a 400 ms debounce; focused background tests pass.
- [x] (2026-09-20 02:40Z) Full dev-profile Cargo tests and clippy passed; final TUI suite passed after mouse/Escape refinements. Worker deployed and smoke-tested.
- [x] (2026-09-20 02:40Z) Committed the shared contract and deployed proxy as `7372cef2`; reviewed the final UI changes and prepared the current-branch delivery to origin/master.

## Surprises & Discoveries

Substring filtering already exists in `mj-tui/src/help.rs`. Its unwrapped rows combine bindings, labels, descriptions and availability. Existing archive search debounces at 250 ms, but the user chose 400 ms for this remote lookup. The public proxy currently accepts only turn-verdict evidence.

## Decision Log

The user selected a grouped reference, substring improvements plus semantic search, and extending/deploying the public proxy. Literal matches stay first; at most eight additional semantic results with probability at least 0.70 follow in a Related shortcuts section. This preserves already-visible results. No commands execute from help. The user also explicitly requested pushing the finished work to origin/master. These decisions were recorded on 2026-09-19.

## Outcomes & Retrospective

Implementation and validation are complete. Help now presents six task groups, wrapped descriptions, configured bindings, an editable search field, immediate category/key/text matches and additional semantic matches. Query generations and cancellable tasks keep stale answers out of reopened help and leave shutdown responsive. Escape clears an active query even after clicking the body. The doubled-prefix hint follows configured bindings.

The hosted endpoint is deployed as version `c625e686-1591-420d-b6ae-2dc5902ffd63`. Synthetic requests returned HTTP 200 with relevant Detach (0.96) and split-right (0.85) scores; the existing turn-verdict endpoint also returned HTTP 200 with typed answers. A second check using all 63 registry commands ranked Detach first at 0.95 in 351 ms and split-right first at 0.92 in 421 ms. No real conversation data was used. Delivery is two coherent implementation commits on the original `hel2` branch, targeting origin/master as explicitly requested.

## Context and Orientation

`mj-tui/src/help.rs` owns overlay state, input and rendering. `mj-tui/src/actions.rs` supplies command descriptions and bindings; its execution scopes must remain independent of help categories. Composer keys are local to help. `mj-chat` provides TextInput, TextField, Form and wrap_styled_line. `mj-cli/src/dashboard.rs` owns the terminal event loop; its io module delivers supervised job results. `mj-core` holds shared data contracts. `services/jev-proxy` is the existing Cloudflare Worker with pinned npm tooling and a deployed TypeSafe secret. `.agents/docs/jev-proxy.md` documents its deployment and recovery.

## Plan of Work

First implement a pure `mj-core` help-search contract and a shared fixed question template. Requests carry a query and entries with request-local integer IDs, category, label and description. Responses carry validated ID/probability pairs. Limit queries to 1024 UTF-8 bytes, catalogs to 128 entries and messages to 64 KiB. The proxy builds Noul (yes/no probability) questions itself, using a fixed relevance instruction for each entry; the direct client builds the same questions. Add `/v1/help-search` with its own 120/minute/IP limiter, the existing eight-second upstream timeout and sanitized errors. Test this milestone through core and proxy tests.

Next replace plain help lines with structured entries grouped into Essentials, Workspaces, Sessions, Panes, Composer, and Settings & Diagnostics. Show aligned key/name rows and indented descriptions using the existing wrapper. Keep prefix and search above a stable scrolling viewport. Use TextField editing and paste, clickable focus, `/` to focus, Escape to clear/unfocus before closing, and Enter to close. Literal matches include category names. Retain unavailable and unbound commands. Correct the doubled-prefix composer hint to use the active prefix. Test key transitions and terminal rendering at multiple sizes.

Finally expose the current semantic request from the TUI and accept results only for its generation. A dashboard-owned task observes request changes, cancels its predecessor, waits 400 ms, resolves credentials off the event loop and calls either TypeSafe directly or the hosted endpoint. Deadline is ten seconds. Clearing the query, closing/reopening help and shutdown cancel or invalidate requests. Errors and task panics reach a visible fallback state and diagnostic logs without query/credential text. No automatic retries. Test scheduling with controlled time and HTTP with local fake servers.

## Concrete Steps

Work at `/home/jonathan/Projects/hel2`. Use apply_patch for edits. Run focused tests during each milestone, then `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings` on the dev profile and `git diff --check`. Every cargo test runs outside the restricted sandbox. Use isolated MJ_CONFIG_DIR and MJ_DATA_DIR for validation; keep normal build storage.

In `services/jev-proxy`, run `npm run check`, `npm test`, and `npm run deploy:dry-run`, then deploy with `npm run deploy` using existing authentication. Verify the deployed search endpoint using synthetic help entries and both natural-language queries below, and verify turn-verdict still returns typed answers. Stage only changed files, commit on the current branch, fetch origin and push the resulting commit to origin/master without force, branching, or rebasing.

## Validation and Acceptance

All registry commands and custom bindings remain reachable by scrolling or filtering; descriptions do not clip on narrow screens. Tests must cover category/key/description matches, counts, no-results, Unicode editing and paste, scroll bounds after resize, wizard restoration, literal/semantic deduplication and ranking, the 400 ms debounce, cancellation and stale responses across close/reopen. Fake HTTP tests cover malformed/missing scores, HTTP errors and deadlines. Proxy tests cover bounded validation, fixed questions, independent rate limits, direct/hosted parity and existing turn verdicts. Live synthetic queries “leave agents running when I exit” and “show two conversations side by side” should identify detach and split commands respectively. Live results are evidence, not deterministic CI assertions.

## Idempotence and Recovery

No database changes or new crates are needed. Test fixtures remain isolated. The proxy adds a route without changing the existing route contract; unsuccessful deployment leaves local fallback functional. Use the existing runbook's Worker rollback procedure if deployment verification regresses. Never force-push; report a non-fast-forward obstruction without altering branches.

## Artifacts and Notes

Record actual checks, deployment version and synthetic outcomes here as work completes. Do not record credentials or real user queries.

Focused validation: 23 help-related tests and initial background lookup tests passed. The complete `cargo test` command subsequently passed, including the third test proving cancellation drops in-flight work. `cargo clippy --all-targets -- -D warnings` passed. After the final mouse/Escape refinements, all 681 TUI tests passed (two existing tests ignored). Formatting and diff checks passed. TypeScript type checks, proxy tests and elevated deployment dry run passed. Full Rust validation used `/mnt/optane/mj-help-validation.RNZzTu` for isolated config/data and logs (`cargo-test.log`, `clippy.log`, `tui-final.log`), retaining normal Cargo build storage. Local branch `hel2` matched origin/master before implementation commits; no branch change was made.

## Interfaces and Dependencies

Add shared serializable HelpSearchRequest, HelpSearchEntry and HelpSearchResponse types in mj-core and a question template consumed by Rust and TypeScript. Export a TUI pending-search snapshot and result application method, keeping I/O in mj-cli. Add HelpSearchFinished to DashboardIoUpdate. Reuse reqwest, Tokio, existing TypeSafe credential resolution, shared component editing and wrapping; no new dependency is required. Proxy POST accepts `{query, entries}` and returns `{scores}` with entry IDs and probabilities. Unknown, duplicate, incomplete, non-finite or out-of-range scores fail the request rather than partially changing the screen.

Initial plan recorded 2026-09-19 from the approved conversational plan; added the user's explicit push instruction.

2026-09-20 update: recorded completed implementation, focused tests, deployed Worker version and synthetic verification. Full validation and commit/push remain.

2026-09-20 completion update: recorded full validation, final TUI regression results, full-catalog semantic smoke results and the first implementation commit. This plan accompanies the final UI commit; the authorized remote delivery target is origin/master.
