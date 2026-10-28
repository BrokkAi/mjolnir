# Reuse runtime publications when their inputs do not change

This ExecPlan follows `.agents/PLANS.md` and remains a living document.

## Purpose / Big Picture

Frequent session updates must cost work proportional to changed sessions, rather than all historical sessions and unchanged configuration. The daemon currently spends roughly four cores rebuilding snapshots. Preserve public JSON and capacity admission behavior while removing that work.

## Progress

- [x] (2026-10-28) Profile the installed daemon; save evidence in `/mnt/optane/tmp/mj-cpu-20261028/diagnosis.txt`.
- [x] Inspect runtime, capacity, catalog, storage, and API wait owners.
- [x] (2026-10-28) Share metadata publications and cache them under their owners.
- [x] (2026-10-28) Maintain capacity inputs from record differences; reuse the result across surfaces.
- [x] (2026-10-28) Replace linear lookup and cheapen unrelated wait wakeups.
- [x] (2026-10-28) Add large-history, immutable-publication, wire, and staleness regression coverage.
- [x] (2026-10-28 15:20Z) Complete executable regressions, all four touched crate suites, workspace clippy, and formatting.
- [x] (2026-10-28) Commit the first fix as ac9d7ce9f, merge concurrent upstream work as 1d746fab4, push, and install through normal startup.
- [x] (2026-10-28 15:28Z) Install the first fix and verify handoff/session survival.
- [x] Identify remaining long poles: finite SessionWiki startup indexing and repeated local delegation queue copies.
- [x] (2026-10-28 16:01Z) Retain delegation and operational publications across empty polls and index report-parent candidates from committed differences; focused regressions and final suites pass.
- [ ] Commit, push, and install the validated follow-up; measure settled CPU again.
- [ ] Record settled CPU and remaining reconciliation costs against the user’s under-one-core target.

## Surprises & Discoveries

Capacity projection added on October 25 scans all sessions for each host on every runtime capture. Web publication repeats the same calculation. Profiling also found metadata clone/drop/deep equality and linear viewer lookups. There are existing persistent session maps and isolated regression fixtures to extend. The installed perf lacks symbol decoding; the downloaded Ubuntu perf resolves self costs but unwinding is incomplete, so debugger snapshots established callers.

## Decision Log

Use immutable, serializable shared values with copy-on-write mutation and pointer identity before deep equality. Keep their JSON identical so no protocol change is necessary. Owners replace publications on input changes; consumers clone shared values. The runtime history owns the capacity projection and derives its allocation inputs incrementally from durable session differences. Admission retains its existing serialized calculation. Avoid modifying or restarting the live store during tests.

## Outcomes & Retrospective

Implementation and validation are complete. The four touched crate suites passed 3,348 tests, with no failures; workspace clippy and formatting passed. The large-history capacity, API waiter, catalog-sharing and immutable metadata/wire regressions all passed. Delivery and installed CPU measurement are pending. Automatic approval review rejected creating a GitHub tracking issue containing internal profile details; this does not block implementation.

## Context and Orientation

`mj-controller/src/daemon/feed.rs` captures runtime state into cursor history consumed by terminal clients and the web server. `mj-client/src/runtime_feed.rs` defines its serialized metadata and delta. `mj-controller/src/daemon/capacity.rs` accounts for live containers and reservations. Profile choices and storage facts have independent owners in `server_runtime/profile_catalog.rs` and `target_storage.rs`. `server/api/wait.rs` watches the viewer publication while waiting for a session turn; `ViewerSessions` already has a keyed persistent map.

## Plan of Work

First add a shared immutable value alongside `SnapshotMap` and use it for large runtime metadata. Config, quota, model-catalog, and storage owners retain their publications until input changes. Then replace repeated capacity calculation with one incremental projection under runtime history; web consumes that same projection. Finally use keyed shared session records in API waits so unchanged rows avoid deep comparisons. Extend existing isolated publication, capacity reservation, wire, and wait regressions with large history, sharing, and stale input transitions.

## Concrete Steps

Work on the existing branch. Use ordinary mbx-managed Cargo storage, with no target redirection. Run focused tests during implementation, then full suites of touched crates and workspace clippy once after the final edits. All Cargo tests run outside the restricted sandbox with isolated test configuration; any executable invocation uses `--instance cpu-publication-regression`. Review formatting and the final diff before staging only changed files and committing. Push HEAD to origin/master; if upstream moves, fetch and merge it.

## Validation and Acceptance

Unchanged captures retain metadata identity, historical sessions do not enter capacity rescans, reservations and live allocations immediately change the published capacity, and waits answer changes to their own session while skipping unrelated revisions. Existing serialized runtime/API golden tests must remain unchanged. Run `cargo fmt --all -- --check`, touched crate suites with `cargo test -p <package>`, and `cargo clippy --all-targets -- -D warnings` in the dev profile. Where practical compare a repeatable isolated workload before and after; never run a test build against the default daemon.

## Idempotence and Recovery

All test data belongs to named isolated instances and temporary roots. No migration or live-store edits are required. Old snapshot holders must remain valid after replacement, and failed tests rerun only their affected cases until repaired.

## Artifacts and Notes

Baseline CPU measurements and raw profiles remain outside the repository at `/mnt/optane/tmp/mj-cpu-20261028/`. Add validation outcomes here as work proceeds.

## Interfaces and Dependencies

Use existing `Arc`, serde support, persistent maps, and watch channels; no new dependency or crate is required. Shared snapshots serialize their inner values. Capacity publication uses the existing `RuntimeContainerCapacity` wire shape; admission remains authoritative for launch decisions. API session lookup continues to return the same errors for unknown IDs.

Plan created after profiling to implement the three verified sources of repeated work.

Implementation update: metadata owners retain serializable SharedSnapshot values; runtime history alone projects capacity; reservation admission/release publishes a revision; web and terminal readers share the result. Waits use keyed Arc rows and avoid a second lookup on skipped wakeups. Existing large-history and waiter regressions were extended.

Validation update: `cargo test -p brokk-mj-core -p brokk-mj-client -p brokk-mj-controller -p brokk-mj-tui --no-fail-fast -- --test-threads=8` passed; `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed. Logs are in the profiling directory. A fresh installed-daemon baseline averaged 389% CPU before delivery.

Follow-up scope (user steering): after the initial index pass, the daemon measured about 1.7 cores. Existing delta transport still rebuilt and copied full local delegation queue vectors at multiple layers on each poll. Retain the connection-owned publication across empty delta pages; invalidate before applying any page, including partial-page failures. The five-second reconciliation sweep also revisits historical parents; preserve its durable completion guarantees when assessing that remaining cost.

Reconciliation decision: the operational owner now incrementally indexes parents with children that can still produce a report. The same `SessionRecord::is_cleanly_stopped` predicate used by child status excludes completed closes, while failed resumes remain eligible. This removes historical parents from each sweep without dropping reports for a stopped parent whose child is active, failed, or parked. A committed-store integration regression checks those transitions.

Operational follow-up: retain the standalone connection’s operational publication on empty catch-up too. This removes another deep JSON copy per poll while keeping exported view and wire shapes unchanged. The existing isolated submit/catch-up integration verifies unchanged polling preserves publication identity.

Follow-up validation: 3,351 tests passed across core, client, controller and TUI, including the newly merged upstream regression. Workspace clippy and formatting pass. The queue test covers large payloads, empty deltas, completion/retirement, and partial-page failure. The committed-store test covers clean closure, active/parked/lost/error states, failed resume and resumption. The initial installed fix settled to 145.7% CPU in a fresh ten-second sample before this follow-up.
