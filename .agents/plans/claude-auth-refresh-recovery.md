# Recover Claude turns interrupted by login rotation

This ExecPlan follows `.agents/PLANS.md` and remains a living record.

## Purpose / Big Picture

Issue #1515 reports Claude turns failing with Authentication required between a host login refresh and delivery of the replacement login. The worker (the process running one agent session) must resume that interrupted work when its access token changes, without requiring a person to send Continue. An unchanged or missing login must still report its authentication failure.

## Progress

- [x] (2026-11-12) Claimed #1515 and traced credential synchronization, prompt completion, and durable retry admission.
- [x] (2026-11-12) Recorded access-token evidence on failed turns and implemented worker-owned durable recovery, with install wakes and startup checks.
- [x] (2026-11-12) Covered both socket delivery orders, restart, duplicate wakes, unchanged tokens, newer input, cancellations, questions and pending checkpoints.
- [x] (2026-11-12) Validated the previous iteration and resolved its review findings.
- [x] (2026-11-12) Validated cached-identity prompt progress, recovery and all worker startup cases; final full worker suites, workspace Clippy and formatting passed. Prepared final review/publication.

## Surprises & Discoveries

Credential synchronization explicitly tells the user to retry even after it pushes a newer login. The ACP Authentication required code is preserved as a numeric diagnostic code, while credential detection currently recognizes the warning marker but not that diagnostic code itself. A push can precede the final failure notification, so a recovery that only reacts to pushes after completion has another race. Queued checkpoint barriers are not yet reflected as active barriers in activity facts; recovery also consults the existing pending-checkpoint owner. Superseding input is compared with the failed command’s acceptance, so input queued and removed during that turn still fences recovery.

## Decision Log

Decision: use evidence of an actual changed access token, recorded with the failed turn, rather than a guessed refresh time window. Rationale: works across daemons sharing a host login, delayed synchronization, and either event ordering. The worker alone admits the recovery against its current durable completion and activity. Date/author: 2026-11-12, Codex.

Decision: retain only a hash of the access token, never credential bytes, in the optional turn diagnostic field. Rationale: existing journal/checkpoint persistence preserves the evidence without a database migration or a new credential transport. Existing diagnostic readers accept additional optional fields. Date/author: 2026-11-12, Codex.

## Outcomes & Retrospective

Implementation and validation are complete. The core/controller full suites passed (638/2569 tests, 3/19 intentionally ignored). The final cached-identity worker run passed all 834 library tests (11 intentionally ignored), its binary tests and all integration suites. Workspace Clippy with warnings denied, rustfmt and diff checks passed.

Review removed both I/O hazards and the unnecessary per-prompt prerequisite: startup observation is independent, installations publish the shared identity, and dispatch only samples memory. A full run caught reentrant relay locking in the launch initializer; all launch facts are now captured under one guard before constructing `LaunchSpec`, and all six affected startup regressions pass. An earlier unchanged Git rendezvous fixture failed once, then passed in isolation and subsequent full runs. The result is prepared for the repository’s final review and push workflow. Installation is outside this request.

## Context and Orientation

`mj-worker/src/acp/session.rs` sends each prompt to the harness bridge and emits its completion. `mj-core/src/diagnostic.rs` defines the diagnostic persisted with a failed turn. `mj-worker/src/relay/commands.rs` owns atomic command admission and `mj-worker/src/worker_runtime/unix/dispatch.rs` coordinates runtime events and dispatch wakes. `mj-worker/src/worker_runtime/unix.rs` serves credential installs independently of the running turn. `mj-core/src/continuation.rs` identifies generated prompts so retries cannot become fresh user authorization. Credential interpretation belongs in `mj-core/src/credentials.rs`.

## Plan of Work

Sample the shared `ClaudeLoginIdentity` at prompt dispatch and attach that fingerprint only to an authentication failure. Prompts never read the credential file or wait for optional recovery evidence. A supervised, read-only startup task initializes the identity without delaying work. Each successful credential installation publishes the access-token fingerprint from the installed bytes before acknowledging its request. An installation mutex gives file commit and identity publication one order across concurrent clients; a revision check prevents a delayed startup sample from replacing an installed identity.

The relay alone decides whether a failed completion remains eligible and whether the current identity differs. Its generated retry ID includes the completion ordinal. Newer input, cancellation, questions, goals, activity and checkpoint barriers take precedence. Recheck this decision after runtime events, installation wakes, startup observation and background judgments. The startup read uses a named thread outside Tokio’s shutdown-waited blocking pool, reports errors/panics, and is supervised through its async result. On restart, recheck persisted failures using the new startup observation. A recovery posts a transcript notice and supplies no new authorization.

## Concrete Steps

Work from the repository root on the current master branch. Use normal Cargo storage and mbx configuration unchanged. Run focused tests first with `TZ=UTC cargo test -p brokk-mj-worker <filter>` outside the sandbox; run final affected-crate suites and `cargo clippy --all-targets -- -D warnings` once after the final edit. Existing fixture tests retain their isolated directories. Any real binary/daemon smoke test must use `--instance auth-refresh-1515`.

## Validation and Acceptance

Regression cases must prove that a failed Claude prompt resumes exactly once after the token changes, whether delivery preceded or followed failure, and that reopening the durable relay retains this behavior. Unchanged credentials, refresh-token removal with the same access token, unrelated errors, and newer user input must not produce a retry. A bridge fixture must prove the diagnostic captures the dispatch-time identity rather than a newer installation published before failure. A blocked startup observation must not delay primary prompts or runtime shutdown; a stale observation must not overwrite an installation. Existing generated-prompt tests must continue to exclude the retry from authorization.

## Idempotence and Recovery

The durable retry command ID is derived from the completion ordinal; repeated wakes cannot submit it twice. If the worker crashes after accepting it, its existing durable command recovery applies. Missing or unreadable login evidence does not authorize a retry. No live store, login, daemon, or worker is changed by validation.

## Artifacts and Notes

The issue evidence records failures 3–18 seconds after rotation and replacement pushes about 25 seconds later. No assumption that old access tokens remain valid is needed for the fix.

## Interfaces and Dependencies

Use the existing credential hash, shared activity predicates, durable relay command queue, and generated-prompt declaration. Add no dependency, migration, timer, rate limit, or external service.

Revision 2026-11-12: initial plan after tracing the observed failure and reverse delivery ordering.

Revision 2026-11-12: implemented worker recovery; socket and ACP fixtures pass both event orders and preserve the starting login. Expanded control cases to cover a pending checkpoint and input removed before failure. Final validation remains pending.

Revision 2026-11-12: addressed review responsiveness findings with a supervised reader and cancellable prompt preparation. Real FIFO fixtures prove progress/shutdown while reads are blocked. Recovery uses the canonical completed turn, including delegated assignments, without altering authorization.

Revision 2026-11-12: finalized validation evidence, including the isolated rerun of the unchanged Git rendezvous fixture; formatting and workspace Clippy passed. The implementation is complete and prepared for final review/publication.

Revision 2026-11-12: final review exposed runtime-drop waiting on cancelled Tokio blocking jobs. The credential reader now uses an independent read-only thread and reports panics/errors. A real runtime-drop regression keeps its FIFO blocked until shutdown is observed. All 834 worker tests (11 ignored), worker integration suites, workspace Clippy and formatting passed on the final implementation.

Revision 2026-11-12: removed per-prompt filesystem reads after review identified that supplemental evidence could delay all Claude work. Added one shared cached identity: installations publish it in commit order, initial observations carry a revision guard, and dispatch samples it without I/O. Replaced the preparation cancellation path and watch-driven rereads with direct recovery checks. Validation of this simplification is in progress.

Revision 2026-11-12: finalized the cached-identity design. Startup exposed duplicate relay locks held across struct initialization; collecting all launch facts under one guard removes that deadlock structurally. Final full worker validation (834 passed/11 ignored plus integrations), workspace Clippy, rustfmt and diff checks passed.
