# Clear pending mailbox messages with conversation context

This ExecPlan follows `.agents/PLANS.md` and must remain current during implementation.

## Purpose / Big Picture

A successful `/clear` starts a conversation with no pending messages from the old conversation. Discard messages in both the worker and the daemon queue, including arrivals through the database transaction recording clear completion. New messages after that transaction remain deliverable. Failed resets preserve pending messages; visible history, files, configuration and project memory retain their existing behavior.

## Progress

- [x] (2026-10-19) Trace clear completion, mailbox leases, actor delivery and transactional projection; confirm both queues and arrivals during clear are in scope.
- [x] (2026-10-19) Implement worker cleanup and transactional daemon cleanup with durable replay protection.
- [x] (2026-10-19) Serialize typed mailbox delivery and revalidate stale batches and reconnects.
- [x] (2026-10-19) Validate the local feature: new regressions, chat/core/transcript/worker suites, controller suite and workspace clippy. One unrelated 250 ms readiness timing assertion failed under full load and passed in isolation in 30 ms.
- [ ] Checkpoint the validated feature, merge the advanced upstream, move the mailbox migration to revision 86, and validate the merged code.
- [ ] Review, commit and push task changes on the current branch (push authorized by the user).

## Surprises & Discoveries

The worker's `ContextCleared` completion resets conversation context but leaves mailbox fields untouched. The daemon fetches batches before delivery, so deleting queue rows alone cannot prevent stale batches from reintroducing mail. Projection can be rebuilt from genesis, so clear side effects also need a receipt outside the rebuildable projection. Journal-only replay does not imply an in-flight dispatch was interrupted; the interrupted-clear regression therefore uses its actual saved dispatch snapshot, matching the existing crash-recovery test.

## Decision Log

On 2026-10-19 the user selected both daemon and worker queues and discard-through-completion semantics. Use the daemon's committed completion transaction as the delivery boundary. Preserve outbox rows with a terminal discard reason for deduplication, and retain a per-session processed reset frontier so historical replay cannot discard new mail. Add a compatible forward migration: the new table is preserved by older writers, and existing stored event/JSON formats do not change. Seed its frontier from existing materialized session ordinals to avoid treating historical pre-upgrade clears as new resets. Upstream advanced 26 commits during validation and introduced breaking schema revision 85 for GitHub watches. After the local checkpoint, merge upstream and move this compatible mailbox migration to revision 86, retaining upstream's minimum compatible revision 85.

## Outcomes & Retrospective

Worker and daemon cleanup, compatible schema revision 85, typed mailbox disposition and regression coverage are implemented. The initial worker test build passed. A focused regression exposed a fixture error in its interrupted-clear case: journal-only replay retains pending dispatch intent, while crash interruption requires the saved in-flight dispatch snapshot. The test now preserves that snapshot only for the interruption case. The worker regression, all mailbox regressions, 16 migration regressions and the runtime-history regression passed. The initial complete run passed chat/core/transcript/worker suites and exposed three older tests assuming the compatibility floor equals the current revision; those assertions were corrected and their targeted reruns passed. The complete controller rerun passed 1,993 tests; its only failure was an unrelated 250 ms readiness-cancellation assertion, which passed in isolation in 30 ms. All local feature checks are complete; upstream integration remains. Workspace `cargo clippy --all-targets -- -D warnings` passed after correcting a missing event-format field in the database test fixture.

## Context and Orientation

`mj-core/src/relay/snapshot/apply.rs` folds journal events into worker state. `ContextCleared` is the successful replacement of the provider conversation. `mj-transcript/src/projection/observation.rs` converts it into a `MaterializedSessionMutation`, the database change description defined in `mj-core/src/storage.rs`. `mj-controller/src/database/materialized.rs` applies those changes atomically with the processed journal ordinal. The ordinal is the monotonically increasing event number. `mj-controller/src/database/mailbox.rs` owns durable delivery records. `mj-controller/src/session_manager/actor.rs` owns the session's serialized worker connection; `mj-controller/src/mailbox_outbox.rs` retries delivery with stable command IDs.

## Plan of Work

First extend successful worker clear completion to retire pending and leased events, preserve their retry keys, remove leases, and discard mailbox prompt contexts. Add a mailbox-reset projection effect. Apply it in the existing projection transaction using a helper in the mailbox database module: advance a durable reset frontier only for a new ordinal and mark pending target rows terminal with `Discarded by /clear`. Introduce the frontier table in the next forward schema migration and seed existing sessions with their current projection frontier. Keep failed clear paths unchanged.

Next add a typed handle operation using the existing actor submission/reconnection and view publication path. The actor checks eligibility on every attempt; the handle translates definite deferrals and durable terminal dispositions into the delivery outcome. Synchronize before reading the durable outbox status; pending lifecycle/reset work returns pending, terminal discarded rows return discarded, accepted rows return their existing receipt, and pending rows use the existing stable relay command ID. Repeat this admission check after reconnect. Route the outbox through this operation and report expected discards without retrying them. Change command help to mention queued mailbox cleanup.

Finally extend worker, database and actor regressions. Cover stale batches, arrivals during reset, late lease acknowledgements, lost acknowledgements, failed/interrupted resets, journal replay, projection rebuilds and migration preservation. Keep tests isolated and colocated; mark defect regressions with the required Hard-won comment.

## Concrete Steps

Work in the repository root. Run focused `cargo test -p brokk-mj-worker clear` and focused controller mailbox/migration regressions outside the sandbox during development. Once edits settle, run `env -u NO_COLOR cargo test -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-worker -p brokk-mj-controller -p brokk-mj-chat --no-fail-fast` and `cargo clippy --all-targets -- -D warnings` in the dev profile with the normal mbx configuration. Use existing isolated test directories; any real CLI/daemon invocation must specify `--instance clear-mailbox-test`. Run rustfmt and `git diff --check`, then stage only task files, commit on the current branch and push its configured upstream. If upstream moved, fetch and merge it before retrying the push.

## Validation and Acceptance

Old messages must never appear in a fresh prompt/hook after successful clear, even when a previously fetched batch is delivered or the worker/controller restarts. A failed clear must leave its pending messages deliverable. New messages after the committed boundary must still deliver, including after rebuilding the projection and replaying the old clear. All new regressions and touched crate suites must pass, as must workspace clippy.

## Idempotence and Recovery

Cleanup, its reset frontier and conversation completion commit together or all roll back. Replaying an already processed reset is a no-op for mailbox state. Preserve rows and command IDs rather than deleting deduplication evidence. Never test against the host default store, alter Cargo target storage, or install the test build into the live instance.

## Artifacts and Notes

Local validation used normal mbx storage. The chat suite passed 412 tests, core 499, transcript 85, and worker 677 library tests plus its integration suites. Controller mailbox regressions passed 7 tests, migration regressions passed 16, and runtime-history preservation passed. The full controller rerun passed 1,993 tests with one unrelated wall-clock cancellation assertion failing under load; its isolated rerun passed in 30 ms. Workspace clippy with warnings denied, rustfmt checking, and whitespace checks passed. Logs are under `/mnt/optane/tmp/clear-mailbox-*`, notably `clear-mailbox-tests.log`, `clear-mailbox-controller-final.log`, `clear-mailbox-migration-regressions.log`, and `clear-mailbox-clippy-verified.log`. Post-merge validation is still required.


## Interfaces and Dependencies

No CLI or HTTP request changes are needed. Add an internal mailbox reset mutation and an internal discarded delivery outcome; keep the existing worker wire command and completion formats. The database frontier is owned by the shared writer and session delivery decisions by the session actor. Use existing subprocess, database and async helpers; no new dependencies or crates are required.

Revision 2026-10-19: Initial implementation plan records the approved queue scope and reset boundary. Updated after implementation to record shared actor submission admission, schema revision 85, regression coverage, and explicit user authorization to push.
