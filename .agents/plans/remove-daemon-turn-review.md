# Remove daemon turn review

This plan follows `.agents/PLANS.md`. The daemon is the session control process; the worker owns repository capture. Manual review must review committed work through the worker's push gate, just like the agent request_review tool.

## Progress

- [x] Read removal map and slice-1 report.
- [x] Fix session reviewer enablement.
- [ ] Route manual starts through begin_push; replace turn-only host tests.
- [x] Add compatible revision 112 cleanup for obsolete stored turn groups.
- [ ] Remove turn driver, forwarding, follow-ups, baselines and host events.
- [x] Update review prompt wording.
- [ ] Add lenient replay parsing after removing turn request kinds.
- [x] Validate checkpoint with formatting, check --tests, affected-crate clippy, reviewer-choice and old-row regressions, and prompt tests; commit checkpoint.
- [ ] Complete removal and full touched-crate suites.

## Context and Orientation

`mj-controller/src/review_host` owns orchestration. `mj-core/src/review` defines persisted driver state and terminal views. `mj-review` runs the pure review driver. Worker and relay files belong to another agent and must remain untouched. Relay requests must retain explicit PushableCommits scope and empty baselines until the parent removes obsolete variants.

## Plan of Work

First make manual starts use begin_push and correct ReviewConfig::for_session. Retire old turn groups before deserialization through migration 112, preserving columns and push groups. Then delete turn-only driver and host operations, collapse push state into ReviewSlot, and preserve old terminal decoding by emitting allowed and known phase tags. Keep Rust Turn identifiers for a later mechanical change.

## Concrete Steps

Run cargo fmt --all, cargo check --tests, targeted TZ=UTC cargo tests and clippy --tests -- -D warnings for changed crates. Cargo tests require elevated execution and isolated data. Do not redirect Cargo output directories or install builds.

## Validation and Acceptance

Manual review must capture committed work and deliver the same mailbox verdict as request_review. Its refusal must reach the requesting user. Stored turn rows must not prevent unrelated push review recovery. Session reviewers enable push review unless explicitly Off.

## Idempotence and Recovery

Migration 112 records its revision transactionally and preserves the compatibility floor at 111. Retry interrupted preparation through the existing push path. Keep columns readable by prior daemons.

## Surprises & Discoveries

The host start method can share begin_push directly without changing client wire types.

## Decision Log

Use a compatible cleanup migration because removed orchestration is no longer meaningful and existing columns remain readable. No protocol bump is needed for routing changes alone.

## Outcomes & Retrospective

Compatibility preparation is implemented. Manual routing and the remaining turn pipeline still need deletion. The manual method can delegate to begin_push, but this must accompany replacement of the old turn-only host tests.

## Artifacts and Notes

Evidence belongs in the assigned report directory. No product docs or worker files are owned here.

## Interfaces and Dependencies

TurnReviewHost::start keeps its public signature and returns begin_push errors unchanged. Revision 112 retains the minimum compatible revision 111.
