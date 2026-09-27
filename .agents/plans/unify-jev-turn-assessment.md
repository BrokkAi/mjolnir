# Unify Jev turn assessment

This ExecPlan is maintained under `.agents/PLANS.md`. The worker (the process running a session's harness) owns classification and durable action admission. The daemon (the control process serving clients) consumes those decisions and resolves subscription reset deadlines.

## Purpose / Big Picture

Every completed turn, including a turn started autonomously by Codex, must receive the same provider-recovery, quota, activity, and authorized-continuation assessment. In session 0b4b14fb06f2c331d802b44d8e35c660, Jev returned 91% retryability but no retry was armed because the autonomous turn lacked a prompt-command retry record. A separate daemon classifier then declined continuation. The finished system has one durable worker decision, visible through diagnostics, and cannot lose a positive retry verdict at this boundary.

## Progress

- [x] Investigated incident and existing worker/daemon classification.
- [x] User selected unified turn assessment and authorized implementation, then push to origin/master.
- [x] Implement shared contract and evidence retention.
- [x] Implement durable worker assessment and atomic action admission for every completion origin.
- [x] Consume worker decisions in daemon; retain explicit legacy protocol handling.
- [x] Update hosted proxy, diagnostics, compatibility and regression tests.
- [x] (2026-09-27) Run isolated validation and prepare the validated change for commit and push to origin/master.

## Surprises & Discoveries

The worker's `RetryAssessmentStarted` projection requires a real prompt command; `HarnessTurnSettled` never creates one. Jev's existing question also tells it to answer NO when completion metadata is absent. The worker's process-local transcript summary is truncated and cannot replace the daemon's authorization history. A valid uncertain verdict currently leads to repeated classification of unchanged evidence.

## Decision Log

Use a shared worker-owned assessment rather than adding an autonomous-turn special case. Keep classification distinct from current admission: results remain durable while actions wait for safe execution. Preserve provider backoff, continuation limits, and quota deadline policy. Jev judges three independent axes: failure, input requirement, and remaining work. Missing authorization history prevents ordinary continuation, not current provider-error assessment. Running silence checks remain input-only consumers. The user explicitly forbids upgrading the live instance; all runtime tests use `--instance jev-unified` or existing isolated test directories. Push only after validation is complete.

## Outcomes & Retrospective

Implementation and validation are complete. Every physical completion now creates a worker assessment independently of retained prompt-command identity. The worker admits provider retries under its serialized relay owner; new daemons consume that verdict without a second classification. Durable evidence, request backoff, cached uncertainty, checkpoint state and diagnostics survive restart. The full Rust suite, final daemon continuation regressions, Clippy, formatting, proxy tests, TypeScript checking, and proxy deployment dry run pass. Named-instance startup and upgrade tests passed; no live installation or live store was upgraded. Final delivery is a commit on master followed by the user-authorized push to origin/master. Hosted v5 deployment remains a prerequisite for a future client release.

## Context and Orientation

`mj-core/src/activity/verdict.rs` defines worker evidence and parsing; `mj-core/src/continuation.rs` defines existing daemon evidence and durable continuation limits. `mj-worker/src/relay/commands.rs` and `mj-worker/src/relay.rs` record prompted and autonomous completions. `mj-worker/src/relay/verdict.rs` and `mj-worker/src/worker_runtime/unix/dispatch.rs` classify and apply worker verdicts. `mj-core/src/relay/snapshot/apply.rs` is the deterministic journal projection. `mj-controller/src/daemon/continuation.rs` currently independently classifies continuation and quota. `mj-transcript` owns shared conversation interpretation. `services/jev-proxy` validates hosted requests and shares question JSON with Rust.

## Plan of Work

First introduce a versioned three-axis contract with a deterministic action policy and shared bounded evidence. Retain whole user instructions since context reset, whole recent assistant messages, final completion metadata, omission markers, and runtime facts within 64 KiB. Persist the context through snapshots and recovery; seed missing upgraded context from the controller at a checked frontier.

Next unify prompted and harness completion under a durable turn identity and evidence revision. Pending, assessed, deferred, scheduled, consumed, superseded, and failed classification states must survive restart. Classification must not depend on action eligibility. Admission, cancellation, and scheduling run under the relay owner; a result cannot be applied to a different turn. Stable generated command identities preserve at-most-once admission. Cache valid uncertain answers until relevant evidence changes; retry only request failures using bounded exponential backoff.

Then replace daemon classification for new workers with consumption of their published decision. Preserve daemon ownership of quota refresh and reset-time preparation, with bounded resumable work and worker-side admission. Keep legacy classification only for older negotiated protocols. UI/wait consumes worker state. One diagnostic record explains the classification and actual action or deferral.

Finally add the hosted endpoint while preserving released endpoints, bump relay protocol and durable format for incompatible state, retain forward readers and checkpoint seed compatibility, and test upgrade safety. Database migration 58 is breaking because stored relay observation and command JSON now has variants older binaries cannot read. Relay protocol 25 and durable format 15 likewise prevent older owners from silently dropping assessment state.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir` on the existing branch. Do not redirect Cargo targets or change mbx caching. Use focused tests during development, then elevated `cargo test` and `cargo clippy --all-targets -- -D warnings` in the dev profile. In `services/jev-proxy`, run `npm test` and `npm run check`. Any daemon/CLI runtime invocation of the new build must include `--instance jev-unified`. Never install, replace, or restart the host's live Mj binary/daemon. Stage only changed files, commit coherent validated checkpoints, and push the final result to origin/master.

## Validation and Acceptance

An autonomous goal's text-only capacity refusal with confident transient-provider classification must schedule one delayed Continue regardless of uncertain work/input scores. Prompted, steered and autonomous equivalents must behave identically. Cover quoted failures, authentication/local errors, quota, genuine and redundant permission requests, missing history, oversized evidence, malformed replies and disabled Jev. Race classification/timers with user input, cancellation, goals, background completion and checkpoints. Reopen at durable boundaries and verify pending classification, backoff and accepted action identities survive without duplicates. Verify old protocols and checkpoint restores, direct/hosted parity, diagnostic outcomes, and isolated daemon handoff.

## Idempotence and Recovery

Classification requests are read-only and may be retried after restart. Actions use stable IDs derived from durable assessment identity and are atomically admitted once. New user input supersedes prior automatic work. Persisted inference cannot prove runtime idle after restart. New binaries must read shipped formats while older binaries refuse incompatible new stores. Keep old hosted contracts available until released clients retire.

## Artifacts and Notes

Incident evidence: worker decision `323464-1790535986172-00000000000000000002` reported `retryable_server_error=0.91`, `reason=keep_current`; daemon decision `1550843-1790529848282-00000000000000000033` independently reported unfinished 0.89 and no-input-needed 0.75. These are diagnostics only; tests use synthetic fixtures, not live session data.

## Interfaces and Dependencies

The unified contract has independent failure (none/transient_provider/quota/other/unclear), input (none/redundant_request/required/unclear), and work (finished/authorized_unfinished/waiting/unclear) choices with confidence. Automation uses 0.90, activity uses 0.85. Durable turn identity distinguishes prompt and autonomous origins without requiring a handled prompt for autonomous turns. Operational state publishes the assessment and its action reason. Reuse existing HTTP transport, subprocess helpers, journal, quota recovery and command admission; introduce no new crate.

Revision note: implementation now retains bounded context and assessments through journals and native checkpoint restores, publishes a transcript-free API status, and writes one worker-owned diagnostic record through action admission. Request failures back off durably; uncertain verdicts stay cached. Explicit required input outranks automatic recovery. Full-suite validation found legacy fixtures and async stack growth; published assessment payloads are boxed to keep relay snapshots compact. The v5 proxy must be deployed before releasing clients using it; released endpoints and their evaluation scripts continue using v4. Live installation and store remain untouched.

Validation evidence (2026-09-27): `cargo test --no-fail-fast` completed successfully, including 1,868 controller tests, the autonomous provider retry and checkpoint-seed regressions, and named-instance CLI startup/upgrade tests. `cargo test -p brokk-mj-controller --lib daemon::continuation::tests` passed all nine tests after the final context-recovery timer change. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, proxy `npm test`, `npm run check`, and `npm run deploy:dry-run` succeeded. Historical schema migrations and checkpoint reuse now include the new durable format. Diagnostic inspection during development was removed. Test logs are in `/mnt/optane/jev-validation.log`, `/mnt/optane/jev-validation-clippy.log`, and `/mnt/optane/jev-continuation-final.log`.
