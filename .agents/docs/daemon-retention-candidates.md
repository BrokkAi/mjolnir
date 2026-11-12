# Daemon retention audit, 2026-11-12

This is a code-backed ownership audit from the daemon CPU/RSS investigation. Raw evidence is in the private directory `/tmp/mj-daemon-regression-20261111`. The Linux daemon now uses jemalloc; application owners can still retain obsolete values. Per-owner live heap bytes have not been measured.

## 1. Old unfinished tools pin the live actor's whole history — confirmed

Before the correction, `ProjectionWindow::trim` in `mj-core/src/state.rs` and the matching loader in `mj-controller/src/database/materialized.rs` moved the window boundary back to the oldest streaming message or pending/in-progress tool. All intervening turns stay live. The 1,024-item target is consequently not a bound.

A read-only probe using the exact production boundary calculation found:

| Active conversation | Whole stored history | Loaded live window | JSON body bytes in window |
| --- | ---: | ---: | ---: |
| Largest sampled history | 67,576 items | 56,041 items | 82,923,551 |
| Second sampled history | 36,583 items | 36,246 items | 54,592,707 |
| Control without an old unfinished entry | 29,127 items | 1,028 items | 1,634,688 |

These byte counts cover serialized bodies, before decoded JSON allocations, indexes, Arc vectors and conversion copies. `ProjectionIndex::new` and trimming revisit the oversized live projection on updates, so the ownership also costs CPU.

Both measured pins are in-progress tools, last changed on 2026-10-31 and 2026-10-05. Removing every pin would retain 1,029 items / 1,951,417 bytes and 1,029 items / 1,457,784 bytes respectively, but loses implicit ownership needed for late output.

The final rule computes the tail boundary from the target and newest turn only, rounds back to a turn start, and retains complete older turns containing pending/in-progress tools or streaming Agent/Thought items. It drops settled turns between those older turns and the tail. Items before the first turn start count as one turn. Loader and live trim agree; omitted_items counts dropped items even when the window has gaps.

Review of 83536df64 exposed separation of unfinished Kimi tools from parked terminal output. Keeping the whole unfinished turn allows a late completion to claim that output, whether rawOutput arrives before completion or with it. The matcher still requires completed/failed status; a terminal arriving after a settled owner has left the window remains the existing window limitation. Its parked-candidate queries also repeatedly decoded historical outputs. Keeping unsettled turns replaces that machinery; explicit stable-ID rehydration remains for settled historical items receiving late updates. A single exceptionally long unsettled turn can still be large, but no longer retains every subsequent settled turn.

Read-only final-rule SQL measurements: 07d292ca retained 1,153 items / 1,543,326 body bytes; bbe1fe0a retained 2,115 / 3,204,840; large active control 0bf0983d retained 2,284 / 2,760,382. Production remains active, so tail values change between probes.

## 2. Cursor history undercounts raw transcript roots — concrete accounting gap

`RuntimeHistory::publish` in `mj-controller/src/daemon/feed.rs` retains immutable `RuntimeProjection` roots for cursor replay. It charges serialized `RuntimeDelta` bytes against a 16 MiB budget and also has a 4,096-root count bound.

A newly appearing transcript is represented by `TranscriptChange::Refetch` in `mj-client/src/runtime_feed.rs`, while the retained projection owns its raw `SessionTail`. That tail can be much larger than its marker. Removing the session does not immediately release old roots; those roots can keep the removed tail alive until history eviction. Scoped projections can share these roots, and are pruned with the history.

Next verification: compare distinct retained transcript payloads with current tails and charged delta bytes during create/close cycles. This requires an ownership measurement or regression, rather than assuming the wire-byte budget bounds heap ownership.

## 3. Browser projector keeps full typed entries behind a clipped display

`BrowserTranscriptProjector` in `mj-client/src/transcript.rs` owns `entries: Vec<ChatEntry>` and a cloned map of published browser entries. Typed entries retain raw source Arcs and converted tool input/output. `browser_transcript` applies the 1,000-line display limit to rendered output. It does not apply that line limit to the projector's typed cache.

`server_runtime/run.rs` schedules projections for changed active transcript tails, independent of whether a browser currently has that conversation open. Cleanup removes projectors and conversations for inactive sessions. The candidate is excessive live duplication across active sessions, rather than demonstrated failure of that cleanup.

Next verification: measure raw tail, typed-entry and rendered-entry ownership separately for the largest active sessions, including hidden/raw-only tool output.

## 4. Projection backlog pins superseded snapshots

`ConversationProjectionDispatcher` in `mj-controller/src/server_runtime/projection.rs` captures a materialized tail before acquiring one of two projection permits. It allows one running/queued request and one coalesced pending request per session. Many active sessions can therefore hold their own old materialized tails while waiting, plus newer pending tails.

The result channel is bounded and updates coalesce, so this is not a claim of an unbounded per-session FIFO. The retained bytes and age of queued generations remain to be measured.

Next verification: correlate queue age and queued/pending payload ownership with the live-session count and projection completion rate.

## 5. Late remote publications can repopulate retired reseed views

`spawn_remote_session_manager` in `mj-controller/src/session_manager/remote.rs` prunes its `latest` map when target sets change. Its publication branch inserts the received view into `latest` before checking whether the session is desired. A publication arriving after target retirement can therefore put a full materialized view back into the reseed cache until another target update.

The initial publish/target ordering also uses this reseed cache, so changing insertion order requires preserving initial actor seeding. This is a lifecycle-ordering candidate; production retention from this ordering has not been quantified.

Next verification: exercise retirement followed by a delayed publication and inspect whether the reseed view survives, alongside the publish-before-initial-target case.

## Other inspected owners

The store's idle reader pool is bounded to eight connections per database path. Session-manager live views and transcript roots are pruned against the desired actor set. Daemon client JoinSets are drained. These findings lower their priority relative to the candidates above; they do not constitute a complete leak audit.
