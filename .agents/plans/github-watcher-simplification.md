# Record GitHub changes before processing each item

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

GitHub comments, reviews and pull request changes must keep arriving when one item fails. The watcher will record the issues feed before classification or delivery, then commit each item's timeline separately. GitHub event IDs prevent duplicate deliveries, and the first stored body stays authoritative.

## Progress

- [x] Implement the follow-up design: single offset walk for each full timestamp tie; a durable reconciliation checkpoint with hourly attempts and an hour of lag; response Date minus a 30-second margin; regressions for old backlog, long pauses, interrupted reconciliation and snapshot/Date interleaving. Separate source cursors and durable classification retry backoff remain present.
- [ ] Execute the new regressions, full controller/core suites and both production dry runs. The parent explicitly directs no cargo test in this round and will run these checks. Formatting and diff whitespace checks passed. Workspace Clippy excluding desktop with all targets and warnings denied passed for the checkpoint revision (checkpoint-clippy.log). Tests and dry runs remain unrun as directed; earlier runtime logs validate earlier iterations only.

- [x] (2026-10-19 UTC) Read repository instructions, existing implementation, migration conventions and the failure diagnosis; inspect real issues and timeline shapes with read-only GitHub requests.
- [x] (2026-10-19 03:54Z) Implement and validate schema and database operations for recorded items and per-item timeline progress.
- [x] (2026-10-19 UTC) Replace polling and old tests; update configuration documentation and the embedded mj skill reference.
- [x] (2026-10-19 03:54Z) Run isolated regressions, full controller/core suites, workspace clippy excluding desktop, formatting, and read-only production mapping on recent and historical real data.

## Surprises & Discoveries

Real timelines omit existing review line comments: PR 22 has eleven line comments attached to a reviewed event but no nested comments or line-commented events; PR 2136 corroborates this. Private evidence is in the report directory. The real timeline for PR 5376 has a merged event at 00:32:12 and its closed event at 00:32:13. Suppressing only matching timestamps would deliver two lifecycle notices. Committed entries have no numeric ID or created_at and are ignored.

## Decision Log

- Decision: Record feed pages before classification and delivery; commit each item separately.
  Rationale: No mailbox collision or failed item can roll back a repository cursor or another item's events. The outbox owns the first stored payload for each GitHub ID.
  Date/Author: 2026-10-19 UTC, assigned design and implementing agent.
- Decision: Use breaking revision 85 and raise its compatibility floor in the migration transaction.
  Rationale: Older binaries address removed feed and PR-state columns. The parent approved this revision. Reset cursors and watch starts to migration time, retaining watch/classification rows.
  Date/Author: 2026-10-19 UTC, parent and implementing agent.
- Decision: Add paged per-item pulls/{n}/comments requests using the cursor stored before this pass.
  Rationale: Real complete timelines for PRs 22 and 2136 omit existing review line comments. The parent explicitly authorized this extra request. Commit its events with timeline events and independent source cursors (the later review decision replaces the original shared cursor).
  Date/Author: 2026-10-19 UTC, parent decision.
- Decision: Replay boundary timestamps and keep the first GitHub outbox row. Map every comment returned by the comments API, which filters on update time.
  Rationale: GitHub uses distinct ID spaces and exclusive since filtering. A lower ID at a shared timestamp must not disappear. A newly published comment can have an older creation time; edited comments are harmless duplicates by key.
  Date/Author: 2026-10-19 UTC, implementing agent.

## Outcomes & Retrospective

The latest follow-up replaces the expiring two-hour reconciliation window with a durable reconciled_through checkpoint. Each eligible pass reads from that checkpoint through the earlier of the main cursor or one hour ago, saving progress with every recorded page. An empty completed range also advances its checkpoint to the upper bound. The hourly attempt claim stays separate and cannot discard unfinished coverage. Pages cannot change the other feed cursor or overwrite newer seen-item versions. Historical catch-up, long pauses and failed passes therefore retain their second-read coverage. Offset paging still has a conditional guarantee: an item can be missed if concurrent movement in its tied second disturbs both the main and later reconciliation reads. The 30-second processed-point margin is unchanged. Runtime suites and dry runs remain the parent's responsibility; no cargo test is attempted this round. Earlier validation results below apply only to earlier implementations.

Implementation and required validation are complete. Controller tests: 1992 passed, 13 ignored; core tests: 499 unit and 3 scenario tests passed; no failures. Workspace clippy excluding desktop and formatting passed. The serial watcher regression rerun passed. The production mapper emitted 361 comment and 299 lifecycle candidates from 511 recent items. Three historical PRs supplied real shapes absent from that window: 3 reviews and 13 line comments, plus 4 other events. No live store was opened and nothing was committed, pushed or installed.

Real API evidence changed the timeline-only assumption: line comments require a separate per-item request. The parent authorized it, and both sources share one item transaction while keeping independent cursors. Historical migration fixtures also needed the old watcher shape restored before reopening old revisions. Full details and sanitized mapping results are in `.mj/agents/1766081c04e21ba8376463313c3edcb7/findings.md` and `dry-run-summary.json`.

## Context and Orientation

`mj-controller/src/github_watch/service.rs` owns periodic background polls, GitHub credentials, rate limits and Jev classification. Jev decides whether an item interests a session or was created by that session. `mj-controller/src/database/github_watch.rs` owns durable progress and outbox writes; the outbox is the database queue delivered to session mailboxes. `mj-controller/src/database/schema.rs` applies forward migrations, and `database.rs` names the current revision. Mailbox types and the worker relay protocol remain unchanged.

## Plan of Work

First add a forward migration and database functions for repository watch start, updated cursor, seen items and item timeline progress. Store feed pages and their cursor in one transaction. Derive pending classification from recorded items and session creation time. Keep each classification transaction. Fetch complete paged timelines only for watches with newer recorded updates. For each PR also fetch pulls/{n}/comments with since set to one second before that source's stored cursor. Map comments, reviews, nested line comments and PR lifecycle entries, then commit both sources and each source's newest timestamp in one item transaction. Save the first timeline response Date as the processed point, floored to a second minus 30 seconds, and process equal timestamps again. Reconcile from the durable reconciled_through point to min(main cursor, now minus one hour), at most once per hour, using the same acquisition. Save only reconciliation progress and never change the main cursor. Mark 404/410 items processed. Preserve credential plumbing, rate limits, cancellation, config gates and self-authored turn wake suppression. Delete replaced scans, ETags and mutable PR-state inference.

## Milestones

The database milestone makes feed recording independent from delivery. Verify a revision-84 fixture upgrades without losing classification/watch rows and resets all progress. The polling milestone demonstrates on the fake HTTP server that one failed item cannot block another or roll back a repository page, duplicate event bodies preserve their first value, timestamp boundaries replay safely, and sessions never classify earlier items. The final milestone runs the required checks and exercises the same mapping code on recent real responses without database writes.

## Concrete Steps

From the repository root use normal cargo/mbx configuration, without redirecting target output. Run `cargo test -p brokk-mj-controller github_watch` while iterating, with elevated permissions for socket tests. After the final edit run `cargo test -p brokk-mj-controller -p brokk-mj-core`, `cargo clippy --workspace --exclude brokk-mj-desktop --all-targets -- -D warnings`, and `cargo fmt --all -- --check`. Save logs in `.mj/agents/1766081c04e21ba8376463313c3edcb7/`. Rerun timeout or mbx deadline failures serially before reporting failures.

## Validation and Acceptance

All synthetic watcher regressions and migration tests must pass. The dry run uses read-only `gh api` responses for brokkai/bifrost-dev since 2026-10-16 and runs the production timeline mapper through an ignored test entry point. Its report gives event counts and trimmed examples without retaining private text in committed fixtures. Full validation must be green or document unrelated baseline failures with logs.

## Idempotence and Recovery

Feed upserts and inclusive cursor overlap can repeat safely. Each GitHub outbox insert ignores existing keys; every other producer retains strict equality checks. Failed timelines leave their item progress untouched for the next poll. Migration runs atomically; interruption retries from the previous revision. Installation and live data changes are excluded.

## Artifacts and Notes

Initial line counts: service.rs 2933; database/github_watch.rs 423. Detailed evidence and command logs belong in the report directory above.

## Interfaces and Dependencies

Use existing reqwest API requests, rusqlite serialized database writes, chrono timestamps, GithubItemEvidence and MailboxEventBody. New database operations record a feed page, load recorded items, load pending watches and commit timeline events. A pure timeline mapper is shared by polling and the read-only dry run. No new crate, relay message or config setting is needed.

Plan updated after the parent authorized the per-item review-comments request. Its events share wake suppression, stable keys and the timeline transaction. Validation and both production-mapper dry runs completed successfully. Final report: `.mj/agents/1766081c04e21ba8376463313c3edcb7/findings.md`.

Review follow-up: migration 85 has not shipped and will be updated in place. The parent requires keyset feed acquisition with offset paging only within a full shared second; independent timeline and review-comment cursors in one item transaction; durable per-pair classification attempts with one-minute to one-hour backoff and fair ordering; and eligibility at or after the response-Date fetch point, floored to a second. Each defect gets a regression. Earlier validation results above describe the previous iteration until the follow-up validation completes.

The latest parent decision adds reconciled_through to unshipped migration 85, initialized to migration time with the main cursor. It replaces the two-hour wall-clock window. Nullable last_reconciled_at_ms remains the serialized hourly attempt throttle. Reconciliation loads the current main and reconciliation checkpoints after main acquisition, and never reads past the earlier of main progress or one hour ago. Page transactions save seen items and exactly one progress owner, named by GithubFeedProgress. Short pages and the first item beyond the bound prove the remainder of a range empty and allow its reconciliation checkpoint to reach the bound. Failed pages retain the preceding committed point. A new attempt restarts there, overlapping timestamp ties. Equal lower and upper bounds still replay their shared boundary: a failed full page may have saved that second before finishing its offset pages. No migration 86 is added.

The revised regressions use the 401-item moving tie to hide item 201 in main acquisition. They recover it while catching up three-hour-old backlog, after a three-hour pause, and after an HTTP 500 following a durable prefix page. The failure case checks the hourly claim delay, the saved checkpoint, and the resumed request's since parameter. Each case delivers the recovered watched comment, leaves the main cursor unchanged, and checks the reconciliation upper bound. The guarantee explicitly permits loss if movement disturbs both reads of the same second; it does not claim unconditional recovery.
