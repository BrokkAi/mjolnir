# Clear session GitHub notifications with the conversation


This living plan follows `.agents/PLANS.md`.

## Purpose / Big Picture


After `/clear`, the session stops receiving notifications for its previous conversation's issues and pull requests. Other sessions keep their subscriptions. New work can subscribe normally, including when GitHub reports only whole-second timestamps or a worker's clock differs.

## Progress


- [x] (2026-11-25 19:42Z) Reset existing subscriptions atomically with a successful context clear; validate rollback and replay.
- [x] (2026-11-25 19:42Z) Reject classifier results whose captured clear ordinal has changed; validate with a real isolated database.
- [x] (2026-11-25 19:45Z) Replace copied item receipts with compact repository cutoffs and restrict creator/history evidence to the current conversation.
- [x] (2026-11-25 19:45Z) Validate compatible migration, creator evidence, history rendering, clear races, and clippy.
- [ ] Complete required review and push to origin/master.

## Surprises & Discoveries


GitHub timestamps cannot establish an exact conversation boundary: items on either side of a clear can report the same second, and worker clocks can differ. Copying every recorded item into a cleared session's classification table prevents retries but makes storage proportional to the retained global feed. Delayed feed discovery also exposes retained tool output unless evidence reads obey the conversation boundary. The isolated race regression already passes for in-flight results, failed pairs, and a rebuilding session alongside a healthy one.

## Decision Log


Use the existing durable `mailbox_reset_frontiers.through_ordinal` as the classification generation. SQLite serializes clear and classification writes, so results from old evidence cannot create watches. Keep completed classification receipts with interest and creator flags disabled. Add compatible schema revision 160 with `github_session_reset_frontiers`, storing one maximum issue/PR number per session and watched repository. GitHub assigns increasing issue and PR numbers within a repository; this cutoff needs no worker clock. Restrict creation output and history scans to positions after the latest context boundary, so late-discovered items cannot use the discarded conversation. These decisions replace timestamp filtering and copying the global feed. Date/author: 2026-11-25 19:42Z, Codex.

## Context and Orientation


`mj-store/src/database/mailbox.rs` applies clear cleanup once per successful relay event and retains its completion receipt across projection rebuilds. `mj-store/src/database/github_watch.rs` stores classification decisions, creator watches, and GitHub feed rows. `mj-github-watch/src/service.rs` reads evidence, calls a classifier outside the database, and submits its result. A classifier is the component that decides whether a new item concerns a session. A clear ordinal is the monotonically increasing relay event number of the last successful clear. `mj-store/src/database/materialized.rs` reads transcript rows and already finds the last clear boundary. `mj-store/src/database/schema.rs` owns forward migrations; `mj-store/src/database.rs` declares their current revision.

## Plan of Work


Finish the compact cutoff table in schema revision 160, leaving minimum compatible revision 159. During clear, detach creator/follower watches without deleting watches other live sessions need, neutralize the session's existing classifications, remove failed-pair retry rows, and capture each watched repository's indexed maximum item number. Classification target reads and writes must both honor this cutoff. Make the existing transcript boundary reader available to the sibling database module, then apply its position to creation-output and all history-scanner row selections. Preserve reads of position zero when no boundary exists.

## Milestones


The first milestone is already implemented: clearing subscriptions and their mailbox deliveries commits with projection state, and replay cannot clear subscriptions created later. The existing mailbox suite proves this through a temporary SQLite database. The final milestone adds compact durable cutoffs and discards old evidence. It is complete when migration compatibility and delayed-discovery regressions pass, including the existing history-render golden output with pre-clear records present.

## Concrete Steps


Run from the repository root through mbx, outside the restricted sandbox for tests:

    mbx test -p brokk-mj-store database::mailbox::tests::
    mbx test -p brokk-mj-store database::github_watch::tests::
    mbx test -p brokk-mj-store database::github_watch::history_snapshot_tests::
    mbx test -p brokk-mj-store revision_160
    mbx test -p brokk-mj-github-watch clear_fences_classification
    mbx clippy -p brokk-mj-store -p brokk-mj-github-watch --all-targets -- -D warnings

Every filter must execute tests and report zero failures. Commit only changed files on the current branch, request review, then push HEAD to origin/master after the required review is satisfied. Merge upstream when necessary; do not rebase or create a branch.

## Validation and Acceptance


Use only isolated temporary stores and the existing isolated-child test helper. The race regression clears while classification is running, verifies old results and retry backlog cannot subscribe, then verifies new items still produce mail with clock skew in both directions. Extend the creation-output integration test and history-render golden case to include retained pre-clear records that must not appear in evidence. The migration test must upgrade revision 159, retain compatibility with a revision-159 writer, preserve reset rows across ordinary session updates, and delete them when the owning session is deleted.

## Idempotence and Recovery


The clear completion receipt makes projection replay idempotent. Repository cutoffs advance monotonically. Migrations are forward-only and only add a narrow table; no live store is opened for testing. Tests use temporary stores and leave no daemon running. Interrupted validation can be rerun through mbx without changing build storage.

## Artifacts and Notes


Prior focused validation reported six mailbox tests passed, four existing classification tests passed, and the clear/classification regression passed. Both affected crates passed clippy. Final validation passed: six mailbox cases, the creator-evidence case, both history scanner cases, the migration case, and the watcher race case; clippy passed for both affected crates. SQLite EXPLAIN confirmed that each repository maximum uses the existing covering index.

## Interfaces and Dependencies


Reuse rusqlite transactions and the existing serialized database writer. Keep `load_github_classification_frontier(session_id: &str) -> Result<Option<u64>>`, with `None` indicating a rebuilding projection. `GithubItemClassification.clear_ordinal` captures that value before evidence reads. Share `materialized::last_materialized_context_boundary` with the GitHub database module. No worker protocol, JSON format, harness, or external dependency changes are needed.

## Outcomes & Retrospective


Implementation and validation are complete. Existing subscriptions reset atomically, old evidence is excluded, delayed discovery uses the current conversation, and reset storage is one row per watched repository. Required review and publication remain.

Revision note (2026-11-25 19:42Z): Record the final compact-cutoff and evidence-boundary design after review identified delayed discovery and global receipt-copy costs.

Revision note (2026-11-25 19:45Z): Implemented the compact frontiers and current-conversation evidence filters; recorded passing integration and lint checks.
