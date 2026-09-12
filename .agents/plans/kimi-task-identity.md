# Reconcile Kimi background work by native task identity

This ExecPlan follows `.agents/PLANS.md` and is maintained during implementation.

## Purpose / Big Picture

Completed Kimi sessions must become idle after their background tasks terminate. Queries about an existing task must not create phantom tasks. The separate compact status change displays `Working`, `N tasks` with age, or `Idle`.

## Progress

- [x] Confirmed all 12 stale entries across eval9 3209, 3167, and 3212 against live relay snapshots and native wire records.
- [x] Implemented native identity history and provisional reconciliation; added query-order regressions.
- [x] Full Rust tests and clippy passed; final diff reviewed.
- [ ] Commit the validated changes and push to origin/master.

## Surprises & Discoveries

All remaining entries were provisional: nine TaskOutput queries and three WaitFor calls. Each queried a terminated task. In eval9 3209, native wire line 710 starts bash-tlqj0v63 with launcher tool_cRcPD3vHk85f8sb0TTB71cYn. Lines 814–815 query it using tool_IT23mFTBUqZYZHAf4Lbsryld; line 825 terminates it. Reconciliation previously matched only the launcher ID, so the query survived forever.

## Decision Log

- Decision: correlate by task identity as well as launcher identity, retaining observed identities after termination. Reason: lifecycle events and queries name the same task but different tool calls; delayed query results must not revive terminated tasks.
- Decision: preserve unmatched ACP evidence and existing safety gates. Reason: an empty or unavailable native scan alone cannot prove an unobserved launch terminated.
- Decision: retain the user's compact task-count labels. Reason: foreground and background activity are different even with correct lifecycle tracking.

## Outcomes & Retrospective

Implemented native identity reconciliation, query deduplication, and delayed-result protection. The full Rust suite and clippy passed, including both event-order regressions and the native wire monitor integration test. Live sessions have only been inspected; their processes and state have not been modified.

## Context and Orientation

`src/hel_acp/kimi_tasks.rs` reads the native Kimi wire log and builds a snapshot of active tasks and observed launcher IDs. `src/hel_worker.rs` combines this with provisional ACP tool-card evidence and hosted terminals. `mj-worker/src/hel_worker_runtime/unix.rs` polls the native stream in a blocking background task and forwards the snapshot to the relay. Provisional means positive activity evidence whose native lifecycle record has not yet been matched.

## Plan of Work

Extend KimiTaskSnapshot and its tracker with observed_task_ids, retained for both starts and terminations. Extend the relay's provisional entries with an optional native task ID. Store the current native identity history in the relay, remove matched provisional entries when native snapshots arrive, and reject delayed ACP observations already represented by that history. Deduplicate unresolved observations of the same task while preserving their oldest timestamp. Clear identity history when the owning harness is torn down, alongside existing process-local tracking. Forward the new field through KimiTaskMonitor.

Keep native task history internal; no serialized relay protocol or database migration is required. Update existing tests for the additional explicit history argument. Add regressions for repeated TaskOutput and WaitFor observations, native-first and ACP-first order, termination, and late results. Extend the real wire monitor regression so its termination clears a query with a different launcher identity.

## Concrete Steps

From `/home/jonathan/Projects/hel`, run elevated `RUST_TEST_THREADS=8 cargo test kimi_`, then elevated `RUST_TEST_THREADS=8 cargo test` and `cargo clippy --all-targets -- -D warnings`. Use distinct logs in target for each run. Review `git diff --check` and the final diff. Stage only the task's files, commit on master, and push origin/master as explicitly requested. Do not include the pre-existing workspace plan or mj.sqlite3.

## Validation and Acceptance

Both query types must represent one task while active, zero after termination, and zero after a delayed running response. Unknown launch evidence remains busy. The monitor test must report safe_to_replace only after all real work ends. Existing terminal deduplication, parser rotation, scanner failure, and checkpoint safety tests must pass. Compact status tests cover task count, user shells, foreground precedence, and transition to idle.

## Idempotence and Recovery

Tests use temporary directories; runtime task data remains untouched. Failed native scans retain conservative behavior. Existing workers need the updated executable before using the corrected tracker; this task does not forcibly restart live sessions or erase tracking records. Repeated snapshots and queries must be idempotent.

## Artifacts and Notes

Initial focused validation log: `target/kimi-identity-focused.log`. Final successful full-suite log: `target/kimi-identity-full.log`. Final successful clippy log: `target/kimi-identity-clippy.log`. The initial focused run exposed a missing ready-state setup in the new fixture; correcting that setup made the idle assertion pass in the full run. Native evidence was read from morannon Podman containers using bounded read-only commands.

## Interfaces and Dependencies

KimiTaskSnapshot gains an observed_task_ids set. DurableRelay::kimi_background_tasks_changed receives that set alongside active tasks and observed launcher IDs. A private KimiProvisionalTask holds BackgroundCommand and optional native identity. No new dependencies or public wire fields are added.

Created during implementation to record verified cause, identity reconciliation, and validation requirements.

Validation completed before committing; no additional implementation changes remain.
