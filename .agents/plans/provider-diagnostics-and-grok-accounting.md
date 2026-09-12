# Preserve provider diagnostics and Grok accounting

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Remediate the two latest #986 comments (branch-export authentication and lost Kimi quota diagnostics), #988 (Grok launchers broken by installation relocation), and #989 (missing completed-turn usage). Callers should discover Grok without cache maintenance, distinguish quota exhaustion from other failures, and read accurate future Grok usage through wait and usage APIs.

## Progress

- [x] (2026-09-12) Inspected source and reports; user approved the plan.
- [x] (2026-09-12) Existing real-worker authentication suite: 5 passed.
- [x] (2026-09-12) Installer suite: 11 passed; disposable real Grok install: 1 passed, both final launchers execute.
- [ ] Preserve turn diagnostics and classify Kimi quota exhaustion.
- [ ] Ingest Grok turn usage and preserve provider accounting details.
- [ ] Validate, document, and commit coherent phases on the current branch.

## Surprises & Discoveries

Branch authentication is already repaired by f23c1196, merged into this checkout. `mj-worker/tests/worker_environment.rs` exercises the real worker across clean re-exec. Kimi 0.41.0 ACP forwards auth messages but omits native error structure; the existing native main-agent wire follower already refreshes before prompt completion. Grok launchers are validated before staging rename. Grok completion extension notifications have no handler; consumption currently comes only from ACP prompt response usage.

## Decision Log

User selected reporting quota and stopping, without automatic quota retries, and future-turn usage only without historical backfill (2026-09-12). Preserve existing ModelCapacity retry behavior separately. Keep provider versions pinned. Preserve native full input/output counts: cached tokens and reasoning are subsets. Store reported turn cost separately from cumulative provider session cost. Do not remove leased runtimes during invalid-cache recovery.

## Outcomes & Retrospective

Implementation is pending. Record validation and material limitations here as milestones complete.

## Context and Orientation

`mj-worker/src/worker_runtime/harness.rs` installs target-local managed executables under an installer lock and holds shared runtime leases. `mj-worker/src/acp.rs` receives ACP messages and emits RuntimeEvent completions. `mj-worker/src/acp/kimi_tasks.rs` parses the native main-agent wire stream; `mj-worker/src/worker_runtime/unix.rs` supervises its reads and enriches events before relay recording. Shared serialized types live in mj-core (acp, relay/snapshot, state, usage); projection folds durable relay events into controller database records. `mj-controller/src/server/api.rs` resolves wait outcomes and exposes usage. Native provider details belong to the worker; shared pure interpretation belongs to mj-core.

## Plan of Work and Milestones

First validate the existing branch-export tests and fix Grok runtime finalization: rewrite internal absolute links relative to their parents, validate after rename, then publish the manifest. Invalid cached runtimes repair automatically under exclusive leases, with active installations deferred. Test relocated launchers after staging disappears, invalid caches and leases.

Second introduce optional serializable turn diagnostics through RuntimeEvent, RelayCommandOutcome, MaterializedTurnOutcome and wait responses. Preserve ACP errors; enrich Kimi from correlated native main-agent turn records during the existing forced completion refresh. Classify explicit usage exhaustion as QuotaLimit without scheduling ModelCapacity retry. Prove ordinary auth/403 failures remain errors and historical or child errors never attach to new turns.

Third handle Grok completion notifications and associate usage with a live prompt. Reconcile standard and extension reports without summing duplicates. Persist normalized full-turn usage with optional provider cost/timing/model details, including original integer USD ticks (1e10/USD) and completeness flags. Preserve unknown counters and incomplete coverage. Test transport ordering, multiple prompts, replay, exact totals and database reopening.

Finally update docs/src/content/docs/api-reference.md, run full validation, and commit all remaining changes. Do not backfill old sessions, automatically restart active sessions, or change harness pins.

## Concrete Steps

Work in /home/jonathan/Projects/hel2. Run elevated `cargo test -p brokk-mj-worker --test worker_environment`, focused module tests, then elevated `cargo test -- --test-threads=1`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all --check`, `node scripts/check-crate-boundaries.mjs`, and documentation/package checks. Keep logs in target/. Commit only task files on the current branch; no new branch, rebase, or pull request.

## Validation and Acceptance

Branch fixture pushes reach a local bare remote after production login cleanup. Grok fixture launchers execute from final installation paths with staging absent and invalid caches recover without manual cleanup. Kimi wait reports quota_limit plus provider explanation/reset without retry; other failures retain their actual diagnostic. Grok wait/usage expose exact tokens, honest completeness, provider cost and timing; persisted results are stable across replay/restart. Use disposable live sessions/cache for provider smoke checks where available; never modify existing sessions.

## Idempotence and Recovery

Optional fields deserialize absent historical values. Preserve relay compatibility and event integrity. Installer publication occurs under the existing lock, and failed validation leaves no completed manifest. Runtime leases prevent removal of files used by live processes. Use shared subprocess helpers and supervised background tasks for I/O.

## Interfaces and Dependencies

Add shared optional TurnDiagnostic and provider accounting types without new crates. QuotaLimit is a distinct stop reason from ModelCapacity. Extend current runtime/relay/outcome serialization and API responses with serde defaults. Reuse native Kimi follower, ACP typed notification registration, usage database projection, installer locks and runtime leases.

## Artifacts and Notes

Initial plan recorded 2026-09-12 from the approved conversational plan. No implementation checks have run yet.

Revision: runtime relocation and lease-safe repair validated with fixtures and the pinned real installer. Logs: target/provider-installer-tests.log, target/provider-grok-install-live.log, target/provider-branch-auth-tests.log. Slow tool startup prompted runbook inspection; TEST_STATEID remained 86 to 86, no host changes.
