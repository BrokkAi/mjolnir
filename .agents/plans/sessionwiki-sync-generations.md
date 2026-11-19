# Session search syncs on demand, settles on the sync it asked for, and shows progress

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`.


## Purpose / Big Picture

Mjolnir keeps a full-text index of every coding session on the machine, built by the SessionWiki library (crate `brokk-sessionwiki`, source at github.com/jbellis/sessionwiki, used through the workspace crate `mj-sessionwiki`). The dashboard searches that index from two places: the Sessions filter (the text box on the row with the create, open and filter controls, which narrows the session list by conversation text) and the open dialog (the resume dialog, which searches archived and live sessions).

On 2026-11-18 a user message sent at 16:04 UTC in a live session was not found by either surface for several minutes, even after the user waited for the dialog's "index syncing" notice to clear. A diagnosis (report in `.mj/agents/90bb86e2d00b6989f9ac01893fa43fdb/findings.md`, not committed) found these defects:

1. A search only asks for a sync when the last sync started more than 60 seconds ago (`sync_is_stale` in `mj-sessionwiki/src/sessionwiki.rs`, `SYNC_STALE_AFTER`). Inside that minute a search answers from the old index and shows no sync activity, so the dialog believes it is done.
2. The daemon's `RuntimeState::wiki_search` (`mj-daemon/src/daemon/state.rs`) runs the query and only afterwards reads whether a sync is running. A sync that completes in between yields pre-sync rows labelled finished, and the dialog's follow loop (`follow_wiki_search` in `mj-cli/src/dashboard/io/spawn.rs`) stops. The indexer's flags also read idle at both ends of a pass: `requested` is cleared before `in_flight` is set, and `in_flight` is cleared before the success is published.
3. The Sessions filter (`spawn_sessions_text_search` in the same file) asks once and never asks again after the sync it triggered.
4. Successful sync passes are not logged at all, so a miss cannot be traced afterwards.

After this change: opening the open dialog, or the first keypress in the Sessions filter, starts a sync. Both surfaces show a progress bar for that sync and search again when that exact sync has finished, so a message sent before the surface opened is always found once the bar finishes. The daemon log records every pass with its generation, kind, duration and counts. To see it working: start an isolated instance, send a message containing a unique word in a session, open the dialog, watch the bar fill, and see the session appear without retyping.


## Progress

- [x] (2026-11-18) Milestone 1: sibling published `brokk-sessionwiki` 0.34.27 (source e23c1fe); schema remains 9.
- [x] (2026-11-18) Milestone 2 (clippy and 54 index tests plus parked-pass regression pass): indexer generation state machine, logging, removal of the staleness trigger.
- [x] (2026-11-18) Milestone 3 (protocol/reconnect regression and 22 browser resume cases pass): protocol actions and search generation, dashboard flows for dialog and filter.
- [x] (2026-11-18) Milestone 4 (44 resume tests and both full-surface goldens pass): progress bars in both surfaces, replacing the syncing notices.
- [x] (2026-11-18) Isolated `swsync` validation: both bars captured with partial counts, new messages found without retyping, generation 4 confirmed, pass logs checked, daemon stopped.
- [x] Final implementation and validation checkpoint prepared for the required commit/review.
- [x] (2026-11-18) First review findings fixed; epoch replies, late-handoff watches, sync-only 404 and actual write-refusal causes pass targeted regressions and affected-crate Clippy.
- [x] (2026-11-18) Second review fixes validated: passive existing-only connections, selection-preserving background refresh, and stamp-bound snapshot coverage with one state-driven re-request.
- [x] (2026-11-18) Third review fixes validated: attachment-owned readiness, full-demand carry-forward, delayed browser replies and same-route authentication recovery. 55 index tests, protocol/attachment regressions, targeted browser cases and affected Clippy pass.
- [x] (2026-11-18) Fourth review ordering fix validated: retain the connection readiness baseline and consume prepublished attachment success without a further event; focused tests and Clippy pass.
- [x] (2026-11-18) Remaining fourth-review fixes validated: web503 recovery, attachment-driven completion-read retry/cancellation, and compact minimized progress/error goldens. Parent deferred the library partial-session contract; review override must be recorded on the next exact reviewed HEAD.
- [x] (2026-11-18) Fifth review completion-read fix validated: all25 Resume browser cases and Node tests pass; typed-query, epoch, route and login cancellation retire retries. All four parent scope rulings are now accepted by the review tool (partial-session library reporting, already-handled POST handoff refusal, old loaded-viewer behavior, and hypothetical manual backup restoration).
- [x] (2026-11-18) Sixth review fixes and passive Attach support validated: one-shot explicit refusals, backend HTTP500, queued-target labels, protocol/HTTP/stop regressions and queued goldens pass. Clippy and four targeted browser cases pass. Interrupted Rust validation resumed to completion.
- [x] (2026-11-18) Seventh review fix validated: terminal reads acquire clients through the attachment owner, covering untyped SSH discovery failures without reclassifying backend errors. Four focused read/reconnect/cancellation tests and affected Clippy pass.
- [x] (2026-11-18) Eighth review fixes validated: independent epoch-reset reads, list-only progress/search repaint, and80-column dialog progress/error titles. Three browser cases, Node, affected Clippy and two Resume goldens pass. Explicit-stop refusal override accepted; global contract unchanged.
- [x] (2026-11-18) Ninth review fixes validated: retained-data Refreshing states/Searching caption, independent terminal epoch refresh and named deferrable admission. Two admission regressions,45 Resume tests (one existing ignored timing test), four browser cases, Node and affected Clippy pass.
- [x] (2026-11-18) User-directed simplification implemented: one daemon per episode, no reconnect/read retry, attachment readiness or query coverage metadata. Wiki-focused tests,45 Resume tests (one existing timing measurement ignored), filter transitions and goldens, all31 web Resume cases across the full run and focused fixes, Node checks, formatting and six-crate Clippy pass. Single simplification commit prepared; required review follows.
- [x] (2026-11-18) Follow-up review fixes validated: completion-read intent scopes sync errors; later typed-query failures recover normally. History Loading/Failed notices compose with sync progress/error in both tabs.45 Resume tests (one existing ignored timing test), filter transition, dialog goldens, three affected browser cases, Node and six-crate Clippy pass; retained background-connection/shared-fetch findings overridden under the user decision.
- [x] (2026-11-18) Compact-title and blank-query follow-up validated: History failed remains beside compact sync status; blank/whitespace live-text queries return locally before opening a connection. Updated80-column dialog golden and its verification pass, two CLI Wiki tests pass, and six-crate Clippy passes.
- [x] (2026-11-18) Final-search/history and quit follow-up validated: history Loading/Failed composes with final Searching at80 columns; queued sync updates cannot start reads after shutdown is requested.45 Resume tests (one existing ignored timing test), shutdown ownership test, updated/verified dialog golden and six-crate Clippy pass. Parent-directed external-rebuild override recorded.
- [x] (2026-11-18) Disabled-search and task-supervision follow-up validated: unavailable web search clears its filter and preserves metadata rows; independent joined terminal tasks surface read/follower failures and retain cancellation. Three CLI Wiki tests (panic isolation/cancellation included), four browser cases (typed-before-mismatch and golden included), Node and six-crate Clippy pass.
- [ ] Follow-up review and parent acceptance; parent owns push.


## Surprises & Discoveries

A query typed before a newer-schema reply could remain applied after the box was disabled, hiding every resumable row with no way to clear it. The box owner now clears the unavailable filter in both list and query state, so metadata rows remain usable and reopening does not restore the stale filter. Terminal reads and the sync follower also now reuse the existing join-error conversion behind cancellable task boundaries; one read panic becomes its own visible failure rather than unwinding its sibling or leaving the UI pending.

The final-read Searching shortcut was another title early return, so it now uses the same composition and compact layout as sync progress. Pending quit mutations can keep the dashboard event loop alive after reads were cancelled; the sync-completion handler now consults that same shutdown owner before starting any new query. The parent explicitly declined changing the restored external-rebuild behavior: the existing header mismatch suppresses the full-pass marker and the next pass is full.

A further review caught compact title replacement dropping the history error even though wide titles composed both states. Compact titles now retain a History failed indication beside sync status. The simplification had also omitted the pre-feature blank live-text query guard; restoring the local empty answer avoids an unnecessary daemon connection and unrelated failure during an otherwise successful empty-query refresh.

The simplified flow initially treated every query after a successful pass as a completion read. Read intent now belongs to the pending query: the dialog uses its existing Refreshing state, the filter distinguishes Typed from SyncRefresh in its pending state, and the browser passes that intent into its one read. Ordinary read errors can recover normally; only a failed completion read terminates the sync episode. The title renderer also now combines history and sync notices, so independent history Loading/Failed states cannot return before drawing progress or a sync error. The review findings asking to change retained connect_or_start behavior or shared upgradeAwareFetch retry were declined under the user's explicit simpler-design decision.

The user simplified the design after ten review rounds: connection interruption ends the current episode visibly, and reopening recovers. The reconnect/epoch-coverage/stamp mechanisms described in earlier historical entries below have been removed. Freshness now follows the ordering of one successful pass completion and one subsequent query; keystroke answers make no coverage claim.

Seventh review exposed plain SSH command failures during endpoint discovery. Reads now acquire connections through the existing attachment-owned helper before executing their query; this handles discovery errors without guessing from error types and still leaves actual backend read failures terminal. The completion-read regression starts with an untyped discovery failure before its dropped/refused RPC sequence.

Fourth review highlighted prepublished readiness: resetting the attachment baseline while handling EOF discarded a successful observation already published for the active connection. The owner would publish again on its next successful keep-alive, but retaining the connection's prior baseline removes that needless wait and directly consumes the intervening transition. The deterministic reconnect regression now publishes readiness before failure handling and proves that no further publication is needed.

Third review found that a single existing-only reconnect could fail during the handoff readiness gap. Attachment already owns successful connect+Attach observations, but its enum watch suppresses unchanged Attached states; a separate counter preserves fresh readiness without waking the renderer or adding another probe. Review also exposed a delayed pre-completion browser reply being misclassified as a rebuild, same-route login retaining a stale follower, and a coalesced incremental success hiding a failed full request. Targeted ordering/authentication regressions cover these cases.

Second review found that daemon-owned successful counters alone cannot certify a cache rebuilt by standalone SessionWiki. The existing schema cookie detects a cache rebuilt back to the same supported schema; compatibility alone does not. Search metadata must therefore travel with the rows read under one transaction. Second review also found that the ordinary typed-query initializer clears the selected archived result; background refresh now only advances request identity and retains both answers.

Review also exposed a watch arriving after cancellation but before the next poll: route all watches through the same cancellation-first transport-close path before the general shutdown refusal. An unsupported sync endpoint now clears only the sync follower and leaves working searches enabled. Regression coverage exercises both conditions.

The existing HTTP search handler also triggers stale syncs through backend traits; those hooks must be removed with the daemon staleness trigger. The callback was prepared in the sibling checkout `/home/jonathan/Projects/sessionwiki-progress` and is now published as `brokk-sessionwiki` 0.34.27; the temporary path patch was removed before the dependency checkpoint. The first compilation found two old return-value callers: destroy must wait through `sync_now`, and the hourly match arm must discard `request_sync`'s returned generation. Both are fixed. Two TUI Clippy suggestions (guarded match and checked division) were fixed; affected-crate all-target Clippy now passes with warnings denied. The first sibling checkout used the upstream package by mistake; the corrected fork package is `brokk-sessionwiki` 0.34.27. Workspace package names use the `brokk-` prefix, and the CLI package is `brokk-mjolnir`; the concrete validation commands below use these actual names.


The first isolated live probe rejected the real worker before creating a session: building the worker with `test-hooks` also embeds the zero test identity, conflicting with its real build stamp. Keep test hooks on the CLI/daemon for the sync barrier, and build `brokk-mj-worker` without that feature for the real worker. The failed probe cleaned up its isolated daemon and owned processes.


Final transition review found that a destroy confirmation retains the resume dialog beneath it. The completion-triggered query must use `resume_dialog_mut`, just as result application already does, so it refreshes that retained owner before a Cancel reopens it. The existing lost/destroyed-row test now exercises completion through that ordering. Followers retain the owner's actual latest outcome; each surface decides coverage from `successful >= target` instead of rewriting that outcome.


Automated review found that the web viewer retained a query's generation across an epoch change. An old generation 20 could incorrectly cover replacement target 1. Epoch changes now clear cached query coverage and invalidate in-flight query identities before requesting the new target; the existing browser sync case exercises the counter reset and verifies a fresh query without retyping.


## Decision Log

- Ninth review (parent approved): final queries retain existing rows but remain visibly Searching until their answers/errors arrive; terminal epoch transitions independently refresh the query before replacement sync outcome. Both sync-request entry points are deferrable during drain, with distinct named HTTP/terminal admission labels.

- Parent ruling (2026-11-18): preserve explicit-stop pre-dispatch shutdown refusal. It is not an upgrade handoff; returning UpgradePending could make foreground clients restart a daemon the user stopped. A surface in that stop window reports shutdown and can recover when reopened. Approved override recorded against177f95677.
- Eighth-review fixes: epoch replacement schedules its own current query independently of sync success; sync/search updates repaint only the visible resume list, preserving detail controls. Narrow dialog titles prioritize a fitted sync/error line over verbose list metadata, retaining mismatch context.

- Supporting decision (parent approved, 2026-11-18): the existing attachment owner's periodic Attach must also be one-shot/existing-only, otherwise its generic SSH retry can start a stopped daemon. Add attach_existing only for that owner; foreground initial Attach keeps its established retry semantics. Explicit non-admission becomes Missing and the normal attachment tick retries without startup.

- Decision (parent approved, 2026-11-18): backend index failures are HTTP500 and remain visible; HTTP503 is reserved for refusal/shutdown/handoff. Do not add a second client retry policy: background sync/search methods return typed one-shot non-admission without invoking SSH startup discovery; the attachment-driven owner retries only explicit refusals (never a lost sync mutation acknowledgement). Show a waiting label until started reaches the requested target, then show that pass's counts.

- Parent ruling (2026-11-18): no legacy GET-sync shim for an already-open old viewer. No automatic build/version asset-reload signal was found; a stale tab loses only on-demand sync until reload while background passes and searches continue. Keep one explicit sync mechanism. Manual restoration of an older index copy with identical schema cookies is also outside this task; do not add a new persisted content identity for hypothetical external restoration.
- Decision (parent approved, 2026-11-18): browser idempotent search reads, including the final completion query, retry transport loss/HTTP503 while their query and visit remain current. Abort them when superseded, on epoch reset or when leaving/logging out. Initial and retarget sync POST upgrade refusals already retry through upgradeAwareFetch's marked non-admission path; plain503 must not authorize mutation replay.

- Parent ruling (2026-11-18): leave SessionWiki's pre-existing warn-and-skip partial-session behavior unchanged. The old last_success had the same meaning; the hourly full pass uses since=None and retries skipped input. Reporting per-session failures needs a separate SessionWiki contract change, outside this task. Record an automated-review override against the next exact reviewed HEAD (the first attempt was refused because the late finding arrived after the already-validated readiness commit).
- Decision (parent approved, 2026-11-18): resume browser watches on HTTP503 as on transport loss; retry terminal idempotent search reads after the attachment owner reports readiness, including completion refreshes after their follower ended. Query guards cancel retries when superseded, cleared or closed. Narrow filter progress keeps a shorter bar and counts; errors put the cause first and use the existing ellipsis renderer.

- Decision (parent approved, 2026-11-18): the attachment owner publishes a separate counter for completed connect+Attach operations; its existing presence/render watch remains unchanged. A sync follower retains the counter used for its connection and retries existing-only connections after a newer successful attachment, including readiness published before the watch processed its lost transport. Failed reconnects wait for another counter advance, with no follower timer, startup path or timeout. This survives handoff and leaves explicit stops effective.
- Decision (parent approved, 2026-11-18): restore `full_requested` after a failed full pass without requesting another generation. An already queued or later explicit request must run full before a successful generation can cover that full request. A failure with no successor remains terminal.
- Decision (parent approved, 2026-11-18): only queries begun after a target's completion may invalidate that coverage. A pre-completion answer arriving later triggers the already scheduled final query, never another sync. Authentication loss retires the resume visit's follower; logging back into the same route starts a fresh episode.

- Decision (parent approved, 2026-11-18): tie each successful generation to the existing `IndexStamp` (schema version plus SQLite schema cookie) from its pass. Query header, compatibility status and rows in one SQLite read snapshot. Return generation zero with the same epoch if the sampled successful stamp does not match that snapshot. A completed surface target then transitions back to pending and requests one new target; further uncovered answers cannot re-arm it until a follower state change. A failed pass remains terminal. This covers standalone SessionWiki rebuilding the shared cache without adding a wire field or migration.
- Decision (parent approved, 2026-11-18): the passive sync follower and its background query tasks use `connect_existing`, so an explicit daemon stop stays stopped. Completion-triggered searches retain existing answers and selection until replacement answers arrive; typed queries keep their normal clearing behavior.

- Decision (parent approved, 2026-11-18): both search reply types carry `epoch` and `generation`, sampled together from one indexer snapshot before querying. All surface coverage checks require the answer epoch to match the target epoch. Protocol 107 remains the unshipped protocol for this change. A query reaching a replacement daemon re-arms its surface follower. Unwritable-index failures name the actual isolation or newer-schema cause.

- Decision: searches no longer trigger syncs; the surfaces request one explicitly (dialog open, first filter keypress). Rationale: the user asked for exactly this ("sync on dialog open or on first filter keypress") and called the 60-second gate dumb; one explicit trigger per surface episode makes "the sync this search waits for" well defined. Date: 2026-11-18.
- Decision: the indexer owns one state with monotonically increasing generation numbers, and every reader decides freshness from generations, never from booleans sampled at different times. Rationale: AGENTS.md concurrency rule (one owner per fact, single state machine); the stale-rows race came from two independent samples. Date: 2026-11-18.
- Decision: a failed pass (including SQLite busy) finishes its generation as failed and is shown, rather than silently re-arming a retry. Waiting surfaces then stop waiting and show the error. Rationale: fail visibly; a hidden retry flag is what left a surface waiting on nothing. Date: 2026-11-18.
- Decision: progress comes from a new callback in SessionWiki itself, published as a new crate version. Rationale: user choice; per-session progress exists only inside the library's sync loop. Date: 2026-11-18.
- Decision: the dashboard follows sync state by long-polling a new daemon action that returns when the state changes, not by timed polling. Rationale: the daemon protocol is request/response; a long poll gives push behavior without inventing an interval. Date: 2026-11-18.


- Decision: include a per-indexer epoch UUID in sync status and in watch requests; epoch mismatch returns immediately and causes the surface to request a new target. Rationale: counters alone cannot distinguish a restarted daemon, and an old version counter can strand a watch. Approved by parent, 2026-11-18.
- Decision: track a separate `successful` generation, advanced only on successful completion, and sample it before each query. Rationale: a failed later pass must not erase the generation a query can safely claim. Approved by parent, 2026-11-18.

- Decision: give the web resume/archive viewer the same explicit sync episode, progress display, and final re-query through `POST /api/v1/wiki/sync` and `GET /api/v1/wiki/sync/watch?epoch=...&after=...`. Rationale: removing the HTTP staleness hook without a replacement would strand the web search on background updates. Extend the existing web resume spec. Approved by parent, 2026-11-18.

- Decision: `sync_now` waits on the single state watch until `finished >= target`; it succeeds if `successful >= target`, otherwise returns the latest failure. A later successful pass covers the request equally. Do not retain per-pass waiters or an outcome registry. Rationale: callers need coverage of their committed data, not an exact historical pass result; one state and one watch are sufficient even when publications coalesce. Parent ruling, 2026-11-18.

- Decision (supersedes every reconnect, epoch-coverage and index-stamp decision in this log and in the Surprises section): a surface's sync episode lives on one daemon. If the follower's watch loses its connection, gets a refusal, sees a different epoch, or the final read fails, the surface shows that error in place of the bar and stops; reopening the dialog or starting a new filter episode starts over. No reconnect, no retry of the final read, no attachment readiness counter, no `attach_existing`, no epoch or generation on search pages, no IndexStamp coverage tie. Freshness comes from ordering alone: the follower waits on the watch until `finished >= target`, then runs the query once, which therefore reads after the pass committed; keystroke searches answer from the index as it is and claim nothing. Kept: the indexer state machine (generations, epoch in the sync status and watch, `successful`, full-demand carry-forward, `sync_now` rule), INFO/WARN pass logging, removal of the staleness trigger, explicit request on dialog open and first filter keypress, deferrable named admission at both request entry points, "Waiting for session index sync…" before the target starts, progress bars and narrow-width rendering, retained rows shown as "Searching" during the final read, 500 for backend search failures, the shared `upgradeAwareFetch` retry. Background calls keep the pre-change connection behavior. Rationale: ten review rounds were almost entirely about reconnect and handoff edge cases in machinery added for rare events; the user chose the simpler design ("show the error, recover on reopen"), 2026-11-18. Further parent ruling: keep the restored pre-change handling of a cache rebuilt during a pass. Its header mismatch suppresses the full-pass marker so the next pass is full; do not add a new pass failure for this rare external timing.


## Outcomes & Retrospective

All implementation milestones are complete. Dependency/docs/plan checkpoint `1dacc57d9` uses published0.34.27 with no path patch and unchanged index schema9. The final design follows one daemon per episode: watch/refusal/epoch/final-read failures stay visible until reopening. Search pages contain no coverage metadata; freshness is one successful completion followed by one query. Reconnect/readiness/stamp-coverage machinery and its tests are removed. The retained parked-pass/coalescing test,55 index tests, both named admission tests,45 Resume tests (one existing timing measurement ignored), filter transition test and all affected TUI goldens pass. The31-case web Resume spec was validated through a full run and focused reruns after fixing fixture ordering and its golden. Node, formatting and six-crate all-target Clippy pass with warnings denied. Earlier validation entries describe the superseded design; the latest Decision Log entry controls.

Before the final simplification, the isolated `swsync` probe captured real partial progress in both surfaces (252/1800 in the dialog and 72/1800 in the filter), then found unique messages without retyping. Its final search returned generation 4 and the matching session. INFO logs contain starts, finishes, duration and both phase counts. The named daemon was stopped and the owned processes were cleaned up. The probe uses private config/data/index/home paths, a minimal environment, the real worker and a deterministic ACP fixture with recorded-format native transcripts. Keep hooks on the CLI/daemon only: a hook-enabled real worker embeds conflicting test and real identities. The original probe's lone-bar-delimiter predicate also matched an unrelated profile rail; the final probe requires actual sync text, adapter counts and nonzero partial session counts.

The final code checkpoint is submitted to the required automated review before handback. Parent acceptance and push remain outside the delegated implementation scope.


## Context and Orientation

The daemon (`mj daemon-run`, crate `mj-daemon` over `mj-controller`) owns the index sync. Its sync job is `WikiIndexer` in `mj-sessionwiki/src/sessionwiki.rs` (struct around line 779). Today it holds atomic flags `requested`, `full_requested`, `in_flight`, a `last_success` time, and a `tokio::sync::watch` counter `completed`. `WikiIndexer::request_sync(full)` sets flags and notifies a worker task (`Indexer::run`), which loops calling `Indexer::sync`. One pass is `sync_blocking` (around line 1015): it indexes Mjolnir's own sessions first through `sessionwiki::index::sync_with` with a one-element adapter list (the `MjolnirAdapter`), then every other tool's sessions ("native" adapters, such as Claude Code and Codex stores) through a second `sync_with` call, then writes tags and provenance. An incremental pass passes `since` (epoch seconds) to skip unchanged old sessions; a full pass passes `None`.

Other callers of the indexer, which must keep working: `request_sync` at `mj-daemon/src/server_runtime/run.rs` (startup around line 278, hourly tick around line 660), `mj-daemon/src/daemon/observe.rs:52` (uses `changes()` and `request_sync`), and `sync_now` callers in `mj-daemon/src/daemon/serve.rs`, `daemon/resume.rs`, `daemon/actions.rs`, `daemon/continuation/quota.rs`, `daemon/continuation.rs`, `daemon/snapshot.rs`, `server_runtime/api/subagent_input.rs`, plus `server_runtime/api.rs:473` which reads `last_success()`.

The daemon protocol is a request/response action protocol defined in `mj-client/src/daemon.rs`: `DaemonAction` (search actions `WikiSearch` and `SessionTextSearch` near line 980), client methods `wiki_search` and `session_text_search` near line 2787, and `PROTOCOL_VERSION` near line 3721 (currently 106). Structs use `#[serde(deny_unknown_fields)]`, so any added field or action is a protocol change that bumps `PROTOCOL_VERSION`. `WikiSearchPage` and `WikiStatus` are near line 247. The daemon dispatches actions in `mj-daemon/src/server_runtime/api.rs` (trait methods near lines 228, 472, 4123), which call `RuntimeState::wiki_search` and `session_text_search` in `mj-daemon/src/daemon/state.rs:257-290`.

The dashboard (crate `mj-cli`, module `dashboard`, rendering in crate `mj-tui`) starts searches from `mj-cli/src/dashboard/chat_tasks.rs` (filter at line 8, dialog at line 39) and runs them as background tasks in `mj-cli/src/dashboard/io/spawn.rs` (`spawn_wiki_search` around 1622, `follow_wiki_search` around 1694, `spawn_sessions_text_search` around 1740). Results come back as `DashboardIoUpdate` messages. The dialog's state and title line live in `mj-tui/src/resume.rs` (results applied near 1442, `next_wiki_search` near 1636, title notices near 2701, where "index building, more may arrive" and "index syncing, more may arrive" are drawn). The filter row and its keys are in `mj-tui/src/dashboard_sessions.rs` (layout near 430, keys near 489) and input dispatch in `mj-tui/src/dashboard_input.rs:509`. The dialog open action is `mj-cli/src/dashboard/actions.rs:846`. The TUI has no progress-bar widget yet; the import tab draws "scanning n/m" with a compact spinner (`mj_chat::spinner::compact_span`), which shows the style to match. The dashboard must never block its render loop: all daemon calls stay in spawned tasks.


## Plan of Work

### Milestone 1: SessionWiki progress callback

Work in a fresh clone of github.com/jbellis/sessionwiki at its current default branch (not the stale checkout in `~/Projects/sessionwiki`). In `src/index.rs`, `sync_with(conn, adapters, since)` loops over adapters; for each it either runs `shared_store::sync` (store-backed adapters such as Mjolnir's) or discovers files and builds a `pending` list of changed sessions, then parses them, printing `\r` progress to stderr when stderr is a terminal. Add a public `SyncProgress` struct with the tool name, the adapter's position and the adapter count, sessions done, and sessions to do in this adapter (known once that adapter's changed set is computed; before that, report the adapter as started with an unknown total), and a public `sync_with_progress(conn, adapters, since, on_progress: &mut dyn FnMut(&SyncProgress))`. Make `sync_with` call it with a callback that does nothing, so existing callers and the terminal output are unchanged. Report at adapter start, once the changed set is known, and after each session parsed (both file-backed and shared-store paths). Add a test in the crate's existing style that drives two adapters and checks the sequence of reports is ordered and ends with done equal to total for each adapter. Release a new patch version through that repository's normal release process (read its `release.yml` and any RELEASING notes; the user has authorized publishing this version). Then bump `sessionwiki = { package = "brokk-sessionwiki", version = ... }` in the mjolnir workspace `Cargo.toml` and `Cargo.lock`.

Acceptance: `cargo test` passes in sessionwiki; the new version is on crates.io; `cargo check -p brokk-mj-sessionwiki` passes in mjolnir against it.

### Milestone 2: indexer generations and logging

Replace the indexer's flags with one state guarded by a single `std::sync::Mutex` and published through one `tokio::sync::watch::Sender<WikiSyncState>` (send while holding the lock, so watchers see transitions in order). `WikiSyncState` holds: `requested` (highest generation anyone asked for), `started` (generation of the running or last started pass), `finished` (last generation that ended), `last_outcome` (succeeded, or failed with an error string), `full_requested`, and `running: Option<SyncProgress>` (phase label, tool, adapter position and count, done, total). The rules: a pass with generation N covers everything committed before N started. `request_sync(full)` returns the target generation, which is `started + 1` (the next pass to start), and raises `requested` to it. The worker starts pass `started + 1` whenever `requested > started`, coalescing any number of requests into one pass. At pass end it sets `finished = started` and the outcome in the same locked update that clears `running`. There is no window in which a reader sees "not requested, not running" while a requested pass is still to run. A failure, including SQLite busy, ends the generation as failed (log it at WARN with the error) and does not re-arm a hidden retry; the next request starts a new pass. `sync_now(full)` requests and then waits on the watch until `finished >= target`; return success when `successful >= target` (including a later covering pass), otherwise return the latest failure. Keep `changes()` for observe.rs as a receiver that advances on each finished generation (or switch observe.rs to the new watch; one owner either way). Keep `last_success()` only if `server_runtime/api.rs:473` still needs it after the staleness trigger is gone; otherwise remove it.

Progress: `sync_blocking` calls `sync_with_progress` for both phases and updates `running` through the same lock and watch. Throttle publication to state changes the UI can show (for example, only when the integer percentage or the tool changes), so a 50,000-session pass does not wake watchers 50,000 times; this is a coalescing rule, not a cap.

Logging, at INFO in target `mj_sessionwiki` (or the module's existing target): pass start with generation, full or incremental, and `since`; pass end with generation, duration, sessions re-parsed per phase, and outcome. Busy and other failures log at WARN with the generation.

Remove `sync_is_stale`, `SYNC_STALE_AFTER`, and `request_wiki_sync_if_stale`. `wiki_search` and `session_text_search` no longer request syncs. Searches read the index as it stands and carry no epoch or generation metadata. Freshness follows ordering: wait for the requested pass to finish successfully, then query once. Delete the test `a_session_text_search_asks_for_a_sync_like_the_resume_search` (state.rs around 2253) since the behavior it pins is intentionally removed, and replace it with the tricky-condition test described in Validation.

### Milestone 3: protocol and dashboard flows

Bump `PROTOCOL_VERSION` to 107. Add `DaemonAction::WikiSyncRequest` returning a `WikiSyncStatus` (the client view of `WikiSyncState` plus the returned `target`), and `DaemonAction::WikiSyncWatch { epoch: String, after: u64 }` that returns the state once its version (a counter bumped on every published change) exceeds `after`, or immediately if it already does, including an epoch mismatch. A long poll must not hold daemon upgrade admission; cancellation or a changed epoch ends the episode with a visible error. Remove `WikiStatus.topping_up`: `WikiSearchPage` carries rows and `status.state`; `session_text_search` carries its matches without coverage metadata. Update every consumer the compiler finds.

Dashboard flow, the same for both surfaces: when the dialog opens, or on the first keypress of a filter episode (from the filter being empty or closed to having text), spawn one background task that calls `WikiSyncRequest`, remembers `target`, then loops on `WikiSyncWatch`, sending each state to the dashboard as a `DashboardIoUpdate` for the progress bar. When `finished >= target` it sends a final update and the surface re-runs its current query once (dialog: archive and live searches; filter: the session text search). Keystrokes keep their 250 ms debounce and search immediately from the index as it is; they do not request syncs and do not follow. Remove `follow_wiki_search`'s timed loop. The post-completion query runs once, retains prior rows, and shows Searching until the answer or an error arrives. The task is cancelled when the dialog closes or the filter is cleared/closed (drop guard, as `spawn_wiki_search` does today). Any request/watch refusal, connection loss, changed epoch, pass failure or final-read failure shows its error in place of the bar and stops. Reopening the surface starts a new episode; there is no reconnect or final-read retry.

The web viewer uses the same episode via `POST /api/v1/wiki/sync` and a long-poll `GET /api/v1/wiki/sync/watch?epoch=...&after=...`. Routes live in `mj-server/src/server/api/routes.rs` and `sessions.rs`, with the same state passed through `SubagentBackend` and `ExportRuntime`. The watch selects server shutdown without holding upgrade admission. `mj-server/src/web/viewer.js` owns cancellation for the resume episode, renders progress in `resume-wiki-progress`, and refreshes the current query once after successful completion. The refresh supersedes any outstanding pre-sync read. It follows no reconnect or replacement daemon; failure replaces the bar with an error, and leaving the route cancels pending work. The old five-second and bounded two-second query polling paths are removed.

### Milestone 4: progress bars

Add a small progress-bar span helper in `mj-tui` (near `mj_chat::spinner`, or in `mj-tui` if only used there) that renders a fixed-width bar of block characters, the counts, and the tool name, for example `Syncing Claude Code 2/7 ▕████░░░░░░▏ 1,204/3,090`; before a total is known it shows the spinner and "Scanning <tool>". Open dialog: draw it in the results title line, replacing the "index syncing, more may arrive" and "index building, more may arrive" notices (keep the VersionMismatch notice). Sessions filter: draw it on a new line directly below the create|open|filter row while the filter's sync is running, and collapse the line when it finishes. A failed pass shows "Index sync failed: <error>" in the same place until the surface closes.


## Concrete Steps

From the repository root `/home/jonathan/Projects/mjolnir/.mj/clones/9fdb43883da92c01fbe8ec2478ec4e17` (or the implementer's detached worktree):

    cargo check -p brokk-mj-sessionwiki -p brokk-mj-daemon -p brokk-mj-client -p brokk-mj-server -p brokk-mj-tui -p brokk-mjolnir
    cargo test -p brokk-mj-sessionwiki wiki
    cargo test -p brokk-mj-daemon wiki
    cargo test -p brokk-mj-tui resume
    cargo clippy -p brokk-mj-sessionwiki -p brokk-mj-daemon -p brokk-mj-client -p brokk-mj-server -p brokk-mj-tui -p brokk-mjolnir --all-targets -- -D warnings

Run `cargo test` outside the sandbox. Use the dev profile. For hook-driven live checks, build the CLI separately with `cargo build -p brokk-mjolnir --features test-hooks` and the real worker separately with `cargo build -p brokk-mj-worker`; do not enable test hooks on that worker. Use an allowlisted subprocess environment with private `MJ_CONFIG_DIR`, `MJ_DATA_DIR`, `SESSIONWIKI_DATA` and `HOME`, preserving the test-owned `MJ_TEST_HOOK` and `MJ_TEST_HOOK_DIR` when needed. Ambient session/provider/GitHub variables must not enter the fixture. Every CLI/TUI invocation uses `--instance swsync`, and teardown stops that named daemon before deleting fixture files. The browser contract is checked with `node --test tests/e2e/web/viewer.unit.test.mjs` and, from `tests/e2e/web`, `npx playwright test resume.spec.js --project deterministic`. The latter passed all 22 cases, including the progress bar and final re-query.


## Validation and Acceptance

Tests to add (only the kinds AGENTS.md allows): a tricky-condition test that a parked pass cannot supply rows before its commit. Use the existing `sessionwiki_sync_pass` test hook (`mj_core::test_hooks`) to park a pass, request a target, query while parked and see no new message, release the pass and wait for completion, then query once and see the new message. Also test that requests during a running pass coalesce into exactly one following pass whose generation is the returned target. Add a TUI render golden for each surface showing the bar (extend the existing resume render tests in `mj-tui/src/resume/tests.rs` and the filter render test near `mj-tui/src/render/tests.rs:2895` rather than adding families).

End-to-end in an isolated instance (never the default instance): build, run `mj --instance swsync`, create a session, send a message containing a unique word, open the open dialog and type the word: the bar appears in the results title, fills, and the session appears without retyping. Repeat with the Sessions filter: the bar appears below the create|open|filter row and the list narrows to the session when it finishes. Check the instance's daemon log has INFO lines for each pass start and end. Stop the daemon with `mj --instance swsync daemon stop`.


## Idempotence and Recovery

No schema migration is involved. The protocol bump is handled by the normal daemon handoff on upgrade. The SessionWiki release is additive (new function; `sync_with` unchanged), so older mjolnir builds keep working with the new crate.


## Interfaces and Dependencies

In `brokk-sessionwiki` `src/index.rs`:

    pub struct SyncProgress<'a> { pub tool: &'a str, pub adapter: usize, pub adapters: usize, pub done: usize, pub total: Option<usize> }
    pub fn sync_with_progress(conn: &mut Connection, adapters: &[Box<dyn Adapter>], since: Option<i64>, on_progress: &mut dyn FnMut(&SyncProgress<'_>)) -> Result<()>

In `mj-sessionwiki`: `WikiIndexer::request_sync(&self, full: bool) -> u64`, `WikiIndexer::state(&self) -> WikiSyncState`, `WikiIndexer::watch(&self) -> tokio::sync::watch::Receiver<WikiSyncState>`, `WikiIndexer::sync_now(&self, full: bool) -> Result<()>` (waits until a finished state covers its target; succeeds when `successful >= target`, otherwise returns the latest failure). Query functions read the index directly, with no successful-pass stamp tie.

In `mj-client/src/daemon.rs`: `DaemonAction::WikiSyncRequest`, `DaemonAction::WikiSyncWatch { epoch: String, after: u64 }`, `WikiSyncStatus { epoch, version, requested, started, finished, successful, outcome, running: Option<WikiSyncProgress>, target: Option<u64> }`, `WikiSearchPage { rows, status }` and `SessionTextSearchPage { matches }`, and `PROTOCOL_VERSION = 107`. Sync request/watch use one-shot RPCs and surface any refusal or transport loss. Attachment behavior is restored to its pre-feature owner. Background connection creation uses its pre-change `connect_or_start` behavior.


Revision 2026-11-18: recorded parent-approved epoch and successful-generation corrections before implementation.

Revision 2026-11-18: extended the implementation to the web consumer after finding its topping_up dependency; parent approved matching HTTP endpoints and episode flow.

Revision 2026-11-18: completed implementation, targeted validation and isolated progress/freshness acceptance; recorded the parent's watch-only coverage rule and the retained-dialog confirmation transition.

Revision 2026-11-18: addressed the overlapping review finding about cached web query generations surviving daemon epoch changes.

Revision 2026-11-18: search answers now carry epochs as approved by the parent; both surfaces re-arm sync when a post-completion query reaches a replacement daemon. Verified fetched origin/master 455e4f1c6 still uses protocol 106.

Revision 2026-11-18: second review fixed passive reconnection after explicit stop, preserved selection through background refresh, and tied successful coverage to the SQLite cache snapshot. All affected targeted regressions and Clippy checks passed.

Revision 2026-11-18: third review records successful-attachment readiness at its existing owner, preserves failed full demand without hidden retries, and separates delayed replies from post-completion invalidation. Authentication loss retires the visit. Targeted validation passed; origin/master 1fd20b265 still uses protocol 106.

Revision 2026-11-18: parent approved retaining the readiness baseline from the active connection. The prepublished-counter regression and protocol reconnect case pass, as does affected-crate Clippy.

Revision 2026-11-18: completion searches now retain cancellable read-task ownership through attachment-driven recovery. Existing minimized-pane golden frames show a bar with counts and error cause first. Focused protocol/cancellation, browser503 and golden checks plus affected Clippy pass.

Revision 2026-11-18: browser completion reads now retry without a new mutation, under their own query/visit abort controller. Full affected Resume suite25/25 and Node checks pass. Parent declined old-tab compatibility and hypothetical manual-backup identity changes.

Revision 2026-11-18: background RPCs and passive Attach now surface typed one-shot non-admission for attachment-owned retry; foreground Attach is unchanged. Backend failures stay visible as500, queued targets show waiting instead of earlier-pass counts. All affected checks passed after resuming interrupted validation.

Revision 2026-11-18: removed connection-error guessing from terminal read retries; shared attachment-owned acquisition handles discovery failures separately from query results. Extended real-protocol regression passes from discovery failure through disconnect/non-admission to final data.

Revision 2026-11-18: failed replacement sync no longer strands the current GET; background progress/results preserve restore controls, and narrow dialog titles prioritize useful sync/error content. Fresh origin/master58c2c14ba still uses protocol106.

Revision 2026-11-18: completion refresh remains visibly pending with retained rows; terminal epoch transitions refresh before replacement outcome. HTTP/terminal sync requests refuse draining admission with distinct work labels. Focused checks passed after relay interruption.

Revision 2026-11-18: simplified to the user's one-daemon-per-episode design; deleted reconnect, readiness and query-coverage machinery. Focused Rust checks, updated goldens, all affected browser cases and six-crate Clippy pass. Golden fixtures now explicitly order initial and completion reads, and the queued-progress test waits for its first watch before advancing.

Revision 2026-11-18: scoped failure ownership to the actual final read and composed independent history/sync title status; all affected checks pass. Connection and shared fetch policy remain as explicitly retained in the latest design decision.

Revision 2026-11-18: compact titles retain an independent history-failure indication; restored the original blank live-text fast path. Only affected golden/CLI tests and Clippy rerun; all pass.

Revision 2026-11-18: final-search status shares the history/sync title composition; sync completion checks the existing dashboard shutdown owner before starting reads. Focused tests and Clippy pass; external-rebuild pre-change behavior retained by explicit parent ruling.

Revision 2026-11-18: disabled web index search clears stale query state so metadata rows remain accessible. Terminal archive/live/filter reads and the follower share cancellable task supervision and existing join-error reporting. Focused browser, panic/cancellation, Node and Clippy checks pass.
