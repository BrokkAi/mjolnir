# Classify ambiguous background activity and explain Jev decisions

This ExecPlan follows `.agents/PLANS.md` and must remain current during implementation.

## Purpose / Big Picture

A completed assistant answer must not look busy forever because its harness still lists background tasks. Jev, the optional remote turn classifier, should consider that inventory as evidence and may infer that the agent is idle. Local worker logs must explain the evidence, answer, and application of each decision. Task records and conservative process-lifecycle checks remain intact.

## Progress

- [x] Inspected activity classification, completed-turn scheduling, HTTP client, and default worker logging.
- [x] Implement generation-scoped idle inference and background-aware scheduling.
- [x] Add bounded evidence and decision logging enabled by default.
- [x] Add behavior tests and update user documentation; six HTTP/logging regressions pass.
- [x] Full dev-profile `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and `git diff --check` passed.
- [x] Validate isolated worker behavior, install native/portable binaries, verify startup and exact artifact matches, and preserve the live worker.
- [x] Commit this validated checkpoint on the current branch without pushing.

## Surprises & Discoveries

The completed-turn path skips all background work, has no finished decision, and runs only once. The shared classifier places background inventory above continuation inference. Worker logging defaults to warn, so adding info events alone would not make them visible. Activity also governs safe worker replacement; inferring idle must not erase owned work from that separate safety predicate.

## Decision Log

- Decision: Keep the existing Jev request schema and confidence threshold of 0.85. Replied Finished/User maps to inferred idle; BackgroundWork retains recorded Background or infers Expecting when no tasks exist; other answers preserve activity.
  Rationale: Fix the reported case without a proxy deployment or changes to running-turn input handling.
- Decision: Include bounded input text locally by default, under a dedicated mj_jev logging target.
  Rationale: The user explicitly selected evidence logging for debugging. Never log authentication data.
- Decision: Do not clear task inventory or force replacement of the live worker.
  Rationale: Classification is an inference about agent activity, not proof that a process can safely be destroyed.

## Outcomes & Retrospective

Implemented shared inferred-idle state, conservative safety predicates, generation-aware scheduling with bounded retries, and default local Jev decision logs. All required dev-profile checks passed. The normal installer replaced the controller, native worker, portable musl worker, and voice helper. Both installed workers start and exactly match the built artifacts. The original bifrost-fuzz worker PID 998106 is still alive; it was not restarted and requires an updated worker process to gain the new behavior. No push was requested or performed.

## Context and Orientation

`mj-core/src/activity.rs` owns the shared activity classifier and lifecycle safety predicates. Its `verdict` module bounds the prompt/assistant/tool evidence and translates Jev answers into decisions. `mj-worker/src/relay.rs` owns live task inventory, evidence generation, and published operational state. `mj-worker/src/worker_runtime/unix/dispatch.rs` supervises asynchronous completed-turn requests. `mj-worker/src/acp/verdict_client.rs` performs bounded HTTP requests and handles running-turn classification. The worker stderr stream is captured in each session's worker.log.

## Plan of Work

Add an optional inferred-idle timestamp to activity facts and operational snapshots, absent for old workers and never recovered as a trusted process-local inference. Apply it only after foreground guards and without hiding user shells, goals, or capacity retries. Explicitly retain background commands in has_work_in_flight.

Keep accepted inferences tied to a meaningful evidence generation. Include task identity in invalidation, not just task count. Identical inventory/usage reports must not invalidate a decision. Reclassify completed turns on relevant evidence changes; retry inconclusive/failed requests after 60 seconds, doubling to a 300-second cap. Use supervised requests and bounded timers, reject obsolete responses, and cancel requests when their generation becomes invalid or the coordinator stops.

Log bounded evidence, a process-local request identifier, session, harness, phase, generation, source, latency, verdict/confidence/question probability, decision, and applied/discarded/cancelled/error outcomes. Log skips only at attempts or eligibility transitions. Default worker logging enables mj_jev=info while retaining warn elsewhere; explicit RUST_LOG remains authoritative.

## Concrete Steps

Work in `/home/jonathan/Projects/hel`. Add colocated unit tests for classifier decisions, generation invalidation, retry scheduling, and default logging; extend fake-HTTP coordinator tests for completed replies with old background tasks, errors, cancellation, and new foreground work. Update `docs/src/content/docs/sessions.md`. Run `cargo fmt --check`, elevated `cargo test`, and `cargo clippy --all-targets -- -D warnings` in the dev profile. Build and exercise an isolated updated worker with deterministic fake Jev responses. Commit only task files on the current branch, without pushing.

## Validation and Acceptance

A final answer with four old Claude tasks is sent to Jev. A confident finished answer produces Idle across worker and published state, while all four task controls remain and safe_to_replace stays false. User and background-work decisions, uncertain/error retries, identical reports, replacement of a task without count changes, new prompts, resumed output, stale replies, and responsive shutdown are tested. Default worker logs contain the bounded input and terminal outcome without credentials. No tests mutate the live SQLite store.

## Idempotence and Recovery

All state added is optional and process-local. No database migration is required. A restarted worker must obtain fresh evidence and a new verdict rather than reuse an old inference. Keep the live bifrost-fuzz worker running; new behavior only becomes available when updated worker code runs. Do not claim that installing a binary updates an already-running process.

## Artifacts and Notes

Focused worker tests initially passed all 14 cases. The expanded full suite exposed logging-test issues: f32 confidence formatted through tracing as a long f64, and cancellation cleanup could log after the coordinator returned. Confidence now uses its native display, request guards retain their tracing dispatcher, and orderly coordinator exits cancel and join classifier tasks. All six fake-HTTP/logging scenarios then passed (`/mnt/optane/hel-jev-http-tests.log`).

An optional hosted probe using actual session text was rejected by automatic approval review because of private-content export; that request was not executed. A subsequent synthetic arithmetic probe, containing no session data, was allowed but returned HTTP 403. Live hosted inference therefore remains unverified; deterministic local fake-HTTP tests are the acceptance evidence.

## Interfaces and Dependencies

Reuse ActivityFacts, RelayOperationalState, TurnContext, TurnEvidence, TurnVerdict, and the existing reqwest client. Introduce a completed-turn idle decision and a worker-local scheduler/inference holder. The existing hosted/direct request schema and ActivityState variants stay unchanged; optional operational fields default to absent. No new crate or dependency is needed.

Revision note: implementation additionally invalidates evidence on harness initialization and restart. Inspection showed those lifecycle boundaries were not covered by the older continuation-only guard.

Validation: final full-suite output is `/mnt/optane/hel-jev-tests-final.log`; Clippy output is `/mnt/optane/hel-jev-clippy-final.log`. The full suite includes all six coordinator HTTP/logging tests, generation/restart/retry tests, and preservation of the actual Claude stop target after inferred idle. Validation uses isolated worker coordinators with deterministic fake HTTP responses; no live session was restarted. The only edits after validation were indentation inside tracing/select macros, with format and diff checks passing.

Installation evidence: `scripts/install.sh` succeeded (`/mnt/optane/hel-jev-install.log`). Both `/home/jonathan/.cargo/bin/mj-worker --version` and the installed musl worker reported 2.14.0; byte comparisons against `target/worker/release/mj-worker` and `target/worker/x86_64-unknown-linux-musl/release/mj-worker` succeeded. The live worker process still existed after installation. No live store or harness background-task records were edited.
