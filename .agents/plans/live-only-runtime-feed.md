# Send only live sessions to terminal clients and load stopped sessions on demand

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture


Mjolnir's daemon (the background control process started as `mj daemon-run`, code in the `mj-controller` crate) streams a "runtime feed" to every terminal dashboard (the `mj` TUI). Today that feed carries every session record the daemon holds, including every stopped (suspended) session ever created. On a long-lived store this is mostly history: one real store has 1,230 sessions, of which 1,095 are stopped. Every dashboard downloads all of them when it attaches, keeps them in memory, and receives every change to them, although the Sessions pane hides stopped sessions by default. The same "everything" shape already caused one outage: the workspace menu asked for a single-frame snapshot of every session and failed once that reply passed the 8 MiB frame limit (fixed in commits b101287c and 96840092).

After this change, the feed carries only live sessions. A stopped session leaves the feed when it stops. The one terminal surface that lists stopped sessions, the resume dialog (opened from the dashboard to resume or import a session), asks the daemon for them when it opens. The advanced setting "Show suspended sessions", which put stopped sessions in the Sessions pane, is removed. A user can see this working by opening a dashboard against a store with many stopped sessions: the first feed frame is small, the Sessions pane is unchanged, and the resume dialog still lists every stopped session and resumes them as before.

The web viewer and the HTTP API (`/api/v1/...`, used by `mj sessions` and `mj resume`) keep their current behavior in this plan. They run inside the daemon process and read the same projection, so this plan gives them the full record set explicitly instead of through the feed. Making the browser's own snapshot live-only is a separate change, recorded under Outcomes.


## Progress


- [x] (2026-10-01) Surveyed every consumer of feed records in the TUI, the daemon, the web server, and the HTTP API, and every place that could serve stopped sessions on demand. Findings are folded into Context and Orientation and the Decision Log.
- [x] (2026-10-01) Milestone 1: removed the "Show suspended sessions" setting. `AdvancedConfig.show_stopped_sessions` and the deprecated top-level `Config.show_stopped_sessions` are gone; `StoredConfig` still accepts the old top-level key and `AdvancedConfig` ignores the old advanced key (test `retired_stopped_session_filters_load_and_are_dropped_on_save`). Full `cargo test`, clippy and fmt passed.
- [x] (2026-10-01) Milestone 3 (done before Milestone 2, see Decision Log): `DaemonAction::ResumeCandidates` and `DaemonAction::GoStartupSession`, `DaemonReply::is_chunked` deciding chunked replies for both ends (`ReplyChunk` replaces `RuntimeChunk`), `PROTOCOL_VERSION` 50. Daemon tests: `resume_candidates_are_inactive_top_level_sessions_with_every_adopted_native_session`, `go_starts_in_the_remembered_session_or_the_newest_eligible_one`, `resume_candidates_larger_than_a_frame_arrive_whole`.
- [x] (2026-10-01) Milestone 4: the resume dialog requests candidates on open (`spawn_resume_candidates`, `DashboardIoUpdate::ResumeCandidates`, `DashboardState::apply_resume_candidates`), shows "Loading suspended sessions…" and withholds Mjolnir and Import rows until the answer arrives; `DashboardState::session_record` serves live then loaded stopped records to the resume wizard, Hel-row Destroy, confirmations and operation placeholders; import completion no longer waits for the feed to show the stopped record; `mj go` asks the daemon (`begin_go` takes the chosen record); checkpoint sizes follow the loaded stopped records. Test `the_mjolnir_tab_lists_the_daemons_answer_and_resumes_a_session_the_feed_lacks`. Full suite passed.
- [x] (2026-10-01) Milestone 5, part 1: a pinned pane whose session leaves the feed during a Suspending operation keeps its "is suspended, so it was unpinned" notice; a parent that leaves in the same frame as the sub-agent its suspend stopped is still recognized (no reopen); the CLI keeps the last record of a session that leaves while its chat exists (`DashboardContext::departed_records`, `chat_session_record`) so drafts and read positions save to the right workspace. Tests `a_pinned_session_that_leaves_the_feed_by_suspending_says_why_its_pane_emptied`, `a_parent_and_sub_agent_leaving_together_after_a_suspend_is_still_reported`, `a_chat_detaching_after_its_session_left_the_feed_still_saves_its_draft`.
- [x] (2026-10-01) Milestone 2 with Milestone 5, part 2: `mj_core::state::session_is_live` and `live_session_ids` define the live set once. `RuntimeState::capture_runtime` builds the full projection as before, keeps it in `RuntimeHistory::full` for `runtime_publication` (the web server), and publishes `RuntimeHistory::live_projection`, derived from the previous live projection by what changed, with a `debug_assert` against a full evaluation. `RuntimeMetadata.launch_recency` (`mj_client::runtime_feed::launch_recency`) feeds new-session defaults; it is recomputed only when a record's launch inputs change. The dashboard's startup state is filtered with `live_session_ids` (durable active moves as the operations) and starts with the launch summary of every record. Test `the_terminal_feed_follows_live_sessions_and_drops_stopped_ones`.
- [x] (2026-10-01) Milestone 6, measurement and checks: on a slim copy of the 1,232-session store, the feed's records went from 5,868,093 bytes (1,143 records) to 1,036,436 bytes (104 live records); the resume dialog's on-demand list is 2,581,689 bytes (813 records), sent in chunks. Full dev-profile `cargo test`: one failure, `daemon::tests::cancelled_api_startup_is_not_submitted_after_daemon_reconstruction`, which passed alone three times and in three reruns of the daemon test module (likely a timing flake in startup-prompt cancellation, unrelated to the feed; not confirmed). clippy and fmt passed.
- [ ] Milestone 6, remaining: exercise suspend, resume, import and `mj go` by hand in a named instance with a fresh store; push when the user asks.


## Surprises & Discoveries


- Observation: `SessionState::is_active()` already counts `Error` and `Parked` as active. Only `Stopped`, `Lost` and `DestroyedWithDataLoss` are inactive.
  Evidence: `mj-core/src/state.rs` `is_active` (around line 771).

- Observation: a resume keeps the record `Stopped` for most of the operation. The record becomes `Provisioning` only after repository preflight and checkpoint verification. A filter on `is_active()` alone would drop a resuming session from the feed and bring it back later.
  Evidence: `mj-controller/src/controller/resume.rs` sets `record.state = SessionState::Provisioning` near line 1633; `mj-controller/src/server/api/wait_policy.rs` lines 32-36 say "Its durable record still says stopped".

- Observation: several daemon lifecycle operations run on a stopped record from start to finish: DestroyStopped, ArchiveStopped, Cleanup, ForceDestroy of a stopped record, a Suspend of an already stopped record, and the middle of a Move. A completed Suspend with deferred cleanup stays "visible" while its record is already `Stopped`.
  Evidence: `mj-controller/src/daemon.rs` `ActiveLifecycle::is_visible`; `mj-controller/src/controller/lifecycle.rs` around 1090-1126; `mj-controller/src/controller/move_session.rs` around 1282-1330.

- Observation: the dashboard treats a record missing from the feed as gone. `DashboardState::set_state` removes its row, its native-agent child rows, its session details, and silently empties every pane showing it. Today a session that stops stays in the map with state `Stopped`, and several behaviors hang off that transition (the "unpinned from its pane" notice, the stopped sub-agent read-only pane, draft saving on detach).
  Evidence: `mj-tui/src/ingest.rs` `set_state` (around 587-666); `mj-tui/src/notify.rs` `release_suspended_pane` (around 278-288); `mj-cli/src/dashboard.rs` `record_chat_detach_state` (around 1864-1893) fails with "unknown session" without a record.

- Observation: imported native sessions are created `Stopped`, and the import completion waits until the feed shows the new record. In a live-only feed that never happens, and the import would report "The operation was saved, but the runtime feed has not confirmed it".
  Evidence: `mj-controller/src/import/native_import.rs` around line 168; `mj-cli/src/dashboard/io.rs` around 516-541.

- Observation: a sub-agent closed on its own by its parent stays `Stopped` with its parent relation, and the Sub-agents view shows it read-only. Children stopped by their parent's suspend are destroyed instead.
  Evidence: `mj-controller/src/daemon/close.rs` around 133-197 and 357-377; `mj-tui/src/stopped_subagents.rs`.

- Observation: the web viewer, `/api/v1/sessions`, `GET /api/v1/sessions/{id}`, `POST .../resume`, `POST .../wait`, and web actions all resolve sessions from the projection that `RuntimeState::runtime_publication` returns. Filtering the shared projection would remove stopped sessions from all of them.
  Evidence: `mj-controller/src/server_runtime/run.rs` lines 34 and 247-301 (`controller.state.sessions = runtime.records`); `mj-controller/src/server/validation.rs` `require_session_record` (around 720).

- Observation: a daemon must never be started on a copy of a live store, even in a named instance. At startup it reattaches to the workers of the copy's live sessions, which belong to the real daemon, and runs deferred cleanup for stopped sessions that still name a target, which can remove real containers and worktrees. The live store is also 7.5 GB, so a full copy is impractical.
  Evidence: `mj-controller/src/daemon/close.rs` around 582-603 (startup cleanup of stopped records with a target); `ls -la ~/.local/share/mjolnir/mj.sqlite3` reports 7,501,807,616 bytes.

- Observation: the full set of stopped records in the measured store serializes to about 3.3-3.7 MB, dominated by `acp_session_title` (average 1.6 KB, maximum 76 KB). That fits one 8 MiB frame today with only about 2x headroom.
  Evidence: read-only measurement of `~/.local/share/mjolnir/mj.sqlite3` during the survey.


## Decision Log


- Decision: the live set is every session whose projected state is not `Stopped`, plus a `Stopped` session that has a visible lifecycle operation, a durable move whose `MoveOperation::is_active()` is true, or that is a sub-agent of a session already in the live set (applied repeatedly so nested sub-agents follow their parent).
  Rationale: the user's model is "the feed holds live sessions". `Lost` and `DestroyedWithDataLoss` are not live in the sense of running, but they are unsettled outcomes that need the user's attention, and the dashboard's failure notifications fire on the transition into them; dropping them from the feed would make a crash silent. They are rare and do not accumulate in normal use. `Stopped` is the settled, accumulating state, so it is the one excluded. Operations that run on a stopped record must keep it visible so its progress row does not flicker. A stopped sub-agent of a live parent is part of that parent's live view (the read-only sub-agent pane).
  Date/Author: 2026-10-01, Claude (from the user's request "all sessions including suspended/stopped is nuts").

- Decision: the predicate lives in one function in `mj-core` and every caller uses it: the daemon when it captures the feed, and the dashboard when it builds its first state from the store before the first feed frame arrives.
  Rationale: AGENTS.md forbids two predicates for one question. The dashboard's startup state is otherwise the full store, which the first frame would then shrink, firing removal handling for every stopped session.
  Date/Author: 2026-10-01, Claude.

- Decision: filter in `RuntimeState::capture_runtime` for the terminal feed only. The same capture also keeps the unfiltered records, and `runtime_publication()` (used only by the in-process web server) returns those, so the web server and HTTP API keep today's behavior.
  Rationale: both views come from one capture under one lock, so they cannot disagree about which records exist. Changing the web viewer's resume list, `mj sessions`, `mj resume` and `wait` is a separate design with its own surfaces.
  Date/Author: 2026-10-01, Claude.

- Decision: remove the "Show suspended sessions" setting entirely, from the TUI and from `AdvancedConfig`. Keep reading an old `[advanced] show_stopped_sessions` key and ignore it.
  Rationale: the user chose removal ("just get rid of it"). Stopped sessions then appear only in the resume dialog, which matches the web viewer's rule (`ViewerLifecycleCategory::is_dashboard_visible`) and the module comment of `mj-tui/src/resume.rs` ("the one surface that lists sessions which are not live"). `AdvancedConfig` is `#[serde(default)]` without `deny_unknown_fields`, so an old key is ignored once the field is gone, and the config version does not need to change.
  Date/Author: 2026-10-01, user and Claude.

- Decision: the resume dialog loads full `SessionRecord`s for all inactive sessions (the Hel tab's rows) in one request when it opens, sent as a chunked reply, together with the import de-duplication data computed from all records. The wizard and Hel-row actions read stopped records from that loaded set through one lookup helper. No paging.
  Rationale: the wizard reads many record fields (checkpoint, profile, target, mounts, allocation, compatibility), so a preview type would need a second fetch and a second code path. Loading happens only when the user opens the history view, which is what the user asked for. Chunking removes the frame-size cliff that a single frame has. Paging would need the dialog, the wiki search merge and the sort to work over partial data, for no current need.
  Date/Author: 2026-10-01, Claude.

- Decision: `mj go` asks the daemon for its startup session (`GoStartupSession`), and the daemon applies today's eligibility rule over all records. A stopped result carries its full record for the resume wizard.
  Rationale: the rule needs stopped records, which the dashboard no longer holds.
  Date/Author: 2026-10-01, Claude.

- Decision: new-session defaults (`most_recent_configured_session`, `bundle_ids_by_recent_creation` in `mj-tui/src/wizards.rs`) read a small summary that the daemon publishes in the feed metadata: for each distinct (project, profile, target) combination used by any session, the newest creation time. The two helpers compute the same answers from it as they do from all records today.
  Rationale: today the defaults come from all records, so a user who suspends everything would otherwise lose them. The summary is bounded by the number of combinations actually used, not by history.
  Date/Author: 2026-10-01, Claude.

- Decision: untitled `mj go` sessions are numbered among the live sessions of their workspace (`go_conversation_title`), not among all sessions.
  Rationale: the number is a display label for live sessions; destroying sessions already changes it. Carrying a count of all history for it is not worth a new field.
  Date/Author: 2026-10-01, Claude.

- Decision: also remove the deprecated top-level `Config.show_stopped_sessions` field, keeping only the file-format acceptance in `StoredConfig`.
  Rationale: it already had no effect; keeping a dead field on `Config` invites someone to read it.
  Date/Author: 2026-10-01, Claude.

- Decision: implement in the order 3, 4, 5, 2: requests first, consumers next, then the dashboard's handling of sessions leaving the feed, and the feed filter last.
  Rationale: every commit then works on its own. Filtering first would leave commits where the resume dialog is empty and imports never complete.
  Date/Author: 2026-10-01, Claude.

- Decision: the Mjolnir tab lists the loaded stopped records plus any inactive record the feed itself carries, preferring the feed's copy of a record.
  Rationale: `Lost` and data-loss sessions stay in the feed and can appear after the dialog opens; a loaded record that has since been resumed must not be listed as stopped.
  Date/Author: 2026-10-01, Claude.

- Decision: advance `PROTOCOL_VERSION` (in `mj-client/src/daemon.rs`) once for this change.
  Rationale: new requests and a changed feed meaning need a daemon of the same version; ordinary startup already replaces an older daemon.
  Date/Author: 2026-10-01, Claude.


## Outcomes & Retrospective


Terminal dashboards now receive only live sessions: in the measured store the feed's records shrank by about 82%, and the size no longer grows with history. Stopped sessions reach the terminal only when the resume dialog opens, through a chunked reply with no frame limit. The setting "Show suspended sessions" is gone. Not yet done: a manual run of the dashboard in a named instance. Known follow-up outside this plan: the web viewer's own `/api/snapshot` and change stream still send every session to the browser, and `/api/v1/sessions` lists every session without paging.


## Context and Orientation


Terms used here:

A "session" is one agent conversation. Its durable description is a `SessionRecord` (`mj-core/src/state.rs`), stored in SQLite and held in memory by the daemon in `controller.state.sessions`, a `SnapshotMap<String, SessionRecord>` (`mj-core/src/snapshot_map.rs`, an immutable map whose clones share data). A record has a `state: SessionState`. "Stopped" (shown to users as "suspended") means the session was checkpointed and its worker torn down; it can be resumed.

The "daemon" is the control process (`mj daemon-run`, crate `mj-controller`). "Workers" run agents. The "runtime feed" is how the daemon tells dashboards what changed: `RuntimeState::capture_runtime` in `mj-controller/src/daemon/feed.rs` builds a `RuntimeProjection` (defined in `mj-client/src/runtime_feed.rs`) with fields `revision`, `records`, `subagents` (parent/child relations keyed by child id), `sessions` (worker views, already live-only), `moves` (durable move operations), `native_agents` (agents a harness spawned inside a session, keyed by owner and child), and `metadata` (config, workspace names, lifecycles, reviews, notices, quotas). `RuntimeHistory` in the same file stores recent projections and serves `RuntimeFrame::Snapshot` (whole projection) or `RuntimeFrame::Delta` (keyed changes) to a client's cursor. The daemon handles `DaemonAction::RuntimeChanges` in `mj-controller/src/daemon/actions.rs` and sends large frames as 64 KiB `DaemonReply::RuntimeChunk` frames (`write_response` in `mj-controller/src/daemon/serve.rs`). The client reassembles them (`mj-client/src/daemon.rs`, `accepts_runtime_chunks`).

`owner.projected_records()` (`mj-controller/src/daemon/owner.rs`) is the record map the feed uses: `controller.state.sessions` with any session that has a pending close shown as `Closing`. A "lifecycle operation" is a daemon-run create, resume, suspend, move, destroy and so on, tracked in `owner.lifecycle: BTreeMap<String, ActiveLifecycle>`; `ActiveLifecycle::is_visible()` (in `mj-controller/src/daemon.rs`) says whether it should still be shown. Durable moves are `MoveOperation` values (`mj-core/src/state/session_move.rs`) with `is_active()`.

On the terminal side, `mj-controller/src/pollers/remote.rs` keeps a `RuntimeReplica` of the feed and sends `RuntimeStateUpdate` values to the dashboard. `DashboardContext::drain_runtime_state` and `apply_runtime_records` (`mj-cli/src/dashboard/drains.rs`) replace the dashboard's records and call `DashboardState::set_state` (`mj-tui/src/ingest.rs`), which diffs old and new records and removes everything belonging to an absent id. `DashboardState` (crate `mj-tui`) is the UI state; `DashboardContext` (crate `mj-cli`, `mj-cli/src/dashboard.rs`) owns IO and background tasks. All IO from the UI runs in background tasks spawned in `mj-cli/src/dashboard/io/spawn.rs`, reporting back as `DashboardIoUpdate` variants handled in `mj-cli/src/dashboard/io.rs`.

The resume dialog is `mj-tui/src/resume.rs`. `merged_resume_rows` builds its "Hel" tab from inactive, non-sub-agent records, and uses every record's `native_session_id` and local managed worktree root to hide already-imported native sessions on its "Import" tab. Picking a Hel row calls `begin_resume_for` (`mj-tui/src/wizards/dashboard/begin.rs`), which opens the resume wizard; the wizard reads the record from `self.state.sessions` in `begin.rs`, `targets.rs`, `render.rs`, `draft.rs`, `subagents.rs` and `dashboard_sessions.rs` (`compatible_profiles`). Wiki search results (`WikiRow`) annotate Hel rows. Checkpoint archive sizes for Hel rows are computed in `mj-cli/src/dashboard/surface.rs` (`refresh_controller_derived_state`).

`mj go` is a fast-start mode (`mj-tui/src/go.rs`): `go_startup_session` picks the remembered or most recent eligible session in the workspace and resumes it if stopped.

The web server runs inside the daemon (`mj-controller/src/server_runtime/run.rs`) and calls `RuntimeState::runtime_publication()`. It must keep receiving every record in this plan.

The setting to remove is `AdvancedConfig.show_stopped_sessions` (`mj-core/src/config/ui.rs`). Its other references: `mj-core/src/config.rs` (a deprecated top-level field that stays readable and ignored), `mj-tui/src/dashboard_sessions.rs` (`is_listed_top_level_session`), `mj-tui/src/row_index.rs`, `mj-tui/src/setup.rs`, `mj-tui/src/setup/schema.rs` (default, label "Show suspended sessions", summary count, help text), tests in `mj-core/src/config/tests.rs`, `mj-core/src/state/tests.rs`, `mj-tui/src/tests.rs`, `mj-tui/src/ingest/tests.rs`, `mj-tui/src/setup/tests.rs`, `mj-cli/src/dashboard/io.rs` (test module), and docs in `mj-core/assets/skills/mj/references/configuration.md` and `docs/src/content/docs/terminal-surface.mdx`.


## Plan of Work


Milestone 1 removes the setting. Delete `show_stopped_sessions` from `AdvancedConfig` and from the Setup schema, label, summary count and help text. In `is_listed_top_level_session` and `row_index.rs`, drop the branch that listed `Stopped` sessions. Keep the deprecated top-level `Config.show_stopped_sessions` handling as it is (it is already read and ignored). Update tests that set or toggle the setting: a test that toggled it to prove an unrelated preference is editable should toggle another advanced preference instead. Update both doc pages to say that suspended sessions are listed only in the resume dialog. Add a config test that a file containing `[advanced] show_stopped_sessions = true` still loads. At the end, the TUI has no way to list stopped sessions in the Sessions pane, and everything else behaves the same because the feed still carries them.

Milestone 2 defines the live set and filters the terminal feed. In `mj-core/src/state.rs` add `pub fn live_session_ids(sessions: &SnapshotMap<String, SessionRecord>, subagents: &SnapshotMap<String, SubagentRecord>, operations: &BTreeSet<String>) -> BTreeSet<String>` (adjust argument types to what `State` holds) implementing the rule in the Decision Log, where `operations` are ids with a visible lifecycle or an active durable move. In `capture_runtime`, compute that set from the projected records, `owner.lifecycle` (visible entries) and the committed moves, then publish to `RuntimeHistory` a projection whose `records`, `subagents` (by child id), `moves` and `native_agents` (by owner) are limited to it. Native agents are maintained incrementally today from changes in the committed owner map; when an owner enters or leaves the live set its whole child set must be added or removed even though the native table did not change, so recompute the per-owner membership from the live set each capture. Keep the unfiltered records, sub-agent relations and moves from the same capture, and return them from `runtime_publication()` so the web server is unchanged. Add `RuntimeMetadata.launch_recency`, the summary described in the Decision Log, computed from all records. Advance `PROTOCOL_VERSION`. Update the feed test that inserts a `Stopped` record and expects it in the delta.

Milestone 3 adds requests. Add `DaemonAction::ResumeCandidates` returning `DaemonReply::ResumeCandidates(ResumeCandidates)`, where `ResumeCandidates` holds the inactive, non-sub-agent records (full `SessionRecord`s), the durable moves of those sessions (for "move needs recovery" marks), and the import de-duplication data: every record's `(harness_kind, native_session_id)` and every local managed worktree root. Add `DaemonAction::GoStartupSession { workspace_id, last_session_id }` returning `Option<SessionRecord>`, applying the rule now in `go_startup_session` over all records; move that rule into the daemon so it exists once. Generalize chunked replies: let the reply type decide whether it is sent in chunks (`RuntimeChanges` and `ResumeCandidates`), and let the client accept chunks for exactly the actions whose replies are chunked, from one function shared by both ends in `mj-client/src/daemon.rs`. Add client methods on `DaemonClient`.

Milestone 4 moves the terminal consumers. When the resume dialog opens (`start_resume_discovery` in `mj-cli/src/dashboard/chat_tasks.rs`), spawn a background `resume_candidates` request, show a loading state in the Hel tab, and apply the result through a new `DashboardIoUpdate` variant and a `DashboardState::apply_resume_candidates(discovery_id, candidates)` that ignores stale discovery ids and rebuilds rows. Change `merged_resume_rows` to take the loaded candidates instead of reading `state.sessions` for Hel rows and de-duplication. Give `DashboardState` one lookup, `session_record(id)`, that returns the live record if present and otherwise the loaded candidate record, and use it in the resume wizard, in Hel-row actions (Destroy, move recovery) and in the import completion; leave live-only code on `state.sessions`. Trigger checkpoint archive size refresh from the loaded candidates instead of feed diffs. Make import completion use the import reply instead of waiting for the feed, then load candidates and open the resume wizard for the imported id. Make `mj go` request `GoStartupSession` in the background at startup and continue when the reply arrives.

Milestone 5 handles sessions leaving the live set. In `apply_runtime_records`, compute removed ids with their last records before replacing the map, and use those records for detach bookkeeping so drafts and read positions are saved with the right workspace instead of failing with "unknown session". Show the pane-release notice when a pinned pane's session leaves the feed, worded so it is true for a stopped or destroyed session. Change `subagents_stopped_by_suspend` so a child that leaves with its parent in the same frame is still recognized as stopped by the parent's suspend. Build the dashboard's startup state with `live_session_ids`. Point the new-session default helpers at `launch_recency`. Number untitled `mj go` sessions among live sessions.

Milestone 6 validates. Measure the feed on a slim copy of the large store: the full schema with only the session tables (no transcripts or events), loaded with `mj_controller::database::load_state_from` in a temporary test, without starting a daemon on it (see Surprises & Discoveries). Exercise suspend, resume and the resume dialog in a named instance with its own fresh store. Exercise suspend, resume, import, `mj go`, stopped sub-agents, destroy of a stopped session, and the web viewer's resume list. Run the full checks.


## Concrete Steps


Work from `/home/jonathan/Projects/mjolnir2`. After each milestone run, outside the sandbox:

    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo test

Do not stage `mj-controller/src/controller/move_session.rs` or `mj-controller/src/daemon/session_move.rs` unless the change needs them; they hold unrelated work in progress. Commit each milestone separately once its checks pass.

For Milestone 6, never start a daemon on a copy of a live store (see Surprises & Discoveries). Build a slim copy instead, with Python's `sqlite3` module: open the live store read-only (`file:<path>?mode=ro`), create every table in a new file, copy the rows of every table except the transcript and event tables (`api_events`, `materialized_transcript_items`, `native_agent_transcript`, `native_agent_replay`, `prompt_history`, `api_idempotency`, `session_turn_usage`, `session_provider_cost`, `project_discovery_changes`, `mount_history`), then create the indexes and triggers and copy `PRAGMA user_version`. Load it with `load_state_from` in a temporary ignored test and compare the serialized sizes of the full and live projections.

## Validation and Acceptance


Milestone 1: `cargo test -p brokk-mj-core config` passes, including the new test that an old `[advanced] show_stopped_sessions = true` file loads. The Setup screen has no "Show suspended sessions" row.

Milestone 2: a new daemon test proves that a `Stopped` record is absent from `runtime_changes` frames, that the same record with a visible Resume lifecycle is present, that a stopped sub-agent of a live parent is present, that a `Lost` record is present, and that `runtime_publication()` still contains the `Stopped` record. A test proves that a session that stops is removed by a delta (key with no value). These tests fail before the change.

Milestone 3: daemon tests prove `ResumeCandidates` returns exactly the inactive, non-sub-agent records and the de-duplication data, survives a reply larger than 8 MiB through chunking (drive it with more than 8 MiB of records, per AGENTS.md's rule on buffer boundaries), and that `GoStartupSession` picks the same session the old rule picked for a fixture with live and stopped sessions.

Milestone 4: TUI tests prove the Hel tab shows candidates after `apply_resume_candidates`, ignores a stale discovery id, and that `begin_resume_for` works for a session present only in the loaded candidates.

Milestone 5: a test proves that removing a session from the feed while its chat is attached saves its draft without an "unknown session" notice, and that a parent and child leaving in one frame after a suspend still trigger the stopped-by-suspend handling.

Milestone 6: in the named instance with the copied store, the first feed frame for the dashboard is reported (log or test harness) and is a small fraction of the previous size; suspend a session and see it leave the Sessions pane; open the resume dialog and see it listed; resume it and see it return.


## Idempotence and Recovery


Every step is a code change validated by tests and can be repeated. The only data touched in Milestone 6 is a copy of the store in a scratch directory used by a named instance; delete that directory to reset. Never run a development build against the default instance; development binaries refuse to.


## Artifacts and Notes


Measured on the large store before this change: 1,230 sessions (1,095 stopped, 67 running, 68 parked), 313 sub-agent relations. The removed all-workspace runtime snapshot was 9,077,500 bytes.


## Interfaces and Dependencies


In `mj-core/src/state.rs`:

    /// Sessions a terminal dashboard follows live: see the plan's Decision Log.
    pub fn live_session_ids(
        sessions: &SnapshotMap<String, SessionRecord>,
        subagents: &SnapshotMap<String, SubagentRecord>,
        operations: &BTreeSet<String>,
    ) -> BTreeSet<String>

In `mj-client/src/runtime_feed.rs`, add to `RuntimeMetadata`:

    #[serde(default)]
    pub launch_recency: Vec<LaunchRecency>,

    pub struct LaunchRecency {
        pub bundle_id: String,
        pub last_profile: String,
        pub target_template_id: String,
        pub newest_created_at: String,
    }

In `mj-client/src/daemon.rs`:

    DaemonAction::ResumeCandidates
    DaemonAction::GoStartupSession { workspace_id: String, last_session_id: Option<String> }
    DaemonReply::ResumeCandidates(Box<ResumeCandidates>)
    DaemonReply::GoStartupSession(Option<Box<SessionRecord>>)

    pub struct ResumeCandidates {
        pub records: Vec<SessionRecord>,
        pub moves: Vec<MoveOperation>,
        pub adopted_native_sessions: Vec<(HarnessKind, String)>,
        pub local_checkout_roots: Vec<PathBuf>,
    }

In `mj-tui` `DashboardState`:

    pub fn session_record(&self, session_id: &str) -> Option<&SessionRecord>
    pub fn apply_resume_candidates(&mut self, discovery_id: u64, candidates: ResumeCandidates)


Revision note (2026-10-01): initial plan written after three read-only surveys of feed consumers, the daemon and web paths, and on-demand query options.
