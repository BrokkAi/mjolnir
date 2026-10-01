# Publish transcript changes from the daemon instead of rereading them from SQLite

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

The terminal dashboard (`mj` with no arguments, built from `mj-cli`) shows every live session in a workspace. Each Sessions row has a short preview of the latest agent text, and opening a session shows its transcript at once, without a database query. To make that possible, the dashboard keeps an in-memory copy of each live session's newest transcript items (its "tail", at most 1,024 items).

Today the dashboard rebuilds that copy from scratch whenever a working session moves. It rereads the whole tail from the SQLite store, decodes every item from JSON again, and frees the previous copy. With several sessions streaming, a real dashboard read about 115 MB/s from SQLite. That work competes with typing for CPU, and the previous copy used to be freed on the dashboard's event loop.

After this change, the daemon publishes what changed in each session's transcript, item by item, on the same runtime feed that already tells the dashboard which sessions moved. The dashboard applies those changes to the tail it holds. Unchanged items stay shared between successive versions, so nothing is decoded or freed again. When the dashboard has no tail for a session, or its tail is from before a gap (a restart, a daemon replacement, a cursor the daemon no longer holds), it asks the daemon for a fresh tail on its existing connection, never SQLite. The dashboard's SQLite read rate for transcript tails becomes about zero. Previews still need no query when a session is opened, because the tail is already in memory.

You can see it working in the lab described under Validation and Acceptance. The dashboard's read bytes per second drop to the small amount its other reads need, and key-to-screen latency and event-loop busy share do not get worse.

## Progress

- [x] (2026-10-01 16:40Z) Measured the current behavior and traced the data flow (see Context and Orientation).
- [x] (2026-10-01 18:10Z) The user chose this design (option 3 in the Decision Log): the daemon publishes the changes, with no store schema change.
- [x] (2026-10-01 19:45Z) Milestone 0, independent of the feed source: move the worker projection out of a dashboard update instead of copying it, and free a replaced transcript off the event loop (commit c184ee00).
- [x] (2026-10-01 21:30Z) Milestone 1: the daemon keeps a bounded, shared transcript tail per live session and serves it on request, at a feed cursor.
- [x] (2026-10-01 21:30Z) Milestone 2: runtime feed deltas carry per-session transcript changes; protocol 52 -> 53 (renumbered at integration: master had already taken 52 for daemon-owned profile capabilities).
- [x] (2026-10-01 21:30Z) Milestone 3: the dashboard's runtime feed holds the tails, applies the changes, fetches fresh tails from the daemon on gaps, and no longer reads transcript tails from SQLite. Milestones 1 to 3 land as one commit, because none of them works without the others.
- [x] (2026-10-01 23:30Z) Milestone 4 (required): the browser's conversation projection reads the same shared tail, and its update cursors are exact for every item kind.
- [ ] Lab measurements before and after, at 30 sessions with 3 streaming and at 66 sessions with 6 streaming.

## Surprises & Discoveries

- Observation: the dashboard's background feed does not know which transcript items changed. The daemon publishes, for each session, only a projection ordinal and digest, and the dashboard reads the items from SQLite.
  Evidence: `RuntimeSessionView` in `mj-client/src/daemon.rs` has `projection_ordinal`, `projection_digest`, `operational`, `connected`, and `error`, and no transcript. `RuntimeSessionView::from_managed` drops the transcript of the `ManagedSessionView` it is built from.

- Observation: the daemon already holds each live session's transcript tail in memory. Its relay actor publishes a full `ManagedSessionView` to `RuntimeState::publish_session` (`mj-controller/src/daemon/snapshot.rs`), whose `snapshot.materialized.transcript` is a `Vec<Arc<TranscriptItem>>`. The projector reuses an item's `Arc` when the item did not change and publishes a fresh `Arc` when it did (`mj-transcript/src/projection.rs`). Comparing `Arc` pointers therefore finds the changed items without comparing contents.

- Observation: the store has no reliable per-item change marker. `latest_content_event_ordinal` is set only for agent messages. Tool calls, thoughts, plans, and terminal output are rewritten in place, and only `last_changed_at_ms`, a millisecond timestamp that can repeat, moves. Retention compaction (`compact_materialized_transcript_through` in `mj-controller/src/database/materialized.rs`, run after a verified checkpoint from `mj-controller/src/controller/checkpoint/staging.rs`) rewrites old tool bodies in SQLite without changing a timestamp. It does not change the daemon's in-memory projection.

- Observation: a daemon frame is limited to 8 MiB (`MAX_FRAME_BYTES` in `mj-client/src/daemon.rs`), and one measured session averaged about 22 KB per item, so a 1,024-item tail can exceed one frame. The transport already handles this: a reply listed in `DaemonReply::is_chunked` is encoded off the daemon's control loop and sent as 64 KiB fragments, which the client reassembles (`write_response` in `mj-controller/src/daemon/serve.rs`, `read_response` in `mj-client/src/daemon.rs`). `RuntimeChanges` was already chunked. The new `SessionTail` reply is chunked too, so neither tails nor deltas need paging.

- Observation: the browser does not receive item changes either. It is told a revision changed and then pulls `/api/conversations/{id}?after_seq=N` (`mj-controller/src/server/handlers.rs`, `conversation`). That returns converted entries whose `updated_seq` is above `N`. `updated_seq` comes from `item_update_ordinal` in `mj-client/src/transcript.rs`, which stamps thoughts, tool calls, plans, and terminal output with the current frontier, because the projection records no change ordinal for them. Those entries are therefore sent again on every pull.

- Observation: freeing the replaced tail on the event loop was most of the loop's drain time. Milestone 0 moved the free to a background thread. At 66 sessions with 6 streaming, drain time fell from 64 to 73 ms per second of wall time to 14 to 15. See Artifacts and Notes.

## Decision Log

- Decision: the daemon is the one owner of transcript changes. It publishes them on the runtime feed, and the dashboard no longer reads transcript tails from SQLite. No store schema change.
  Rationale: the user's decision (option 3 below). The daemon already applies each change item by item and holds the result in memory. Reading SQLite from a client is the direct-store path that issue #1043 wants removed.
  Date/Author: 2026-10-01, user, relayed by the coordinator.

- Decision: options considered and not taken. Option 1 was a store migration adding a trigger-maintained change sequence per item, so the dashboard could query only changed rows. Option 2 compared row metadata and `octet_length(body_json)` with no schema change, which is not exact: a rewrite that keeps the same timestamp and length is missed until a full reload.
  Rationale: option 1 needs a compatibility decision about the store and keeps the client reading SQLite. Option 2 is not exact.
  Date/Author: 2026-10-01, fix agent and user.

- Decision: one owner decides whether a dashboard tail is current: the dashboard's runtime feed task (`run_runtime_feed` in `mj-controller/src/pollers/runtime_feed.rs`). It holds the tails, applies changes only on top of the exact feed cursor they were computed from, and requests a fresh tail whenever it holds none for a session it needs. Nothing else keeps or patches a tail.
  Rationale: two places deciding whether a tail is current would be two predicates for one fact.
  Date/Author: 2026-10-01, fix agent.

- Decision: a fresh tail is served at a feed cursor, not at "now". The request names the session and the cursor the dashboard currently holds. The daemon answers from the retained projection at that cursor, or answers "reset required" if it no longer holds it. Every later delta is computed from a retained projection too, so a fetched tail and the deltas after it always line up.
  Rationale: a tail fetched at "now" can fall between two feed frames. The next delta would then start from an earlier state than the tail, and the client could neither apply it nor catch up while the session keeps streaming. Serving at the client's cursor makes that case impossible instead of retrying around it.
  Date/Author: 2026-10-01, fix agent.

- Decision: no paging and no per-session size cap. Tail replies and runtime deltas are chunked replies (see Surprises & Discoveries). A delta whose size pushes history past its 16 MiB budget already makes the daemon answer `ResetRequired`, and the client then takes a snapshot and fetches its tails again.
  Rationale: the earlier draft of this plan proposed 4 MiB pages and a 1 MiB refetch cap per session. Both existed only to stay under the frame limit, which chunking already removes.
  Date/Author: 2026-10-01, fix agent.

- Decision: `ProjectionConvergence` and `runtime_projection_view`'s ordinal check stay. Native-agent history loads use the same machinery (`mj-controller/src/pollers/native_agents.rs`). For transcript tails, the check now compares two values from the same daemon version and always passes, so it costs nothing. A tail fetch that meets an expired cursor returns `TailCursorExpired`. `runtime_projection_view` treats that as "retry after a new snapshot", not as an integrity failure.
  Rationale: removing the check would touch native-agent loading, which is outside this plan.
  Date/Author: 2026-10-01, fix agent.

- Decision: the browser milestone is required. The web viewer reads transcript changes from the same daemon-owned tail, with exact item-level change cursors instead of re-sending thoughts, tool calls, and plans.
  Rationale: one owner of transcript changes for every client (user decision, relayed by the coordinator).
  Date/Author: 2026-10-01, user.

- Decision: the browser's exact cursor compares rendered entries, not item versions. The projector keeps the entries it last published, and an entry keeps its cursor while its rendering is unchanged. The first implementation kept a per-item change ordinal in the shared tail for this, and it was removed.
  Rationale: rendering can change without its item changing. List rules mark a tool ended after a later interrupt, and the window trims the oldest entry's lines. An item version would miss those changes; comparing what the browser receives does not.
  Date/Author: 2026-10-01, fix agent.

- Decision: retention compaction is not published as a change. The user confirmed this. It is a storage retention step for items already held in a verified checkpoint, and it does not change the daemon's live projection, which is what clients display, the browser included. When the projection is next loaded from the store (a relay actor restart), the daemon's tail update compares contents for every item whose `Arc` changed. Only the items whose contents differ, the compacted ones among them, become changes.
  Rationale: the live projection is the single source for every client. Compacted content reaches clients through the same change path as any other edit, at the moment the live projection itself changes.
  Date/Author: 2026-10-01, fix agent.

- Decision: the tail bound stays at `PROJECTION_TAIL_ITEMS` (1,024, in `mj-controller/src/database/materialized.rs`), applied by the daemon. Items that fall out of the bound are published as removals, and `ProjectionWindow::omitted_items` counts them.
  Rationale: the dashboard's memory and render costs are sized for this bound today.
  Date/Author: 2026-10-01, fix agent.

## Outcomes & Retrospective

Milestone 0 is done and measured (see Artifacts and Notes). The rest has not started.

## Context and Orientation

The dashboard is a terminal program. Its event loop, `run_dashboard_for_workspace` in `mj-cli/src/dashboard.rs`, waits on input and on background "feeds" (channels that background tasks write to), applies what arrives, and draws frames. Work on this loop delays keys, so `AGENTS.md` forbids blocking or long work there.

The daemon (`mj daemon-run`) owns the SQLite store (`mj.sqlite3` in the data directory) and all session state. A worker runs each session's agent and streams numbered "relay events" to the daemon. In the daemon, a relay actor per session applies the events to a "materialized projection": one record per session (`MaterializedSession` in `mj-core/src/state.rs`) whose `transcript` is a list of `TranscriptItem` (`mj-core/src/transcript.rs`). Each item has a `stable_id`, a `position` (the event ordinal that created it, never changed), and a `body` (user text, agent message, thought, tool call, plan, terminal output, or system note). The actor writes changes to the store (`ProjectionPage::flush` in `mj-controller/src/database/materialized.rs`) and publishes its current view, `ManagedSessionView` (defined under `mj-controller/src/session_manager/`), to `RuntimeState::publish_session` in `mj-controller/src/daemon/snapshot.rs`.

`RuntimeState` keeps the newest view of every session in `owner.sessions`, a `SnapshotMap<String, RuntimeSessionView>`. A `SnapshotMap` (`mj-core/src/snapshot_map.rs`) is a persistent ordered map: cloning shares the tree, and changing one entry copies only that entry's path, so old versions are cheap to keep and two versions can be compared quickly with `changes`. `RuntimeHistory` in `mj-controller/src/daemon/feed.rs` keeps up to 4,096 versions of the live `RuntimeProjection` (records, sessions, moves, and so on), or fewer if their deltas exceed 16 MiB. Each version has a cursor, an incarnation id and a sequence number. A client calls the daemon request `RuntimeChanges { cursor, wait }` (`mj-client/src/daemon.rs`) and gets one `RuntimeFrame` (`mj-client/src/runtime_feed.rs`). That is a `Snapshot` when it has no cursor, a `Delta` computed with `RuntimeDelta::between` when the daemon still holds its cursor's version, or `ResetRequired` otherwise. `RuntimeReplica` in the same file applies frames on the client. The wire protocol version is `PROTOCOL_VERSION` in `mj-client/src/daemon.rs` (51 today). A daemon frame is at most `MAX_FRAME_BYTES` (8 MiB).

On the dashboard side, `spawn_remote_dashboard_worker_poller` in `mj-controller/src/pollers/remote.rs` starts `run_runtime_feed` (`mj-controller/src/pollers/runtime_feed.rs`). That loop long-polls `RuntimeChanges` through `poll_daemon_runtime`. For each session whose ordinal or digest moved, it calls `load_runtime_projection`, which reads the newest 1,024 items with `load_materialized_projection_tail` on a blocking thread, at most four at a time. It then checks that the ordinal it read matches what the daemon published (`runtime_projection_view`, `ProjectionConvergence`). The result becomes a `ManagedSessionView` and is published to the dashboard as a worker update (`SessionManagerUpdate`), and to an open conversation's watch. On the event loop, `drain_worker_updates` (`mj-cli/src/dashboard/drains.rs`) hands the projection to `request_materialized_projection` (`mj-cli/src/dashboard/session_state.rs`). That spawns `spawn_materialized_session_projection` (`mj-cli/src/dashboard/io/spawn.rs`), which builds a `PreparedMaterializedSessionDetail` off the loop (`mj-tui/src/ingest.rs`). It reuses work for items whose `Arc` did not change. The result comes back through the `dashboard_io` feed and is applied by `apply_prepared_materialized_session`.

## Plan of Work

Milestone 0 (done, independent of the feed source). `apply_worker_poll_update` in `mj-cli/src/pollers.rs` returns the update's `MaterializedSession`, moved out of the update, so `drain_worker_updates` no longer clones it. `apply_prepared_materialized_session` hands the replaced `TranscriptSnapshot` and projection cache to `mj-tui/src/retire.rs`. That module drops values on one background thread, so the loop pays one channel send.

Milestone 1: a shared tail per session in the daemon. `SessionTail` in `mj-client/src/runtime_feed.rs` holds a session's `MaterializedSession` without its transcript (`header`), its `ProjectionWindow`, and its items in a `SnapshotMap<(u64, String), Arc<TranscriptItem>>` keyed by `(position, stable_id)`, which is the order the store reads transcripts in. `TranscriptItem` and `TranscriptBody` now derive `Eq`, so comparing two `Arc<TranscriptItem>` checks pointers first and contents only when pointers differ. `SessionTail::publish` brings a tail to a newly published projection. It keeps the newest `PROJECTION_TAIL_ITEMS`. It replaces an entry only when the item's `Arc` changed and its contents differ, removes entries that are gone or fell out of the bound, and replaces the header and window when they differ. `RuntimeStateOwner::publish_view` (`mj-controller/src/daemon/owner.rs`) is the only place that publishes a view, and it updates `owner.sessions` and `owner.transcripts` together. The two removal paths (`install_relay_sessions`, `records_changed`) remove from both. `capture_runtime` copies `owner.transcripts` into the projection, which shares structure with the history. `RuntimeProjection::transcripts` is `#[serde(skip)]`, so a snapshot frame never carries tails.

The daemon request `DaemonAction::SessionTail { session_id, cursor }` answers `SessionTailReply::Tail { header, window, items }`, `NoTail`, or `ResetRequired`. `RuntimeHistory::session_tail` reads the retained version at `cursor`. A different incarnation, or a sequence the history no longer holds, is `ResetRequired`. The reply is chunked. The request does not hold upgrade admission (`upgrade_request_activity`), the same as `RuntimeChanges`.

Milestone 2: deltas carry changes. `RuntimeDelta` gains `transcripts: Vec<(String, TranscriptChange)>`, computed in `RuntimeDelta::between` from the two versions' tails. A tail present in both versions gives `Items { header, window, upserts, removes }`, where upserts are whole items and removes are keys. A tail new in the later version gives `Refetch`, because the receiver cannot hold it yet. A tail gone from the later version gives `Removed`. `PROTOCOL_VERSION` is 53. An older dashboard refuses the newer daemon and re-executes through the existing upgrade path, and a newer dashboard replaces an older daemon the usual way. The upgrade handoff format is unchanged.

Milestone 3: the dashboard applies changes. `RuntimeReplica::apply` applies a delta's `Items` only to tails the replica holds. `Refetch` and `Removed` drop the held tail. A snapshot frame replaces the whole projection, and its `transcripts` is empty, so every tail is dropped after a reset or a daemon replacement. `load_session_tail` in `mj-controller/src/pollers/remote.rs` replaces the SQLite loader. A held tail is current, because deltas keep it at the replica's cursor, so it is served from memory. A tail that is not held is fetched at the replica's cursor and kept, if the cursor has not moved. An expired cursor clears the replica's cursor and returns `TailCursorExpired`. `run_runtime_feed` also republishes a session's view when its tail changed but its fingerprint (ordinal, digest, operational state) did not, as after a projection reload. Nothing in the dashboard's runtime feed reads the store.

Two direct store reads of transcripts stay in the dashboard, and both are one-shot reads of sessions the daemon publishes no tail for. One is the resume seed (`request_transcript_tail_seed` in `mj-cli/src/dashboard/session_state.rs`); the other is a stopped sub-agent's transcript (`spawn_stopped_subagent_transcript` in `mj-cli/src/dashboard/io/spawn.rs`). They belong to the broader removal of client store reads (issue #1043).

Milestone 4: the browser reads the shared tail. Before this milestone, the in-process web server (`mj-controller/src/server_runtime/run.rs`) converted each `SessionManagerUpdate`'s projection with `BrowserTranscriptProjector` (`mj-client/src/transcript.rs`) through `ConversationProjectionDispatcher` (`mj-controller/src/server_runtime/projection.rs`). It stamped each entry with `item_update_ordinal`, which uses the frontier for thoughts, tool calls, plans, and terminal output. Now `publish_snapshot!` in `run.rs` compares the publication's `transcripts` with the previous one and queues the changed tails of active sessions. These are the same tails the terminal feed serves, read from `runtime_publication()` after the daemon published them. The worker-update arm no longer queues projections. `BrowserTranscriptProjector::project` remembers the entries it last published. An entry that renders exactly as before keeps its previous `updated_seq`, and a changed entry gets the new frontier, so `/api/conversations/{id}?after_seq=` returns only the entries whose rendering changed. This is exact for every item kind, and also for changes the item does not record, such as a tool call marked ended by a later interrupt. The pull protocol and `viewer.js` are unchanged.

## Concrete Steps

From the repository root, after each milestone:

    cargo test -p brokk-mj-client runtime_feed
    cargo test -p brokk-mj-controller --lib daemon::feed
    cargo test -p brokk-mj-controller --lib pollers::
    cargo test -p brokk-mj-tui
    cargo test -p brokk-mj-chat
    cargo test -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Run every `cargo test` outside a restricted sandbox, on the dev profile. The isolated upgrade regressions are in `mj-cli/tests/daemon_startup.rs` and run as part of `cargo test -p brokk-mjolnir`. Milestone 2 must keep them passing.

## Validation and Acceptance

Behavior tests, colocated with the code:

- Applying a delta's transcript changes to a held tail gives exactly the tail the daemon holds at the new cursor. This covers appends, in-place edits (a streamed agent message gaining chunks, a tool call completing), removals, items falling out of the 1,024 bound, and a relay actor reload that changes one item's contents. Unchanged items are pointer-equal before and after.
- A fresh tail fetched at a cursor equals the daemon's tail at that cursor, even after newer versions were published. A cursor the daemon no longer holds, or one from another daemon incarnation, answers `ResetRequired`.
- Resync: after a reset, a refetch marker, or a daemon replacement (a new incarnation), the feed fetches fresh tails and then applies later deltas.
- Previews need no store read: with the dashboard feed driven by a fake daemon that serves tails and deltas, a session's detail is present before it is opened, and the loader that read SQLite is gone. Run the feed tests with an `MJ_DATA_DIR` that has no database.
- Protocol: a client at the old protocol refuses a daemon at the new one, with the existing message, and the upgrade path replaces an older daemon.
- Tests drive more than 64 KB of transcript text through fetches and deltas, encoded and decoded as on the wire.
- The browser receives an entry again only when its item changed, for every item kind (milestone 4).

Lab measurement before and after uses a disposable lab prepared with `tests/e2e/prepare-luna-lab.py --fake-delay-ms 0`, never the default instance, and is finished with `tests/e2e/finish-luna-lab.py`. Measure at 30 sessions with 3 streaming and at 66 sessions with 6 streaming: key-to-screen latency (median, p95, max), dashboard wakeups and draws per second, loop busy share, dashboard CPU, and the dashboard process's read bytes per second (`rchar` in `/proc/<pid>/io`). Acceptance: transcript-tail reads leave the dashboard's read rate, which falls to what its other reads need. Latency and loop busy share are no worse. The host is shared, so record its load average with each run and interleave the builds being compared.

## Idempotence and Recovery

No step changes stored data. Every milestone can be reverted by reverting its commits. The protocol bump in milestone 2 is reverted together with the code that needs it.

## Artifacts and Notes

Milestone 0 (commit c184ee00), 66 live sessions with 6 streaming, debug build, host load average 54 to 95, two interleaved rounds. "Before" is the build with paced frames (commit 6b61119a):

    before: drain 72.6 / 64.1 ms per s of wall time, loop busy 67 / 70 %,
            dashboard CPU 207 / 209 %, key median 31.9 / 38.9 ms, p95 59 / 139
    after:  drain 15.4 / 13.8 ms per s, loop busy 64 / 65 %,
            dashboard CPU 180 / 156 %, key median 36.4 / 33.9 ms, p95 59 / 126

Frame building (about 550 ms per second of wall time in this debug build) still dominates the loop, so key latency did not move beyond the noise. Read bytes (54 to 81 MB/s) did not change, as expected; that is milestone 3's measure.

A build that also shared unchanged items between SQLite reads (by a digest of each stored body, set aside when option 3 was chosen) did better at 30 sessions with 3 streaming: allocator free on the main thread fell from 4.7 to 1.9 percent of samples, and dashboard CPU from about 95 to about 80 percent. Milestone 3 gets that sharing from the daemon's deltas instead.

## Interfaces and Dependencies

In `mj-core/src/transcript.rs`, `TranscriptItem` and `TranscriptBody` derive `Eq`, so `Arc` equality checks pointers first. In `mj-core/src/state.rs`, `ProjectionWindow` derives `Default`, `Serialize`, and `Deserialize`.

In `mj-client/src/runtime_feed.rs`:

    pub type TailKey = (u64, String);
    pub struct SessionTail { pub header: MaterializedSession, pub window: ProjectionWindow,
                             pub items: SnapshotMap<TailKey, Arc<TranscriptItem>> }
    impl SessionTail {
        pub fn of(materialized: &MaterializedSession, window: &ProjectionWindow, limit: usize) -> Self;
        pub fn publish(&mut self, materialized: &MaterializedSession, window: &ProjectionWindow, limit: usize);
        pub fn materialized(&self) -> MaterializedSession;
        pub fn change_from(&self, before: &Self) -> TranscriptChange;
        pub fn from_parts(header: MaterializedSession, window: ProjectionWindow, items: Vec<Arc<TranscriptItem>>) -> Self;
    }
    pub enum TranscriptChange { Items(TranscriptItems), Refetch, Removed }
    pub enum SessionTailReply { Tail { header, window, items }, NoTail, ResetRequired }

`RuntimeProjection` gains `#[serde(skip)] transcripts: SnapshotMap<String, SessionTail>`, and `RuntimeDelta` gains `transcripts: Vec<(String, TranscriptChange)>`. In `mj-client/src/daemon.rs`: `DaemonAction::SessionTail { session_id, cursor }`, `DaemonReply::SessionTail(Box<SessionTailReply>)` (chunked), `DaemonClient::session_tail`, and `PROTOCOL_VERSION = 53`. In `mj-controller/src/daemon/`: `RuntimeStateOwner::publish_view`, `RuntimeState::session_tail(session_id, cursor)`, and `RuntimeHistory::session_tail(cursor, session_id)`. In `mj-controller/src/pollers/`: `load_session_tail` (remote.rs) and `TailCursorExpired` (runtime_feed.rs).

Revision note (2026-10-01): rewritten around the daemon publishing transcript changes (the user's option 3). The earlier draft proposed a store change marker and is superseded. Its in-memory part is kept as milestone 0. Later the same day: implemented milestones 1 to 3. Paging was dropped because daemon replies are already chunked, the browser milestone became required, and the convergence check was kept for native agents. Then: milestone 4 implemented, with exact browser cursors from comparing rendered entries; the per-item change ordinal was removed.
