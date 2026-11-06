# Remove daemon turn review

This ExecPlan follows `.agents/PLANS.md`. The daemon is the process that controls sessions; workers own repository capture. Manual review now reviews committed work through the worker's push gate, just like the agent request_review tool.

## Progress

- [x] Read removal map and slice-1 report.
- [x] Fix session reviewer enablement, committed as checkpoint 0caabd189.
- [x] Add compatible revision 112 cleanup for obsolete stored turn groups.
- [x] Delete turn driver forwarding, handoff reconciliation, follow-ups and baselines.
- [x] Fold push delivery into the single review slot; the driver owns captured deltas.
- [x] Route manual starts through begin_push and return the same actionable refusals.
- [x] Make stored push replay lenient for retired requests while validating live ones.
- [x] Preserve old serialized compatibility fields and terminal tags; bump client protocol to 83 for narrowed decoding and capture actions.
- [x] Delete turn-only tests and adapt shared review, lifecycle and upgrade fixtures.
- [x] Review-host tests (49), isolated startup/review integration tests (4), and viewer unit test files (7) pass.
- [x] Finish the seven-crate Rust suites, check --tests, clippy and formatting. All review tests pass; one unrelated draft assertion fails intermittently and passes its isolated rerun.
- [x] Commit the completed removal.

## Context and Orientation

`mj-controller/src/review_host` owns orchestration. `mj-core/src/review` defines persisted driver state and terminal views. `mj-review` runs the pure review driver. Worker, checkpoint and relay files belong to another agent and remain untouched. The relay client always sends explicit PushableCommits scope and empty baselines because older workers require those fields. Rust Turn identifiers remain for a later mechanical rename.

## Plan of Work

Manual host start delegates to begin_push. Startup migration 112 retires old turn groups before deserialization, preserving columns and push groups. The driver captures committed work, runs reviewers, reaches one verdict, and lets the host deliver it by mailbox. It has no corrective prompt or follow-up path. Serialized snapshots retain inert legacy seed fields, provenance and scope for old store readers; these fields do not participate in decisions. Terminal verdicts retain an empty allowed list, and completion serializes with the established dismissed tag.

## Concrete Steps

Run cargo fmt --all and cargo check --tests for the seven affected packages. Run TZ=UTC cargo test --no-fail-fast with packages brokk-mj-core, brokk-mj-review, brokk-mj-controller, brokk-mj-client, brokk-mj-chat, brokk-mj-tui and brokk-mjolnir. The last package is the actual Cargo name of mj-cli. Run clippy on the same packages with --tests -- -D warnings. Cargo tests require elevated execution and isolated data; existing fixtures retain their isolation. Do not redirect Cargo output directories, push or install builds. Run node --test tests/e2e/web/viewer*.unit.test.mjs after the status wording update.

## Validation and Acceptance

Manual review must capture committed work and deliver the same mailbox verdict as request_review. Dirty tracked changes, unsupported harnesses and running reviews must return refusals to the caller. Stored turn rows must not prevent unrelated push review recovery. Removed replay requests must decode leniently; malformed live requests must still fail. Session reviewers enable push review unless explicitly Off. Overrides, declined findings and set_review_baseline retain their push semantics. Empty captures and group-wide failures must persist terminal reviewer slots before delivery, so daemon replacement can restore them.

## Idempotence and Recovery

Migration 112 records its revision transactionally and preserves the compatibility floor at 111. All legacy columns remain, with zero, NULL or empty-map values written by current state saves. Durable push groups resume through the ordinary driver and delivery path. Pending command delivery retains mailbox keys and existing retry backoff.

## Surprises & Discoveries

The old-store isolated upgrade fixture retained declined_findings while rewinding to schema 43, causing migration 111 to add the column twice. Removing that column in the fixture restores its historical shape; all four affected upgrade/review integration tests pass. The first full run contained an obsolete turn-admission test and a persistence test whose failed assertion left a test gate blocked; the isolated test process was stopped and the corrected host suite passes. Snapshot restore requires every reviewer slot settled in Verdict: empty captures and group-wide failure now establish that state in reach_verdict.

## Decision Log

Migration 112 is compatible because columns and older push-group serialization remain readable. Protocol 83 prevents new clients from attaching to daemons that can emit removed forwarding/resolution values, and records the narrowed controller capture-action shape. Completion keeps the known dismissed wire tag so old terminals can still read new handoff snapshots. The driver alone owns captured deltas; the host takes a copy only when admitting durable delivery.

## Outcomes & Retrospective

Implementation and validation are complete. The final full run has one unrelated failure, daemon::tests::startup_delivery_reservation_wait_does_not_prevent_move_cancellation (late destination draft assertion), which passes an isolated rerun with the exact full-run binary. The projects.unit.test.mjs check lacks its local Playwright executable; all seven viewer unit test files pass. The worker's old turn-only tests still use mj-review::delta::has_changes and captured_trees, so those public helpers remain until the worker branch is merged. The remaining ReviewBaselineAdvanced response matches acknowledge RecordReviewed and RecordOverridden; the parent must preserve or replace those acknowledgements when removing relay variants.

## Artifacts and Notes

Evidence belongs in the assigned report directory. Product docs and embedded skills are outside this slice. Docs that need updates include turn-review.md, sessions.md, cli-reference.md, api-reference.md and the embedded mj configuration reference: manual review covers committed work and reports verdicts by mailbox, and no forward/dismiss API remains.

## Interfaces and Dependencies

TurnReviewHost::start takes only the session ID and returns begin_push errors unchanged. ReviewSlot holds delivery progress and retry timing, while TurnReviewDriver owns target and role outcomes. ReviewerAction::CaptureDelta carries only repository paths; RelayClient adds compatibility scope and baselines at the wire boundary. Resolution contains Completed (serialized as dismissed) and Cancelled. ReviewRequest contains capture, reviewer start/close and close. The database keeps all historical columns.
