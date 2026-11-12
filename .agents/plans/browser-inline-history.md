# Page browser conversation history inline

Maintained according to `.agents/PLANS.md`.

## Purpose

Scrolling to the top of a conversation loads stored older rows above the live feed without losing the reading position. This permits reducing the daemon's shared transcript window from 1,024 to 512 items.

## Progress

- [x] 2026-11-12: Inspect rendering and history API.
- [x] 2026-11-12: Implement paging and stable identity deduplication.
- [x] 2026-11-12: Extend browser regressions and audit tail readers.
- [x] 2026-11-12: Rust validation completed; commit prepared.

## Context and milestones

`mj-controller/src/web/viewer.js` renders live rows through `paintEntry`; the history endpoint returns chronological pages and an exclusive `before` cursor. First add existing stored identities to `mj-client/src/transcript.rs` history entries. Then replace the main conversation modal trigger with inline paging, retaining the modal for stopped child conversations. Finally halve the constant in `mj-controller/src/database/materialized.rs`.

## Decision Log

2026-11-12 user decision: remove the browser's 1,000-line clip. Render the complete live SessionTail, bounded to PROJECTION_TAIL_ITEMS = 512 stored items. Rich rendering is sufficient; browser full-text access is not required. Remove suffix growth, sparse Message caching and EntryFacts; reuse converted entries over the complete input, release terminal-only detail, and retain Arc-shared published rows.

Inline history requests strictly older positions with before_position equal to the oldest live position and an empty before_id. Remove clipped-row restoration, restoredFor stamps, refetch queues and overlap retention/repainting. A single generation-owned loader retains older paging and gap cursors. When the live boundary advances, fetch backwards from the new boundary to the previous one. Retain cancellation, automatic continuation for invisible pages, visible retry on every failure, and numeric live-row scroll anchors.

## Surprises & Discoveries

HistoryEntry initially omits stable identities although stored items already have them; identity must cross the API for reliable deduplication. Existing CSS overrides the button hidden attribute, requiring the established hidden class. One store regression pinned complete-turn counts for 1,024 items; the corresponding 512-item counts are 600/700.

## Validation and acceptance

From tests/e2e/web run `node --test *.unit.test.mjs` and `npx playwright test --project deterministic`. Extend existing history tests to demonstrate inline prepending, retry, exhaustion and rejection of late responses after navigation. Run Rust tests with packages brokk-mj-core, brokk-mj-transcript, brokk-mj-client and brokk-mj-controller and filters server:: server_runtime:: database::. Run workspace clippy and fmt checks. Tests run outside the restricted sandbox. No live default instance is used.

## Recovery and interfaces

Failed history requests retain their cursor for retry. Session changes abort requests and clear cached history. No database or core transcript contract changes are needed. Repeating checks is safe; commit only assigned files on the detached worktree, without pushing.

## Outcomes & Retrospective

Inline loading, retry, scroll anchoring, exhaustion and late-response rejection pass deterministic browser tests. Overlapping mailbox rows retain one identity each. Web unit tests pass; workspace clippy and formatting pass. Controller suites passed 674 tests with one stale numeric fixture, whose corrected focused rerun passed. Client transcript tests passed 11 cases with one expected identity golden update, whose focused rerun passed. This was the initial implementation; the subsequent user decision above removes the line budget. The complete audit is included in the parent report directory.

Initial implementation updated 2026-11-12 after browser validation and tail audit; documented CSS and fixture findings.

Review correction validation (2026-11-12): web unit files pass (project discovery requires elevated subprocess access); deterministic Playwright: 120 passed, 3 skipped. Final focused paging/mailbox checks: three passed, with the fourth passing after replacing a test-only call to a module-private function with the real snapshot refresh surface. Controller `server::`: 261 passed. No Rust changes; clippy/fmt not required. Mailbox history fixtures now carry their real stored positions. No push or review requested, per delegated scope.

Advancing-window correction validation (2026-11-12): 59 web unit tests pass; deterministic project 120 passed, 3 skipped, with one initial paging failure from a loop-variable scope error fixed during validation; final focused rerun passes all four paging cases. Controller `server::` passes 261 tests. `git diff --check` passes. No Rust changes.
