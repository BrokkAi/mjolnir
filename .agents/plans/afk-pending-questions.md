# Apply AFK policy to questions already waiting

This ExecPlan follows `.agents/PLANS.md` and must remain current as implementation and validation proceed.

## Purpose / Big Picture

Starting AFK (operator away) must apply the existing refuse-once policy to unanswered questions already waiting in a worker, exactly as it does to questions arriving while away. The worker must publish the existing away resolution and send the harness its existing refusal response. Repeated questions remain available to answer under the current policy.

## Progress

- [x] (2026-10-27 18:05Z) Trace activation, worker request handling, and persistence; identify the check/registration gap and waiters that never wake.
- [x] (2026-10-27 18:06Z) Select one synchronized owner for presence, refusal memory, pending registration, answers, and withdrawal.
- [x] (2026-10-27 18:20Z) Implement the worker ownership change; focused AFK and answer/cancellation regressions pass.
- [x] (2026-10-27 18:26Z) Expanded transport regression passes for ordinary questions, tool permissions, normalized plans across harnesses, Grok plans, repeats, late answers, and native-child questions.
- [x] (2026-10-27 18:22Z) Independent source review found no actionable correctness issues; late-resolution persistence coverage extended in the existing database integration test.
- [x] (2026-10-27 18:29Z) Merge origin/master through bf04bc109; restore AFK work with clean automatic merges in dispatch, session, and tests.
- [x] (2026-10-27) Final merged-tree worker/controller suites pass: 2,914 passed, 27 ignored. Workspace Clippy and formatting pass.
- [x] Deliver the validated change on the current branch to origin/master; this plan is included in the delivery commit.

## Surprises & Discoveries

AFK already has a revisioned presence state and per-agent refusal memory in `mj-worker/src/acp/operator_away.rs`. Its ingress decision precedes separate registration in `permissions.rs`, so scanning forms from a UI or daemon would leave a race. Presence reaches workers through existing revision delivery and reconnect paths; these need no new protocol.

The initial regression run compiled concurrently with the production fix and passed. It does not demonstrate a failing baseline. The baseline defect was established by tracing the old presence setter and its separate pending map; final transport regressions directly exercise the corrected ordering.

During final validation, fetching origin revealed concurrent worker changes through `bf04bc109`, including the same dispatch and session files. Pause checks and merge upstream before validating the integrated tree.

The merge fast-forwarded master and all AFK edits reapplied cleanly. Final checks run as one combined worker/controller suite, followed by workspace Clippy and formatting, so only one child owns builds.

## Decision Log

- Decision: Serialize registration, presence, refusal memory, human acceptance, and withdrawal under one worker-owned lock, using an internal typed operator-away answer.
  Rationale: This makes activation versus question arrival and activation versus human answering have a single winner, and preserves accepted answers during cancellation.
  Date/Author: 2026-10-27, Codex.
- Decision: Reuse existing runtime requested/resolved events and harness-specific refusal mappings, with no schema or wire changes.
  Rationale: The normal projection already recognizes the away action and owns capture and return mail.
  Date/Author: 2026-10-27, Codex.

## Outcomes & Retrospective

The worker now atomically registers questions and arbitrates AFK, human answers, and withdrawal. Focused tests pass for fresh and already pending requests across all ACP paths, native-child scope, insertion order, bridge cleanup, accepted replies, and tool cancellation. A database integration test includes a question requested before AFK and resolved while away. Independent review found no actionable correctness issues. Final merged-tree worker/controller suites pass (2,914 passed, 27 ignored), as do workspace Clippy and formatting. No schema or protocol changes were needed.

## Context and Orientation

The daemon is the control process that persists AFK state. The worker runs the harness, the external agent process, and owns unanswered ACP requests. ACP is the Agent Client Protocol used for questions and permissions. `mj-worker/src/acp/operator_away.rs` stores presence and refusal identities. `permissions.rs` stores pending responders and arbitrates cancellation and answers. `drive.rs` handles ordinary forms, tool permissions, plan permissions, and Grok plans. `session.rs` manages the bridge lifetime and user answers. `plan_tests.rs` has a real duplex ACP probe across harness kinds. `mj-controller/src/database/events.rs` tests durable away captures and replay.

## Plan of Work

First replace separate AFK checks and pending inserts with atomic registration carrying the requesting agent and refusal identity. Newer away transitions consume eligible pending entries in registration order, using the same per-agent refuse-once memory. Human answers and cancellation take entries through the same owner. Keep bridge cleanup scoped to its bridge, and retain refusal memory across bridge restarts. Route fresh and already pending requests through the same responder tasks; publish Requested before Resolved and use the away action with no human reply. Preserve managed-child forwarding, harness-specific rejection or cancellation, stale-revision handling, and repeat exposure.

Extend transport tests for activation after Requested, subsequent repeats, native child identity, and human/away winner ordering. Inspect the durable capture test for pre-AFK request followed by away resolution, changing only the existing test if necessary. Review integrated locking, lifecycle cleanup, and cancellation before full validation.

## Concrete Steps

From the repository root use the existing Cargo shim, which delegates to mbx without changing build storage:

    cargo test -p brokk-mj-worker operator_away
    cargo test -p brokk-mj-worker accepted_elicitation_replies_survive_concurrent_withdrawal
    cargo test -p brokk-mj-controller operator_away_mail_is_atomic
    cargo test -p brokk-mj-worker
    cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Only run the controller suite if that crate changes. Keep all live binary invocations on an isolated named instance; transport tests retain their own isolated state. After acceptance stage only changed files, commit on the current branch, and push HEAD to origin/master. If upstream moved, fetch and merge origin/master, resolve conflicts, and push without rebasing.

## Validation and Acceptance

The new regression must observe a pending question while present, start AFK, receive exactly one away resolution and the same protocol refusal as fresh AFK ingress, and find the old form no longer externally answerable. Equivalent repeat requests must remain exposed. Human acceptance before AFK must survive; AFK acceptance before human response must reject the late answer. Existing cancellation and refusal-memory regressions must pass. Full touched-crate suites and workspace Clippy must pass after the final edits; rerun only failures while iterating.

## Idempotence and Recovery

Tests must not access the live store or default daemon. No migrations or installation are needed. Do not redirect target storage or alter mbx configuration. Use normal Git merge recovery if a concurrent push moves master.

## Artifacts and Notes

Child handbacks and detailed logs are under the session's `.mj-agents` report directories, outside tracked product documentation. Record decisive test outcomes here before delivery.

Final validation used `cargo test -p brokk-mj-worker -p brokk-mj-controller`: controller library 2,173 passed and 17 ignored; worker library 718 passed and 10 ignored; worker main and integration binaries 23 passed. All tests passed. Clippy identified a single-pattern match in the new regression helper; after the equivalent `if let` correction, the affected regression, workspace Clippy, and formatting passed. The full suites were not repeated for that mechanical test-only change.

## Interfaces and Dependencies

Use the existing `OperatorAway`, `AwayRequest`, pending elicitation owner, Tokio oneshot channels, and RuntimeEvent requested/resolved events. Keep operator-away resolution an internal typed result distinct from a human `ElicitationResponse::Cancel`, which can terminate a turn. No dependency, schema, or protocol additions are intended.

Initial plan written after the ownership diagnosis to coordinate disjoint production and regression work.

Updated after implementation: recorded focused passes and the remaining transport, persistence, and final validation work.

Updated after independent review and expanded transport coverage: recorded passing focused tests, full validation in progress, and the baseline-run limitation.

Updated after fetching upstream: added integration of overlapping AIR changes ahead of final validation.

Updated after integration: recorded the clean merge and consolidated final validation ownership.

Updated after final validation: recorded passing suites, focused rerun after the mechanical lint correction, Clippy, and formatting.
