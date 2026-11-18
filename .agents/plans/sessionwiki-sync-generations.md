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
- [ ] (2026-11-18) Milestone 2 (implemented; validation pending): indexer generation state machine, logging, removal of the staleness trigger.
- [ ] (2026-11-18) Milestone 3 (implemented; validation pending): protocol actions and search generation, dashboard flows for dialog and filter.
- [ ] (2026-11-18) Milestone 4 (implemented; golden validation pending): progress bars in both surfaces, replacing the syncing notices.
- [ ] Validation in an isolated instance; review; commit and push.


## Surprises & Discoveries

The existing HTTP search handler also triggers stale syncs through backend traits; those hooks must be removed with the daemon staleness trigger. The library progress callback is available in the sibling checkout `/home/jonathan/Projects/sessionwiki-progress` while its release is prepared. The first sibling checkout used the upstream package by mistake; the corrected fork package is `brokk-sessionwiki` 0.34.27. Workspace package names use the `brokk-` prefix, and the CLI package is `brokk-mjolnir`; the concrete validation commands below use these actual names.


## Decision Log

- Decision: searches no longer trigger syncs; the surfaces request one explicitly (dialog open, first filter keypress). Rationale: the user asked for exactly this ("sync on dialog open or on first filter keypress") and called the 60-second gate dumb; one explicit trigger per surface episode makes "the sync this search waits for" well defined. Date: 2026-11-18.
- Decision: the indexer owns one state with monotonically increasing generation numbers, and every reader decides freshness from generations, never from booleans sampled at different times. Rationale: AGENTS.md concurrency rule (one owner per fact, single state machine); the stale-rows race came from two independent samples. Date: 2026-11-18.
- Decision: a failed pass (including SQLite busy) finishes its generation as failed and is shown, rather than silently re-arming a retry. Waiting surfaces then stop waiting and show the error. Rationale: fail visibly; a hidden retry flag is what left a surface waiting on nothing. Date: 2026-11-18.
- Decision: progress comes from a new callback in SessionWiki itself, published as a new crate version. Rationale: user choice; per-session progress exists only inside the library's sync loop. Date: 2026-11-18.
- Decision: the dashboard follows sync state by long-polling a new daemon action that returns when the state changes, not by timed polling. Rationale: the daemon protocol is request/response; a long poll gives push behavior without inventing an interval. Date: 2026-11-18.


- Decision: include a per-indexer epoch UUID in sync status and in watch requests; epoch mismatch returns immediately and causes the surface to request a new target. Rationale: counters alone cannot distinguish a restarted daemon, and an old version counter can strand a watch. Approved by parent, 2026-11-18.
- Decision: track a separate `successful` generation, advanced only on successful completion, and sample it before each query. Rationale: a failed later pass must not erase the generation a query can safely claim. Approved by parent, 2026-11-18.

- Decision: give the web resume/archive viewer the same explicit sync episode, progress display, and final re-query through `POST /api/v1/wiki/sync` and `GET /api/v1/wiki/sync/watch?epoch=...&after=...`. Rationale: removing the HTTP staleness hook without a replacement would strand the web search on background updates. Extend the existing web resume spec. Approved by parent, 2026-11-18.


## Outcomes & Retrospective

(not started)


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

Replace the indexer's flags with one state guarded by a single `std::sync::Mutex` and published through one `tokio::sync::watch::Sender<WikiSyncState>` (send while holding the lock, so watchers see transitions in order). `WikiSyncState` holds: `requested` (highest generation anyone asked for), `started` (generation of the running or last started pass), `finished` (last generation that ended), `last_outcome` (succeeded, or failed with an error string), `full_requested`, and `running: Option<SyncProgress>` (phase label, tool, adapter position and count, done, total). The rules: a pass with generation N covers everything committed before N started. `request_sync(full)` returns the target generation, which is `started + 1` (the next pass to start), and raises `requested` to it. The worker starts pass `started + 1` whenever `requested > started`, coalescing any number of requests into one pass. At pass end it sets `finished = started` and the outcome in the same locked update that clears `running`. There is no window in which a reader sees "not requested, not running" while a requested pass is still to run. A failure, including SQLite busy, ends the generation as failed (log it at WARN with the error) and does not re-arm a hidden retry; the next request starts a new pass. `sync_now(full)` becomes: request, then wait on the watch until `finished >= target`, returning the outcome. Keep `changes()` for observe.rs as a receiver that advances on each finished generation (or switch observe.rs to the new watch; one owner either way). Keep `last_success()` only if `server_runtime/api.rs:473` still needs it after the staleness trigger is gone; otherwise remove it.

Progress: `sync_blocking` calls `sync_with_progress` for both phases and updates `running` through the same lock and watch. Throttle publication to state changes the UI can show (for example, only when the integer percentage or the tool changes), so a 50,000-session pass does not wake watchers 50,000 times; this is a coalescing rule, not a cap.

Logging, at INFO in target `mj_sessionwiki` (or the module's existing target): pass start with generation, full or incremental, and `since`; pass end with generation, duration, sessions re-parsed per phase, and outcome. Busy and other failures log at WARN with the generation.

Remove `sync_is_stale`, `SYNC_STALE_AFTER`, and `request_wiki_sync_if_stale`. `wiki_search` and `session_text_search` no longer request syncs. Each search samples `finished` generation of the last successful pass before running its query, and returns it as `generation` in its answer: the rows include at least everything that pass committed. Delete the test `a_session_text_search_asks_for_a_sync_like_the_resume_search` (state.rs around 2253) since the behavior it pins is intentionally removed, and replace it with the tricky-condition test described in Validation.

### Milestone 3: protocol and dashboard flows

Bump `PROTOCOL_VERSION` to 107. Add `DaemonAction::WikiSyncRequest` returning a `WikiSyncStatus` (the client view of `WikiSyncState` plus the returned `target`), and `DaemonAction::WikiSyncWatch { epoch: String, after: u64 }` that returns the state once its version (a counter bumped on every published change) exceeds `after`, or immediately if it already does. A long poll must not hold daemon upgrade admission (it is a wait, like sub-agent waits, and is cancelled by a handoff; the client retries against the new daemon). Replace `WikiStatus.topping_up` with the generation: `WikiSearchPage` carries `status.state` and `generation`, and `session_text_search` returns its matches with `generation` too. Update every consumer the compiler finds.

Dashboard flow, the same for both surfaces: when the dialog opens, or on the first keypress of a filter episode (from the filter being empty or closed to having text), spawn one background task that calls `WikiSyncRequest`, remembers `target`, then loops on `WikiSyncWatch`, sending each state to the dashboard as a `DashboardIoUpdate` for the progress bar. When `finished >= target` it sends a final update and the surface re-runs its current query once (dialog: archive and live searches; filter: the session text search). Keystrokes keep their 250 ms debounce and search immediately from the index as it is; they do not request syncs and do not follow. Remove `follow_wiki_search`'s timed loop. A result whose `generation >= target` is final for that query. The task is cancelled when the dialog closes or the filter is cleared/closed (drop guard, as `spawn_wiki_search` does today). If the daemon connection drops mid-watch, reconnect and resume watching the same target; if the new daemon's generations restarted (target no longer meaningful because `started` is lower than before), request again. If the pass failed, show the error in place of the bar and stop.

### Milestone 4: progress bars

Add a small progress-bar span helper in `mj-tui` (near `mj_chat::spinner`, or in `mj-tui` if only used there) that renders a fixed-width bar of block characters, the counts, and the tool name, for example `Syncing Claude Code 2/7 ▕████░░░░░░▏ 1,204/3,090`; before a total is known it shows the spinner and "Scanning <tool>". Open dialog: draw it in the results title line, replacing the "index syncing, more may arrive" and "index building, more may arrive" notices (keep the VersionMismatch notice). Sessions filter: draw it on a new line directly below the create|open|filter row while the filter's sync is running, and collapse the line when it finishes. A failed pass shows "Index sync failed: <error>" in the same place until the surface closes.


## Concrete Steps

From the repository root `/home/jonathan/Projects/mjolnir/.mj/clones/9fdb43883da92c01fbe8ec2478ec4e17` (or the implementer's detached worktree):

    cargo check -p brokk-mj-sessionwiki -p brokk-mj-daemon -p brokk-mj-client -p brokk-mj-tui -p brokk-mjolnir
    cargo test -p brokk-mj-sessionwiki wiki
    cargo test -p brokk-mj-daemon wiki
    cargo test -p brokk-mj-tui resume
    cargo clippy -p brokk-mj-sessionwiki -p brokk-mj-daemon -p brokk-mj-client -p brokk-mj-tui -p brokk-mjolnir --all-targets -- -D warnings

Run `cargo test` outside the sandbox. Use the dev profile.


## Validation and Acceptance

Tests to add (only the kinds AGENTS.md allows): a tricky-condition test on the indexer that a search answer's generation never claims a pass whose commit it did not see. Drive it with the existing `sessionwiki_sync_pass` test hook (`mj_core::test_hooks`) to park a pass, request a target, run a search while parked and see its generation below the target, release the pass, and see the next search report `generation >= target` and include the new message. Also test that requests during a running pass coalesce into exactly one following pass whose generation is the returned target. Add a TUI render golden for each surface showing the bar (extend the existing resume render tests in `mj-tui/src/resume/tests.rs` and the filter render test near `mj-tui/src/render/tests.rs:2895` rather than adding families).

End-to-end in an isolated instance (never the default instance): build, run `mj --instance swsync`, create a session, send a message containing a unique word, open the open dialog and type the word: the bar appears in the results title, fills, and the session appears without retyping. Repeat with the Sessions filter: the bar appears below the create|open|filter row and the list narrows to the session when it finishes. Check the instance's daemon log has INFO lines for each pass start and end. Stop the daemon with `mj --instance swsync daemon stop`.


## Idempotence and Recovery

No schema migration is involved. The protocol bump is handled by the normal daemon handoff on upgrade. The SessionWiki release is additive (new function; `sync_with` unchanged), so older mjolnir builds keep working with the new crate.


## Interfaces and Dependencies

In `brokk-sessionwiki` `src/index.rs`:

    pub struct SyncProgress<'a> { pub tool: &'a str, pub adapter: usize, pub adapters: usize, pub done: usize, pub total: Option<usize> }
    pub fn sync_with_progress(conn: &mut Connection, adapters: &[Box<dyn Adapter>], since: Option<i64>, on_progress: &mut dyn FnMut(&SyncProgress<'_>)) -> Result<()>

In `mj-sessionwiki`: `WikiIndexer::request_sync(&self, full: bool) -> u64`, `WikiIndexer::state(&self) -> WikiSyncState`, `WikiIndexer::watch(&self) -> tokio::sync::watch::Receiver<WikiSyncState>`, `WikiIndexer::sync_now(&self, full: bool) -> Result<()>` (waits for its own target).

In `mj-client/src/daemon.rs`: `DaemonAction::WikiSyncRequest`, `DaemonAction::WikiSyncWatch { epoch: String, after: u64 }`, `WikiSyncStatus { epoch, version, requested, started, finished, successful, outcome, running: Option<WikiSyncProgress>, target: Option<u64> }`, `WikiSearchPage { rows, status, generation }`, and `PROTOCOL_VERSION = 107`.

Revision 2026-11-18: recorded parent-approved epoch and successful-generation corrections before implementation.

Revision 2026-11-18: extended the implementation to the web consumer after finding its topping_up dependency; parent approved matching HTTP endpoints and episode flow.
