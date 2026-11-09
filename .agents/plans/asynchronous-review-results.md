# Deliver reviewer results as they finish

This ExecPlan follows `.agents/PLANS.md` and is a living document.

## Purpose / Big Picture

When several reviewers run, the requesting agent should receive useful reports as soon as each reviewer finishes. Each early report says `Review 1/N` (the number of finished reviewers out of the assigned total), identifies its reviewer, and explicitly says it is partial. Clean reports remain silent until the final combined result. The final result explicitly closes the review, including when the last reviewer is clean or failed.

## Progress

- [x] (2026-11-09) Trace reviewer completion, durable state, mailbox delivery, and recovery.
- [x] (2026-11-09) Save each partial notification atomically with the reviewer outcome and extend final result rendering.
- [x] (2026-11-09) Validate ordering, clean and null silence, write failure, restart, and final closeout: 69 controller/database tests and 7 core mailbox tests passed.
- [ ] Complete required checks, commit on the current branch, request review, and push to origin/master.

## Surprises & Discoveries

The host already serializes reviewer outcomes and saves them before acknowledging handback. Its final delivery uses a durable mailbox with unique keys. The mailbox already supports plain text, so partial results need neither a relay protocol change nor a database migration.

The original `wait_for_child` test helper compared the first running reviewer with the first registration, even though reviewers start independently. The expanded clean-result cases exposed this pre-existing assumption. It now checks that the observed child belongs to any durable registration. The first focused run passed 66 existing tests; the next run passed the new out-of-order and real-database atomicity cases after review text rendering was corrected.

## Decision Log

- Decision: Generate partial messages from the transition between the previously saved review group and the group being saved, inside the same database transaction.
  Rationale: An accepted outcome and its notification must survive together; comparing durable outcomes prevents repeated notifications after a restart without adding a second delivery state machine.
  Date/Author: 2026-11-09, Codex.
- Decision: Preserve the final combined report and push authorization rules. Deliver only findings early; clean or missing reports and reviewer failures stay quiet until the final result. A partial result never authorizes a push or an override.
  Rationale: Early findings are useful immediately, while final coverage and override decisions still require the entire review.
  Date/Author: 2026-11-09, Codex.

## Outcomes & Retrospective

Implementation and focused validation are complete. The database integration test proves that an outbox failure rolls back all repository notifications and the reviewer outcome together, while reload and retry produce no duplicate notification. The host golden proves a partial arrives before its peer, survives host restart, and is followed by an explicitly final aggregate. Full validation and required commit/review/push are pending.

## Context and Orientation

`mj-review/src/driver.rs` owns reviewer outcomes and aggregates the final verdict. `mj-controller/src/review_host/reviewer_child.rs` serializes outcome changes through the persistence task in `mj-controller/src/review_host/events.rs`. The production environment saves through `mj-controller/src/database/turn_reviews.rs`; that module will compare old and new groups and enqueue partial mailbox messages in its transaction. `mj-controller/src/review_host/resolve.rs` delivers the final structured verdict. `mj-core/src/mailbox.rs` renders structured verdicts to agent-facing text. A mailbox is the durable queue of messages to an agent; its outbox table prevents duplicate keys.

## Plan of Work

First, factor saving review state so the database writer can use the same connection for state and mailbox writes. Compare saved outcomes with incoming outcomes only while peers remain outstanding. Produce one plain-text partial per affected repository with the captured commit range, completion count, reviewer identity, and findings or failure reason. Reuse the existing outbox insertion helper and notify its delivery task only after commit.

Then extend final structured verdict rendering with an explicit closeout. Render review-source plain text as review context, keeping the existing escaping of reviewer text. Update the existing out-of-order host regression to expect an early report and an ordered final synthesis; extend the host fake to model the same durable transition. Add database integration coverage for atomic rollback and duplicate suppression. Cover silence for clean or missing reports, a clean last reviewer, and recovery using isolated existing test fixtures.

## Concrete Steps

Run from the repository root. Use ordinary Cargo output paths and the configured mbx cache; do not redirect target directories. Run all Cargo tests outside the sandbox. During implementation use:

    cargo test -p brokk-mj-controller --lib -- review_host::tests:: database::tests::review_
    cargo test -p brokk-mj-core --lib -- mailbox::tests::
    cargo fmt --all -- --check

After the last implementation round run the required touched-crate suites and workspace `cargo clippy --all-targets -- -D warnings` once. Existing test fixtures use isolated database directories. Any executable daemon or CLI test must use `--instance async-review-test`; never use the default instance. Commit only task files on the current branch and request review before pushing `HEAD:master`.

## Validation and Acceptance

With two pending reviewers, finishing the second reviewer with findings must yield a `Review 1/2` partial before the first finishes. Finishing the first then produces one final aggregate containing both reports in launch order and an explicit final-result closeout. A clean early report produces no mailbox event. A final clean report still closes out earlier findings. An injected outbox failure must roll back both the outcome and the notification; retrying and reloading the saved group must produce no duplicate partial. Existing review lifecycle and coverage regressions must continue passing.

## Idempotence and Recovery

Partial event keys include the review identity, reviewer role, and repository. Durable state comparison and the existing mailbox uniqueness constraint prevent duplicates. Transaction failure leaves both the old group and outbox unchanged, so ordinary handback retry remains valid. Final delivery retains its existing retry and recovery behavior. No live instance migration or installation is part of this task.

## Artifacts and Notes

Expected early message begins `Review 1/2 — partial result` and says reviewers are still running. Expected final message explicitly says `Review finished; this is the final result. No reviewers are still running.`

## Interfaces and Dependencies

Keep reviewer classification in `mj_review::driver::TurnReviewDriver`. Add a shared controller helper that accepts the owner session ID and previous/current `ActiveReviewGroup` records and returns `Vec<mj_core::mailbox::MailboxEvent>`. Use the existing `MailboxEventBody::PlainText` and `database::mailbox::enqueue_mailbox_event_with` to retain compatibility with existing workers. No new crates, persistent fields, or protocol variants are needed.

Initial plan recorded on 2026-11-09 after tracing durable outcome and notification ownership.

Revision on 2026-11-09: render progress as review context, suppress absent reports as well as clean reports, and normalize reviewer identities in the golden so parallel launch order cannot make the expected output vary.
