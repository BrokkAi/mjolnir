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

## Measured heap owners and fixes, 2026-11-12

jemalloc is now built with `stats` and `profiling` (inactive by default; `_RJEM_MALLOC_CONF` on `mj daemon restart` enables them for one daemon). Live heap profiles of the production daemon (60 running sessions, ~25,500 session records) measured:

| Owner | Live bytes | Outcome |
| --- | ---: | --- |
| jemalloc arenas: 480 by default on a 120-vCPU host | 380 MB slab fragmentation + 198 MB metadata | `narenas:8` compiled in (139836eae): 200 MB + 71–95 MB |
| `CommittedState::startup_groups`: full `step_json` of every session's latest startup group | 253 MB | compact step status (1846d802d); owner gone from profiles |
| Viewer rows for every session | 127–155 MB | boxed optional payloads, shared commands (01366bf04): ~90 MB |
| Decoded actor windows (`decode_transcript_body`) | 450–680 MB | open; see below |
| Browser projector entries for every active session | ~180 MB | open; projection results also feed viewer rows |
| Session state and subagent reports in `CommittedState` | ~120 MB + 37–41 MB | open; the report reconciliation needs report and turn from one snapshot |

Resident plus swap of a fresh daemon at 10–24 minutes fell from 3.0–3.6 GB to 2.3–2.6 GB.

The decoded windows are the largest remaining owner. Their JSON totals ~100 MiB for 60 sessions, but `serde_json` is built with `preserve_order` (enabled by agent-client-protocol), so every object is an `IndexMap` and small structures expand 10x or more. A third of the window JSON comes from two pins: one session's current turn of 7,964 items kept whole, and four turns kept whole for tools stuck `in_progress`. Bounding the current-turn pin, or holding tool calls in a cheaper form than `Value`, are design changes rather than retention fixes.

Follow-up fixes and checks, same day: published browser rows are shared between the projector's cursor baseline and the `conversations` snapshot (403bb1bc7), and relay assessment history is held behind `Arc` so copy-on-write operational-state clones share it (56a4be071). A re-profile after both measured a 1.36 GB sampled live heap at 15 minutes. Remaining owners and why each is not a quick fix:

- Browser projector full-window entry cache (~200 MB): converting only the rendered suffix changes output, because `rich_presentation_key` and `restart_collapse_window_start_seq` (mj-client/src/transcript.rs) are computed over the whole window. Needs a separate whole-window topology pass.
- `publish_view` operational-state copies (~37 MB before the `Arc` change): the remainder is copy-on-write clones of other operational fields kept by older published views.
- `record_index` groups (~16–20 MB): six indexes over ~25,500 sessions; sharing the id strings would save ~5 MB.
- `decode_relay_response` (~27–33 MB): the relay data that makes up each session's operational state; overlaps the `publish_view` figure.
- Subagent reports (~37 MB): periodic report reconciliation reads the report and the completed turn from one snapshot; moving the message to on-demand reads needs re-observation logic.

Second follow-up: `assessment`, `available_commands` and `config_options` (24, 21 and 4 KiB of JSON per live session) are shared behind `Arc` across operational-state copies, and unchanged decoded payloads reuse the held allocation (d2b2b7462). Re-profiled at 15 minutes: `publish_view` fell from 37–50 MB to 10 MB and the sampled live heap to 1.29 GB. `decode_relay_response` (37 MB) is now the held state itself: 20.6 MB current `RelayOperationalState` per session, 9.0 MB pending `SubagentCompletion` and 5.5 MB their evidence. Record indexes are bounded by measurement: at most ~143,000 index entries, so sharing their 32-byte id strings saves at most ~4.6 MB.

Checked and not quick: keeping only the current turn's start item and Plan items (instead of the whole latest turn) breaks three in-memory readers: raw-result terminal matching (mj-transcript/src/projection/terminals.rs:177, :326) needs every candidate in the turn to judge uniqueness; the quota continuation reply lookup (daemon/continuation/quota.rs:98) scans back to the latest agent reply, which can sit more than 1,024 items earlier; reviewer seeding (review_host/reviewer.rs:307) collects every user item since the last context boundary. Each needs a durable lookup first. Duplicated output text inside tool items (terminal output repeated in `call.content`/`rawOutput`) measured 3.9 of 56.5 MiB of tool JSON, too little to deduplicate.

Viewer resume compatibility (target ids, refusal reasons and target lists, ~15 MiB repeated across ~25,700 rows) is interned per snapshot (e91548aee); viewer rows measured 74 MB at 15 minutes. The cached tool `presentation` cannot be dropped from decoded windows: retention removes a finished call's raw input and output, so the cached summary is the only remaining source (mj-transcript/src/transcript.rs `materialized_tool_call_presentation`). Final sampled live heap at 15 minutes: 1.27 GB.

Browser projector, third round: the projector keeps per-item topology facts for the whole window and converts full entries only for the reachable suffix plus older Message rows (27bd415fa, f1b169508, 90b7da547), and drops the terminal-only tool detail (tool content, raw input, error, locations) from its cached entries because browser rows render only summaries and diffstats (b977a6eea). Profiled at 15 minutes in a quiet period: browser projector 210 → 122 MB; sampled live heap 1.09 GB, of which decoded actor windows are 439 MB. Typical windows still need most of their items converted to fill 1,000 rendered lines, because tool rows collapse to one or two lines.
