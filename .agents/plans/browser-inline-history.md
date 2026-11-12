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

Use one conversation-owned paging state, retired with the existing request generation. Keep fetched rows across live refreshes, deduplicate by stored stable identity, and reuse `paintEntry`. Preserve an existing visible row's screen offset when prepending. Show errors with a retry button. Keep BROWSER_TRANSCRIPT_LINES at 1,000 because rendered lines and stored items are different units.

## Surprises & Discoveries

HistoryEntry initially omits stable identities although stored items already have them; identity must cross the API for reliable deduplication. Existing CSS overrides the button hidden attribute, requiring the established hidden class. One store regression pinned complete-turn counts for 1,024 items; the corresponding 512-item counts are 600/700.

## Validation and acceptance

From tests/e2e/web run `node --test *.unit.test.mjs` and `npx playwright test --project deterministic`. Extend existing history tests to demonstrate inline prepending, retry, exhaustion and rejection of late responses after navigation. Run Rust tests with packages brokk-mj-core, brokk-mj-transcript, brokk-mj-client and brokk-mj-controller and filters server:: server_runtime:: database::. Run workspace clippy and fmt checks. Tests run outside the restricted sandbox. No live default instance is used.

## Recovery and interfaces

Failed history requests retain their cursor for retry. Session changes abort requests and clear cached history. No database or core transcript contract changes are needed. Repeating checks is safe; commit only assigned files on the detached worktree, without pushing.

## Outcomes & Retrospective

Inline loading, retry, scroll anchoring, exhaustion and late-response rejection pass deterministic browser tests. Overlapping mailbox rows retain one identity each. Web unit tests pass; workspace clippy and formatting pass. Controller suites passed 674 tests with one stale numeric fixture, whose corrected focused rerun passed. Client transcript tests passed 11 cases with one expected identity golden update, whose focused rerun passed. BROWSER_TRANSCRIPT_LINES stays unchanged. The complete audit is included in the parent report directory.

Updated 2026-11-12 after browser validation and tail audit; documented CSS and fixture findings.
