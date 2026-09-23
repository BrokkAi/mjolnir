# Bound live projections and page earlier conversation history

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture


Issue #1045 asks the daemon (the controller process) to retain recent conversation history instead of every item ever produced by each live session. Durable SQLite history remains complete. Users can read earlier messages in both the terminal and browser through bounded, asynchronous history pages. A current turn can exceed the nominal window: correctness requires retaining its streaming messages and tools until they settle.

## Progress


- [x] (2026-09-23) Read the issue, current projection and rendering paths, and repository instructions. Claimed #1045. Merged current origin/master into the existing branch.
- [x] (2026-09-23) Investigated #1106, #1132, #1121, and #1122 separately; documented upstream ownership, labeled upstream, and relinquished those claims as instructed.
- [x] (2026-09-23) Bound actor loading, catch-up, repair, and publication at safe turn boundaries; preserve metadata and durable history.
- [x] (2026-09-23) Implement one backward history query and asynchronous access for terminal and browser.
- [x] (2026-09-23) Add discoverable earlier-message browsing to both surfaces with visible loading/errors and cancellation on dismissal.
- [x] (2026-09-23) Proved retention, replay, streaming updates, pagination, and UI behavior. Full dev-profile `cargo test`, all-targets clippy with warnings denied, formatting, and diff checks passed.
- [x] (2026-09-23) Prepared the validated change and issue completion comment for publication. The final delivery actions are committing on the existing branch, pushing `HEAD:master` to origin, and closing #1045 after the push succeeds.

## Surprises & Discoveries


Current master already has `BrowserTranscriptProjector`, which calls the same `materialized_chat_entries_reusing` function as the TUI. The issue's proposed incremental-render refactor is already delivered. The active actor still loads the complete projection and retains all transcript items. Browser and terminal surfaces already limit their initial rendering, but neither has durable backward paging for ordinary session history.

Checkpoint capture previously serialized the live snapshot. Windowed captures now explicitly reload the complete durable projection and retain the existing exact-frontier/digest checks, avoiding incomplete archives. Explicit historical tool, terminal, shell, notice, and message references are reloaded before projection so late events can still update immutable identities.

## Decision Log


2026-09-23: Preserve complete turns around the 1,024-item target instead of cutting at an arbitrary item. An oversized active turn is an explicit correctness exception to the target. Keep SQLite as the full history source and preserve the first-message title and latest-turn position separately.

2026-09-23: Expose earlier history as a bounded reader in each surface, separate from the changing live feed. This avoids holding full historical projections in the daemon or mixing a historical page with mutable live rendering. Both use the same database page semantics and shared transcript interpretation. Network and database work must remain supervised background work; closing a reader discards stale results.

## Outcomes & Retrospective


Implementation and validation are complete. Full dev-profile `cargo test` passed, including 1,582 controller tests and the streaming/tool comparison against full history. `cargo clippy --all-targets -- -D warnings`, formatting, and diff checks passed. The deterministic web suite passed 43 JavaScript unit tests and 92 browser tests (3 browser tests skipped). The 2,500-item fixture retains 1,100 items at a complete-turn boundary while preserving all 2,500 durable rows. No live production RSS measurement was performed. Oversized current turns and older unsettled content can exceed the nominal target intentionally.

The completed self-assignment directives were separately pushed to origin/master at `df50feb8`. The implementation is staged for the final commit and push; the completion comment on #1045 will record the published implementation commit.

## Context and Orientation


`mj-controller/src/session_manager/standalone.rs` owns each live worker connection and its `MaterializedSession`, a typed projection of the worker's durable event journal. `apply_event_page` computes mutations on a working copy, commits them to SQLite, and publishes only after success. `mj-controller/src/database/materialized.rs` provides whole-history reads and a bounded tail loader used by pollers. `ProjectionWindow` in `mj-core/src/state.rs` carries facts outside a retained tail. The durable transcript is ordered by `(position, stable_id)`; an agent message's content sequence can advance while its original position stays fixed, so backward paging must use position and stable ID rather than update sequence.

`mj-client/src/transcript.rs` interprets stored items for both surfaces. `mj-chat/src/chat/active.rs` owns supervised terminal I/O updates; `SessionHandleBackend` in `mj-client/src/session.rs` abstracts controller requests. `mj-controller/src/server/api/turns.rs` and `server/api/routes.rs` serve authenticated browser history requests. The viewer is in `mj-controller/src/web/viewer.js`. Existing browser behavior tests are under `tests/e2e/web/`.

## Plan of Work


First add safe projection window maintenance and load only a turn-aligned tail when connecting or reloading after another projector advances. Keep metadata independently of dropped rows, and trim only after successful durable commits. Verify streaming and tool updates survive window movement and account for late updates to retained mutable items.

Next add a database query that reads at most a requested page plus a lookahead row before an exclusive `(position, stable_id)` cursor, in one SQLite read snapshot. Return chronological items, a continuation cursor, and the projection frontier. Share this query between the terminal session backend and an authenticated browser history endpoint. Preserve existing forward transcript API semantics.

Finally add an earlier-messages reader to the terminal and browser. Show loading immediately; page asynchronously; expose errors and retry; retire outstanding results when the reader or session closes. Keep live snapshots bounded and retain existing shared incremental rendering.

## Concrete Steps


Work in `/home/jonathan/Projects/mjolnir3` on the existing branch. Use normal mbx-backed Cargo storage; do not redirect targets or alter build caching. Run focused tests while implementing, then `cargo test` outside the sandbox and `cargo clippy --all-targets -- -D warnings` on the dev profile. Run `cargo fmt --all -- --check`, `git diff --check`, and applicable browser tests. Use an isolated named instance for any live test build. Stage only changed files, commit, and push `HEAD:master` to origin as explicitly authorized. If origin advances, merge it without changing branches or rebasing, and validate affected changes before pushing.

## Validation and Acceptance


Create more than 1,024 synthetic items spanning multiple turns. Confirm the actor retains a turn-aligned tail, SQLite still contains every item, and reconnect does not load the full history. A streaming message and a tool update in the current turn must retain their identity and complete correctly after trimming. Page backward across ties in position without skipping or duplicating items; concurrent appends must not disturb the cursor. Test missing sessions and bounded limits. Exercise earlier-message browsing, loading/error/retry, and dismissal while a result is pending in both surfaces. Existing forward paging and shared renderer tests must remain passing.

## Idempotence and Recovery


No schema migration or live-store mutation is needed for the history reader. Projection pages remain atomic, and failed pages are replayable from their durable frontier. Keep archive/repair writes complete before trimming the in-memory representation. Never save a windowed projection with the whole-session replacement writer. Do not modify live sessions for tests.

## Artifacts and Notes


The four upstream ticket comments contain the source-tracing evidence for the separately requested harness defects. This plan covers only the local #1045 implementation.

## Interfaces and Dependencies


Use existing `ProjectionWindow`, `TranscriptItem`, `SessionHandleBackend`, SQLite readers, and shared rendering functions. Add a serializable history cursor/page in `mj-core::storage`; add no crate or dependency. No existing endpoint loses fields or changes forward-cursor interpretation.

Revision 2026-09-23: Initial plan from current-master inspection and the user's authorization to implement and push each local fix.

Revision 2026-09-23: Implemented durable backward paging, both asynchronous readers, turn-aligned actor windows, historical reference recovery, and complete checkpoint capture. Merged concurrent origin/master changes. Added live-vs-full streaming/tool equivalence, database retention/reconnect/cursor, API authentication/cursor, and UI dismissal/retry regression tests. Full dev-profile Rust tests, clippy, and deterministic browser validation passed. Delivery is the remaining commit/push/issue-close sequence.
