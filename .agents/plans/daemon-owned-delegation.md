# Give delegation one daemon-owned coordinator

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Delegation must dispatch and deliver results with web access disabled or dashboard updates unconsumed. Workers own durable requests; one daemon coordinator owns scheduling, retries, and child completion handling. The separate state-update redesign is outside this change. Never run the new binary against the live instance.

## Progress

- [x] 2026-09-27: Inspected actor publication, dashboard dispatch, backend dependencies, and daemon shutdown.
- [x] 2026-09-27: Added a direct, fair, bounded observation feed from primary actors.
- [x] 2026-09-27: Moved shared backend services, dispatch, and child completion into daemon ownership.
- [x] 2026-09-27: Validated regression tests, separate named instance, Cargo tests, and Clippy.
- [ ] Commit on current branch and push to origin/master.

## Surprises & Discoveries

The backend currently uses the dashboard's remote session facade. Merely moving the dispatch loop would retain this dependency. Backend construction, profile discovery, quota reports and credential rejection tracking must also become daemon-owned. Child completion reminders and parking are currently triggered by dashboard consumption.

## Decision Log

2026-09-27: Keep worker queues as the durable source; use a lightweight actor feed with one pending observation per session and FIFO scheduling. Reuse existing backend operations and durable command IDs, with result-only retries after execution. Retain existing upgrade admission rules. No schema, public API, or wire changes.

## Outcomes & Retrospective

Implementation and isolated real-worker validation are complete. Delegation no longer depends on dashboard consumption or web enablement. Publication remains.

## Context and Orientation

`mj-controller/src/session_manager/actor.rs::publish_view` sees worker requests before the continuation and dashboard queues. `server_runtime/run.rs` currently schedules tools using `SubagentDispatch` and runs child completion tasks. `daemon/process.rs` owns the primary manager and shutdown. The new `daemon/delegation.rs` service is owned there, and shares its backend with HTTP. Profile/quota/credential services must remain active independently of HTTP.

## Plan of Work

First add a dedicated actor observation feed, coalescing relevant request, result, turn and credential changes. A FIFO of distinct session IDs preserves fairness; actor-scoped generations reject retired publishers. Keep all transcript data out of this feed. Publish before the dashboard queue and seed every connection.

Next move backend construction and supporting caches to a daemon service using primary manager control. Feed configuration from RuntimeState, update quota/rejected-login caches at their source, and let the dashboard subscribe. Move scheduling and child completion into the coordinator. Independent tool tasks overlap; same-child inputs remain ordered. Failed delivery retains its result and retries on a timer. Connection leases cover only delivery. Durable relationships and existing command IDs support restart recovery.

Finally remove dashboard ownership, supervise shutdown, and add behavior regressions. Bounded operations retain upgrade admissions; pending waits and retries do not block handoff. Record request lifecycle traces rather than refresh logs.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir4`. Run focused controller tests first, then `cargo test` and `cargo clippy --all-targets -- -D warnings`, using the dev profile and normal mbx storage. Cargo tests run with elevated permissions. All manual CLI/daemon invocations use `--instance delegation-1171` and isolated config/data. Do not install, upgrade, restart or connect the new build to the default instance. Commit only changed files on hel4, then push HEAD to origin/master without force.

## Validation and Acceptance

Prove delegation works with dashboard feed unconsumed and web access disabled; requests without transcript changes propagate; busy lower-sorting IDs cannot starve another parent; independent children overlap and same-child inputs retain order; delivery retries do not execute twice; child completion works without dashboard; obsolete actors cannot overwrite replacements; handoff preserves worker work. Run existing isolated upgrade regressions. Record exact results below as they complete.

## Idempotence and Recovery

Pending requests remain worker-owned. The next daemon observes them again after restart. Preserve stable IDs and existing replay safeguards, never replay arbitrary mutations because acknowledgement was lost. Cancel resumable tasks during shutdown and join/report supervised failures. No database migration is planned.

## Artifacts and Notes

Issue #1171 is assigned and marked agent-in-progress. Original investigation is `.agents/docs/issue-1171-root-cause.md`.

## Interfaces and Dependencies

Add a crate-private delegation observation sender/receiver in session_manager, with actor-scoped publishers and FIFO coalescing. The daemon service exposes a shared ApiBackend and read-only quota subscription plus manual refresh trigger to HTTP. The daemon owns service task handles and joins them before closing its primary session manager. Reuse Tokio channels, cancellation tokens, JoinSet, existing upgrade gates, and the current backend; add no crate.

## Validation evidence

The initial focused run passed 9 tests. The full controller suite passed 1,872 tests but rejected the new integration fixture because its LocalBare worker root did not end in the session ID; corrected that fixture. Clippy passed once; final exact-source checks follow. A real-worker run in `--instance delegation-1171` with private configuration/data and deterministic ACP passed all tools and a daemon replacement in 4.854 seconds. Evidence: `/mnt/optane/delegation-1171-evidence/delegation-seed-1171-749225`. A follow-up proved automatic parking/resume, but its 8-second restart assertion proved too tight under full-suite CPU load (8.127 seconds); strengthen it to prove the child remains running and interruptible after handoff, using a longer fixture turn.

The user also authorized repairing all stale mbx target links across `~/Projects/mjolnir*` and `~/Projects/bifrost*`. Repaired four broken links (bifrost-fuzz, bifrost3, mjolnir3, mjolnir4) using `/mnt/optane/mbx-targets/v1`; healthy targets were preserved and a subsequent scan found none broken. No live mj instance was invoked or upgraded.

Revision note, 2026-09-27: Implemented the ownership change, recorded isolated validation evidence, and refined timing-sensitive fixtures based on actual failures.

Final runtime evidence, 2026-09-27: `tests/e2e/delegation.py --mj target/debug/mj --worker target/debug/mj-worker --instance delegation-1171` passed with private config/data, a deterministic ACP adapter, and real worker processes. It verified list/spawn/handback/wait/input/interrupt, automatic parking and resume, and an active child plus pending wait surviving a 4.832-second daemon replacement. Evidence: `/mnt/optane/delegation-1171-evidence/delegation-seed-1171-976712`.

The stronger parking check exposed two distinct facts: recording the completion notice does not prove the worker was parked, and the worker must refuse parking when native goal state is unknown. The coordinator now retains `Busy` as a retry with a one-second backoff, propagates the actual worker reservation outcome, and reconstructs unfinished parking from the durable notice after restart. A focused regression verifies that retrying does not repeat the parent notice. The fake adapter was corrected to report its absent native goal after loading a parked conversation; production idle admission remains unchanged.

Final-source Clippy passed. All 1,874 controller tests passed, including the direct-feed, delivery retry, fair scheduling, web-disabled, parking, and upgrade regressions. A broader run hit the unchanged worker checkpoint test's one-second timeout under concurrent build load; its isolated rerun passed in 0.17 seconds. The final full run passed every unit test and reached CLI integration tests, where the unchanged eight-second concurrent-startup deadline expired under host load. All four store-divergence tests passed on isolated rerun. The remaining 11 PTY tests passed; documentation tests were run separately to complete the suite. No production changes or test timeout changes were made in response to either timing failure. Logs: `/mnt/optane/delegation-1171-cargo-test-verified.log`, `/mnt/optane/delegation-1171-store-recheck.log`, `/mnt/optane/delegation-1171-pty-final.log`, and `/mnt/optane/delegation-1171-doc-final.log`. Rust formatting, Python lint/format, and diff whitespace checks passed.
