# Clear session GitHub notifications with the conversation


This living plan follows `.agents/PLANS.md`.

## Purpose / Big Picture


After `/clear`, a session stops receiving notifications from its previous issue and pull-request subscriptions. Other sessions keep their subscriptions. A later decision based on the new conversation can subscribe normally; clearing settings does not permanently ban remotely existing items.

## Progress


- [x] (2026-11-25 19:50Z) Reset existing subscriptions atomically with successful clear; validate rollback and replay.
- [x] (2026-11-25 19:50Z) Exclude pre-clear creator output and history from later classification evidence.
- [x] (2026-11-25 19:50Z) Reject successful and failed classifier results whose captured clear ordinal changed.
- [x] (2026-11-25 19:51Z) Finish targeted validation and clippy.
- [x] (2026-11-25 19:54Z) Complete required review and push implementation through 935d316f1 to origin/master; defer the repair-overlap finding below.

## Surprises & Discoveries


GitHub timestamps cannot establish an exact conversation boundary: items on either side of a clear can report the same second, and worker clocks can differ. Copying the retained feed into classification receipts creates unnecessary per-session data. Repository cutoffs can also exclude work the worker starts before the daemon projects a clear. The final design needs neither mechanism nor a schema change: existing subscriptions reset once, old results are rejected, and later evidence comes only from the current conversation.

## Decision Log


Use `mailbox_reset_frontiers.through_ordinal`, the durable event number of the last successful clear, as the classification generation. SQLite serializes clear and result writes, so old results cannot recreate watches or retry state. Retain completed classification receipts with interest and creator flags disabled. Restrict creator output and history scans to transcript positions after the existing context boundary. A future classification using fresh evidence is an intentional new decision, including for a remotely older item. Remove the unshipped timestamp filters, copied feed receipts, repository cutoffs, and migration. Date/author: 2026-11-25 19:50Z, Codex.

## Context and Orientation


`mj-store/src/database/mailbox.rs` runs clear cleanup once per successful relay event and preserves its completion receipt across projection rebuilds. `mj-store/src/database/github_watch.rs` stores classification decisions, creator watches, retry state, and GitHub feed rows. `mj-github-watch/src/service.rs` prepares evidence, calls a classifier outside the database, and submits results. A classifier decides whether an item concerns a session. `mj-store/src/database/materialized.rs` already locates the last context boundary in retained transcript rows. `mj-store/src/database/schema.rs` shares the existing watch-detachment SQL with clear cleanup; its persisted schema is unchanged.

## Plan of Work


Detach creator/follower watches without deleting watches other live sessions need. Neutralize the cleared session's existing classification flags and delete its failed-pair retry rows in the same transaction as the successful clear. Capture the durable clear ordinal before reading evidence, then compare it inside both success and failure write transactions. Defer only a session whose projection is rebuilding. Share the existing transcript-boundary reader with the GitHub database module and apply its position to creator output and every history-scanner selection. Preserve position-zero reads when no boundary exists.

## Milestones


The first milestone makes clear cleanup atomic and replay-safe. Six mailbox integration cases validate rollback, replay, shared watches, and destruction. The second milestone isolates evidence and asynchronous classifier results by conversation. Persisted creator-output tests, a complete history-render comparison, and an isolated watcher race regression demonstrate that old context and results cannot restore subscriptions, while new work remains eligible.

## Concrete Steps


Run from the repository root with mbx; run tests outside the restricted sandbox:

    mbx test -p brokk-mj-store database::mailbox::tests::
    mbx test -p brokk-mj-store creation_output_comes_only_from_calls_running_at_creation
    mbx test -p brokk-mj-store database::github_watch::history_snapshot_tests::
    mbx test -p brokk-mj-github-watch clear_fences_classification
    mbx test -p brokk-mj-github-watch classification -- --skip clear_fences_classification
    mbx clippy -p brokk-mj-store -p brokk-mj-github-watch --all-targets -- -D warnings

Every test filter must execute tests and report no failures. Commit changed files on the current branch, request review, then push HEAD to origin/master after satisfying review. Merge upstream if needed; do not rebase or create a branch.

## Validation and Acceptance


Use temporary stores and the existing isolated-child helper, never live session data. The watcher regression clears during classification, checks that stale success and failure writes produce no subscriptions or retries, then changes conversation interests and verifies new notifications. It covers timestamp precision, clock skew, and a rebuilding session alongside a healthy one. The creation-output test excludes archived tool output, and the history-render comparison remains unchanged when archived rows precede the clear boundary. Six mailbox cases verify atomicity and replay.

## Idempotence and Recovery


The existing clear completion receipt makes projection replay idempotent. Tests leave no daemon running. No live store, schema revision, persisted format, or build-output layout changes are required. Validation can be repeated through mbx safely.

## Artifacts and Notes


Prior checks passed six mailbox cases, the creator-output case, both history scanner cases, four existing classification cases, and clippy for both affected crates. Final success/failure fencing and all four existing classification cases passed. A reviewed alternative used cutoffs, but it was removed because conversation ownership provides the needed boundary without extra persisted state.

## Interfaces and Dependencies


Reuse rusqlite transactions and the serialized database writer. Keep `load_github_classification_frontier(session_id: &str) -> Result<Option<u64>>`, with `None` indicating a rebuilding projection. `GithubItemClassification.clear_ordinal` and the failure writer's clear ordinal capture that value before evidence reads. Share `materialized::last_materialized_context_boundary` with its sibling database module. No worker protocol, harness, or external dependency changes are needed.

## Outcomes & Retrospective


The implementation resets session GitHub subscriptions and separates subsequent classifier decisions by conversation. Implementation, validation, required review, and publication to origin/master are complete. One rare overlap with projection repair is deferred: a repair can begin after readiness was checked, expose a pre-clear checkpoint to an evidence reader, and catch up before the result commits under the same durable clear ordinal. A follow-up should bind readiness to the snapshots used for the session digest, creator output, and history, coordinating those readers with projection replacement. The reset and actual clear transitions are covered; this repair/read ownership change was intentionally left outside the requested feature.

Revision note (2026-11-25 19:50Z): Removed unshipped cutoffs and migration; retained the existing conversation owner and fenced failure writes as well as successful results.

Revision note (2026-11-25 19:51Z): Success and failure fencing regression and existing classification tests pass after simplifying to conversation ownership.

Revision note (2026-11-25 19:54Z): Record the successful push and the deferred projection-repair/read consistency finding from final review.
