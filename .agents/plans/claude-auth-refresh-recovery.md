# Recover Claude turns interrupted by login rotation

This ExecPlan follows `.agents/PLANS.md` and remains a living record.

## Purpose / Big Picture

Issue #1515 reports Claude turns failing with Authentication required between a host login refresh and delivery of the replacement login. The worker (the process running one agent session) must resume that interrupted work when its access token changes, without requiring a person to send Continue. An unchanged or missing login must still report its authentication failure.

## Progress

- [x] (2026-11-12) Claimed #1515 and traced credential synchronization, prompt completion, and durable retry admission.
- [x] (2026-11-12) Recorded access-token evidence on failed turns and implemented worker-owned durable recovery, with install wakes and startup checks.
- [ ] Exercise both orderings of login delivery and prompt failure, restart, duplicate wakes, and superseding user input.
- [ ] Run formatting, affected-crate tests, workspace Clippy; commit and push to master.

## Surprises & Discoveries

Credential synchronization explicitly tells the user to retry even after it pushes a newer login. The ACP Authentication required code is preserved as a numeric diagnostic code, while credential detection currently recognizes the warning marker but not that diagnostic code itself. A push can precede the final failure notification, so a recovery that only reacts to pushes after completion has another race. Queued checkpoint barriers are not yet reflected as active barriers in activity facts; recovery also consults the existing pending-checkpoint owner. Superseding input is compared with the failed command’s acceptance, so input queued and removed during that turn still fences recovery.

## Decision Log

Decision: use evidence of an actual changed access token, recorded with the failed turn, rather than a guessed refresh time window. Rationale: works across daemons sharing a host login, delayed synchronization, and either event ordering. The worker alone admits the recovery against its current durable completion and activity. Date/author: 2026-11-12, Codex.

Decision: retain only a hash of the access token, never credential bytes, in the optional turn diagnostic field. Rationale: existing journal/checkpoint persistence preserves the evidence without a database migration or a new credential transport. Existing diagnostic readers accept additional optional fields. Date/author: 2026-11-12, Codex.

## Outcomes & Retrospective

Implementation and validation are in progress. Installation is outside this request.

## Context and Orientation

`mj-worker/src/acp/session.rs` sends each prompt to the harness bridge and emits its completion. `mj-core/src/diagnostic.rs` defines the diagnostic persisted with a failed turn. `mj-worker/src/relay/commands.rs` owns atomic command admission and `mj-worker/src/worker_runtime/unix/dispatch.rs` coordinates runtime events and dispatch wakes. `mj-worker/src/worker_runtime/unix.rs` serves credential installs independently of the running turn. `mj-core/src/continuation.rs` identifies generated prompts so retries cannot become fresh user authorization. Credential interpretation belongs in `mj-core/src/credentials.rs`.

## Plan of Work

Capture the staged Claude access-token fingerprint immediately before sending a prompt. Attach it only to an authentication failure. Add a generated authentication retry whose identity includes the failed completion ordinal. In the relay, require that completion to remain current, the access token to differ, and normal submission/activity guards to permit continuing; newer input, cancellation, questions, goals, and barriers must win. Recheck after runtime events and credential-install dispatch wakes, reading files off the relay lock. On restart, recheck persisted failures. Emit an automatic-recovery notice in the transcript. Do not replay the original prompt or synthesize new authorization.

## Concrete Steps

Work from the repository root on the current master branch. Use normal Cargo storage and mbx configuration unchanged. Run focused tests first with `TZ=UTC cargo test -p brokk-mj-worker <filter>` outside the sandbox; run final affected-crate suites and `cargo clippy --all-targets -- -D warnings` once after the final edit. Existing fixture tests retain their isolated directories. Any real binary/daemon smoke test must use `--instance auth-refresh-1515`.

## Validation and Acceptance

Regression cases must prove that a failed Claude prompt resumes exactly once after the token changes, whether delivery preceded or followed failure, and that reopening the durable relay retains this behavior. Unchanged credentials, refresh-token removal with the same access token, unrelated errors, and newer user input must not produce a retry. A bridge fixture must prove the diagnostic captures the original token rather than the token at failure time. Existing generated-prompt tests must continue to exclude the retry from authorization.

## Idempotence and Recovery

The durable retry command ID is derived from the completion ordinal; repeated wakes cannot submit it twice. If the worker crashes after accepting it, its existing durable command recovery applies. Missing or unreadable login evidence does not authorize a retry. No live store, login, daemon, or worker is changed by validation.

## Artifacts and Notes

The issue evidence records failures 3–18 seconds after rotation and replacement pushes about 25 seconds later. No assumption that old access tokens remain valid is needed for the fix.

## Interfaces and Dependencies

Use the existing credential hash, shared activity predicates, durable relay command queue, and generated-prompt declaration. Add no dependency, migration, timer, rate limit, or external service.

Revision 2026-11-12: initial plan after tracing the observed failure and reverse delivery ordering.

Revision 2026-11-12: implemented worker recovery; socket and ACP fixtures pass both event orders and preserve the starting login. Expanded control cases to cover a pending checkpoint and input removed before failure. Final validation remains pending.
