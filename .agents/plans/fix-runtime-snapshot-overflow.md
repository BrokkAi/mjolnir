# Keep native transcripts out of dashboard snapshots

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Restore session opening and live chat updates when native subagents have large transcripts. The daemon (the persistent controller process) currently puts every child's latest 200 transcript items in one response. Its 8 MiB transport limit closes the connection before a reply, leaving the dashboard stale. Keep the shared response small and read transcripts through existing controller-owned, read-only SQLite helpers in background tasks.

## Progress

- [x] 2026-09-20: Reproduced live overflow: native transcripts total 8,273,171 bytes, while unrelated workspace records alone add 993,302 bytes. A workspace without native children responds; the all-workspace request disconnects.
- [x] 2026-09-20: Confirmed intentional daemon-only writes with direct read-only clients. No database migration is needed.
- [x] 2026-09-20: Implemented metadata-only native snapshots and independent background projection loading.
- [x] 2026-09-20: Moved native history reads off the bounded daemon reply; added persistent feed failures and actionable oversized-response errors.
- [x] 2026-09-20: Added regression tests; full workspace tests, final-source Clippy, and formatting passed.
- [ ] Commit the validated implementation and verify live refresh using the rebuilt daemon.

## Surprises & Discoveries

The ordinary session path already reads durable projections locally. Native-agent views instead embedded complete transcript tails in the shared runtime snapshot. Existing daemon errors are debug-only, and the terminal poller logs refresh errors without exposing them on screen. Restarting cannot clear a deterministic oversized reply.

## Decision Log

2026-09-20: Retain the 8 MiB transport limit and existing read-only client architecture; do not truncate transcripts or remove stored data. Introduce metadata fingerprints for native projections, read changed tails with at most four concurrent readers, and publish those independently of ordinary sessions. Generation changes retire cached old transcripts. Native history uses the same controller database layer in a supervised background operation. Advance protocol 27 to 28 because the snapshot's native entry type changes. No push is authorized for this task.

## Context and Orientation

`mj-client/src/daemon.rs` defines transport framing and RuntimeSnapshot. `mj-controller/src/daemon/snapshot.rs` builds replies, and `daemon/serve.rs` sends them. `mj-controller/src/database/native_agents.rs` owns native metadata and transcript reads. `mj-controller/src/pollers/remote.rs` distributes runtime updates; ordinary projection loading lives in `pollers/runtime_feed.rs`. `mj-cli/src/dashboard.rs` and `dashboard/drains.rs` consume watch channels without blocking the event loop. `mj-tui/src/native_agents.rs` renders native projections and merges older history. Shared notices live in `mj-chat/src/chat.rs`.

A projection is the stored current transcript and session state reconstructed from worker events. Its applied ordinal and digest identify the last event. A native child also has a generation ordinal identifying the incarnation of its history. Never merge data across generations.

## Milestones / Plan of Work

First add a shared native summary type containing agent metadata, generation, applied ordinal, and digest. Read these without transcript rows when building snapshots. Add a dedicated native background feed that accepts the latest summary set, retains unchanged views, removes missing/replaced generations, loads changed views with bounded concurrency, reports failures, and retries lagging projections with the existing convergence policy. Ordinary runtime updates must continue independently.

Second move terminal native history requests to a bounded background database helper. Add a watch channel for refresh/native-load health and a persistent shared notice that clears on recovery without deleting unrelated notices. Before sending a response, detect an oversized encoding and substitute an actionable small error identifying operation, size, limit, and request ID. Keep the connection usable.

Third use isolated database fixtures and loopback tests to reproduce the overflow and prove delivery. Test unchanged caching, independent loading, failed reads, generation replacement/removal, large paginated history, notice recovery, and response framing. Then validate the whole workspace and commit only task changes on the current branch.

## Concrete Steps

Work in `/home/jonathan/Projects/hel`. Run `cargo fmt --all -- --check`, elevated `cargo test`, and `cargo clippy --all-targets -- -D warnings` in the dev profile. Build the native CLI using the normal build directory. Inspect the normal daemon startup/replacement commands before using them; preserve workers. Repeat the authenticated read-only all-workspace snapshot request and verify it returns successfully, with native metadata rather than transcript bodies. Confirm the saved bifrost-fuzz answer remains readable.

## Validation and Acceptance

A test fixture with native transcripts totaling more than 8 MiB must still permit runtime snapshots and ordinary session publication. Readers return complete data, including history pages larger than 64 KiB. Native errors cannot stop ordinary refresh. Missing/old generations cannot resurrect stale children. An oversized response produces a readable error rather than EOF, and failed refresh remains visible until recovery. Full tests and Clippy pass before commit.

## Idempotence and Recovery

No schema or live data modifications are required. Keep unrelated untracked files untouched. Do not delete transcripts, restart workers, or expand frame limits. Daemon replacement uses its existing protocol compatibility flow. Build/test outputs stay in normal build storage; tests run outside the restricted sandbox.

## Interfaces and Dependencies

Use existing crates, Tokio watch channels and supervised tasks, controller database transactions, and the current convergence retry policy. RuntimeSnapshot carries native summaries; UI state still receives NativeAgentView values. Keep protocol management Ping/Status/Stop shapes frozen. Persistent notice health must be separate from transient background notices.

## Outcomes & Retrospective

Focused native tests passed (39 tests), followed by a passing full dev-profile workspace suite including convergence/replay tests. Final-source Clippy and formatting passed. Release installation and live verification remain pending.
