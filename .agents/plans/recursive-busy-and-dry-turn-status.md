# Recursive Subagent Busy Rollup and DRY Turn Status

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan is maintained in accordance with `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

Today, a parent session whose own turn has ended while a subagent (or a subagent's subagent) is still running is shown as idle or unread across Mjolnir—no spinner in the terminal dashboard, `attention: "unread"` or `"idle"` in the API and browser, and `is_idle: true` in `mj sessions`. Meanwhile, session busy status is still computed in four overlapping places across `mj-core`, `mj-client`, `mj-daemon`, and `mj-tui`, allowing the attention ladder, the daemon verdict, and the terminal spinner to disagree.

After this change, the daemon computes one recursive busy rollup across every session's own worker, its active reviews, and its entire descendant tree (managed `Worker`, `Oracle`, and `Reviewer` subagents plus native subagents, at any depth). That single daemon-owned verdict drives `SessionAttention::Working`, `SessionVerdict.working`, the terminal spinner, the web viewer activity label, and `mj sessions` output. Worker-local activity (`ActivityState` and `ApiSession.is_idle`, including `ActivityState::Expecting` for untracked background waits such as Claude's `Monitor` tool) remains honest about the worker's own process so `mj wait`, checkpoint barriers, and worker upgrades keep their exact semantics, while `mj sessions` reports both worker-local idle state and recursive subtree busy state (`is_idle`, `subtree_idle`, `working_subagents`, and human-readable text).

You can see the change working by starting or simulating a parent session whose own turn has ended while a direct child or grandchild subagent is running: the parent's `attention` becomes `working` (suppressing `idle`/`unread` while still letting `needs_input`, `important`, `failed`, and `unreachable` win), the terminal dashboard animates its spinner and shows the working subagent count instead of `Idle`, and `mj sessions` (both text and `--json`) reports both the idle parent worker and the busy descendant count.

## Progress

- [x] (2026-11-23 23:46Z) Mapped the GLiDE end-of-turn classifier, Claude `Monitor` background-task tracking, daemon publication order (`attention::update` vs `verdicts::project`), API/CLI wire types, and every duplicate busy predicate across the workspace.
- [x] (2026-11-23 23:59Z) Authored this ExecPlan after aligning on product decisions with the user (recursive busy → `SessionAttention::Working`, `mj sessions` reports both own and subtree state, keep `ActivityState::Expecting` for untracked waits like Claude's `Monitor`).
- [x] (2026-11-24 00:19Z) Built the 43-case / 73-row scrubbed real-log fixture corpus (`mj-daemon/tests/fixtures/turn-status-corpus.json`) and runner (`mj-daemon/src/daemon/turn_status_corpus_tests.rs`), ran baseline against `master` (`92e839c47`), and audited all 98 baseline disagreements and 3 edge-case conflicts before starting implementation.
- [x] (2026-11-24 00:31Z) Milestone 1: Centralized `ActivityState::is_computing`, `RelayOperationalState::{is_computing,has_running_turn_or_retry}`, refactored `SessionActivity::is_working`, and added `SessionVerdict::working_descendants`.
- [x] (2026-11-24 00:31Z) Milestone 2: Implemented single-owner recursive busy rollup in `verdicts.rs`, ordered `project_activity` → `attention::update` → `stamp_needs_person` in `feed.rs`, drove `SessionAttention::Working` from the shared verdict, and verified all 43 real-log corpus cases (730 assertions, 0 disagreements).
- [x] (2026-11-24 00:44Z) Milestone 3: Added `subtree_idle` and `working_subagents` to `ApiSession` and `mj wait` (`finish_wait`), added dual reporting to `mj sessions` (`listed_state` and `session_report_lines`), removed the three duplicate `review::any_working` checks in `mj-tui`, updated TUI and web session cards to display working subagents instead of `Idle`, and enabled all 876 assertions across all 43 real-log corpus cases (0 absent, 0 disagreements).
- [ ] Request review via `mj-review`, resolve or override any findings, and push to `origin/master`.

## Surprises & Discoveries

- Observation: Live session "Evaluating Mjolnir GitHub Issue" had `state: running`, `chat_phase: idle`, `is_idle: true`, `attention: unread`, and a failed GLiDE assessment (`request_failed`) while its child `test-windows-minastirith` was mid-turn (`activity_state: turn`, `attention: working`).
  Evidence: Inspected via `mj sessions --session` and `mj sessions --all --json` against the running daemon.
- Observation: Claude's `Monitor` tool does not increment `ActivityFacts.background_commands`. Its wakes arrive as AIR `async_task_spawned` with `taskType == "turn"`, which maps to `AirTaskState::Turn` in `mj-worker/src/acp/air_tasks.rs` and `mj-worker/src/relay/background.rs` and is excluded from `background_commands`.
  Evidence: Code audit of `mj-worker/src/acp/air_tasks.rs:28-94` and `mj-worker/src/relay/background.rs:83-104,240-291,412-427`.
- Observation: `mj-daemon/src/daemon/verdicts.rs` already walks descendants recursively for `interrupt_targets` (`verdicts.rs:34-60`), whereas `working_children` and `unfinished_children` (`verdicts.rs:555-576`) only inspect direct children via `own.get(child)` and filter `working_children` to `SubagentKind::Worker`, omitting `SubagentKind::Oracle` even though `mj-tui` and `viewer.js` list `Oracle` under `Subagents`.
  Evidence: `mj-daemon/src/daemon/verdicts.rs:555-579`, `mj-tui/src/row_index.rs:112-113`, `mj-server/src/server/viewer_types.rs:296-304`.
- Observation: `subagent_waiting_for_background` was added to `verdicts.rs:312` (commit `bf7e3a4d9`) but never reflected in `attention.rs`, so a held subagent had `verdict.working = true` while `SessionAttention` reported `Idle`.
  Evidence: `mj-daemon/src/daemon/verdicts.rs:308-312` vs `mj-daemon/src/daemon/attention.rs:620-644`.
- Observation: `ApiSession` in `mj-server/src/server/api/types.rs:116` does not carry `#[serde(deny_unknown_fields)]`, whereas `ViewerSession` in `mj-server/src/server/viewer_types.rs:647` does. `SessionVerdict` in `mj-client/src/runtime_feed.rs:49` uses `#[serde(default)]` without `deny_unknown_fields`, so adding `working_descendants` to `SessionVerdict` adds no field to `ViewerSession`.
  Evidence: Direct inspection of `mj-server/src/server/api/types.rs:115-198`, `mj-server/src/server/viewer_types.rs:646-851`, and `mj-client/src/runtime_feed.rs:48-75`.

## Decision Log

- Decision: Keep `ActivityState::Expecting` for untracked background waits (such as Claude's `Monitor` tool), with `is_idle() == false`, `has_work_in_flight() == false`, and `is_working() == false`.
  Rationale: Claude's `Monitor` does not appear in `background_commands`, so `Expecting` remains useful for marking those waits non-idle without blocking worker replacement.
  Date/Author: 2026-11-23 / User + Agent.
- Decision: Roll up recursive busy descendants (across all `SubagentKind` values—`Worker`, `Oracle`, `Reviewer`—and running native subagents, traversing through `Parked` intermediate children) into both `SessionVerdict.working` and `SessionAttention::Working`.
  Rationale: Any parent whose descendant is doing work is deterministically busy; `SessionAttention::Working` suppresses `Idle`/`Unread` while preserving higher-priority `NeedsInput`, `Important`, `Failed`, and `Unreachable`.
  Date/Author: 2026-11-23 / User + Agent.
- Decision: Keep `ApiSession.is_idle` (and `mj wait`) strictly worker-local, and add `subtree_idle: bool` and `working_subagents: usize` to `ApiSession` (plus human-readable text in `mj sessions` and `mj sessions --session`) so callers see both worker-local and subtree state.
  Rationale: Automation such as `mj wait` and `put-file` depends on knowing whether the session's own worker can accept prompts or file writes, while humans and coordinators running `mj sessions` need to see both whether the parent worker is idle and whether its subagents are still working.
  Date/Author: 2026-11-23 / User + Agent.
- Decision: Order daemon projection as `verdicts::project_activity` → `attention::update` → `verdicts::stamp_needs_person` in `Daemon::capture_runtime`, breaking the apparent dependency between attention and verdicts cleanly with zero cycles.
  Rationale: `own_verdict` only reads `session_attention` for the single boolean `needs_person`. Computing all activity, review, and recursive descendant busy fields first lets `attention::update` read one authoritative busy state instead of re-deriving `Working` from raw fields.
  Date/Author: 2026-11-23 / Agent.
- Decision: Include `review::any_working` in daemon `verdict.working`, remove the three duplicate `|| mj_client::review::any_working(reviews)` checks in `mj-tui`, and let an active working review yield `SessionAttention::Working` even on a `Stopped` session (before the `!is_active()` early return in `attention.rs`).
  Rationale: Eliminates the split where `attention.rs` and `mj-tui` checked reviews independently from `verdicts.rs`, and resolves baseline conflict #1 (`review-running-stopped-parent`) consistently with `mj-tui/src/render/sessions.rs:249-252`.
  Date/Author: 2026-11-23 / Agent.
- Decision: Include `launching` (`matches!(transition, Some(Starting | Resuming | Moving)) || state == SessionState::Checkpointing`) in a session's own `working` and `unfinished` (and include `subagent_waiting_for_background` in `own_unfinished`), while excluding teardown transitions (`Suspending`, `Stopping`, `Destroying`).
  Rationale: Resolves baseline conflicts #2 and #3 (`child-provisioning`, `reviewer-child-provisioning`, `held-child-background-process`): eliminates the contradiction where a provisioning child had `attention = Working` but `verdict.working = false` and a held/launching child had `working_children = 1` but `unfinished_children = 0`. `SessionRowForm::Transition` in `mj-tui/src/render/sessions.rs:317-348` does not take a spinner, so transition goldens remain unchanged.
  Date/Author: 2026-11-23 / Agent.

## Outcomes & Retrospective

All three milestones are implemented and validated against the 43-case / 73-row real-log-derived corpus (`mj-daemon/tests/fixtures/turn-status-corpus.json`, 876/876 assertions passing, 0 absent, 0 disagreements) as well as targeted crate suites and 7-package `mbx clippy --all-targets -- -D warnings`:

- `brokk-mj-core` `activity`: 32 passed
- `brokk-mj-client` `usage_format`: 12 passed
- `brokk-mj-daemon`: `turn_status_corpus` 1/1 (43 cases, 876 checks), `verdicts::tests` 12/12, `attention::tests` 7/7, `feed::tests` 17/17
- `brokk-mj-server`: 310/310 (including embedded `viewer.js` tests and `mj wait` regression)
- `brokk-mjolnir` (`mj-cli`): `api_commands` 41/41
- `brokk-mj-tui`: `golden` 36/36, `render::tests` 75/75 (run under `TZ=UTC`)
- `brokk-mj-chat`: 419/419 (2 ignored, run under `TZ=UTC`)

Building the scrubbed real-log corpus before modifying production code immediately surfaced three genuine edge-case conflicts (`review-running-stopped-parent`, `child-provisioning` own `verdict.working`, and `held-child-background-process` / provisioning `unfinished_children`), allowing each to be resolved explicitly in one shared owner instead of patching individual consumers. Remaining limitation: one pre-existing `mj-tui` test (`component_events::tests::clicking_a_profile_row_below_the_heading_selects_that_profile`) fails on unmodified `HEAD` as well because provider groups start collapsed; it is unrelated to turn status.

## Context and Orientation

Mjolnir separates its control plane (the daemon in `mj-daemon`, backed by `mj-controller` and `mj-core`) from its data plane (per-session worker processes in `mj-worker`). Each worker publishes its own operational snapshot (`mj_core::relay::RelayOperationalState` in `mj-core/src/relay/snapshot.rs`), including `activity: Option<ActivityState>` classified by `mj_core::activity::classify(&ActivityFacts)` in `mj-core/src/activity.rs`.

The daemon polls state into a `RuntimeProjection` (`mj-client/src/runtime_feed.rs`) inside `Daemon::capture_runtime` (`mj-daemon/src/daemon/feed.rs`). Two daemon projection modules turn raw snapshots into person-facing state:

- `mj-daemon/src/daemon/attention.rs` computes `SessionAttention` (`NeedsInput`, `Important`, `Unread`, `Failed`, `Unreachable`, `Working`, `Idle`, `Inactive`, `Unknown`) and notification cards.
- `mj-daemon/src/daemon/verdicts.rs` computes `SessionVerdict` (`idle`, `working`, `active_work`, `unfinished`, `working_children`, `unfinished_children`, `needs_person`, interrupt targets, and lifecycle action offers).

Downstream consumers read these daemon projections:

- `mj-daemon/src/server_runtime/snapshot.rs` builds `ViewerSession` (`mj-server/src/server/viewer_types.rs`), which feeds both the web viewer (`mj-server/src/web/viewer.js`) and the HTTP v1 API's `ApiSession` (`mj-server/src/server/api/types.rs`).
- `mj-cli/src/api_commands.rs` renders `ApiSession` for `mj sessions` (`listed_state`) and `mj sessions --session <id>` (`session_report_lines`).
- `mj-tui` reads `RuntimeProjection.verdicts` and `session_attention` via `mj-tui/src/dashboard_sessions.rs`, rendering session rows in `mj-tui/src/render/sessions.rs`, tracking animation dirty state in `mj-tui/src/render_changes.rs`, and polling animation ticks in `mj-tui/src/resume.rs`.
- `mj-client/src/usage_format.rs` defines `SessionActivity` for formatting activity clocks and labels, and currently hosts a second `is_working` predicate called by both `mj-daemon/src/daemon/verdicts.rs` and `mj-chat/src/chat/status.rs`.

A "managed subagent" is a Mjolnir child session recorded in `RuntimeProjection.subagents` with a `parent_session_id` and `SubagentKind` (`Worker`, `Reviewer`, or `Oracle` in `mj-core/src/subagent.rs`). A "native subagent" is an in-harness child recorded in `RuntimeProjection.native_agents` with an `owner_session_id` (and optional `parent_session_id`). A session tree can nest to arbitrary depth (for example, a top-level parent, a depth-1 worker/supervisor, and a depth-2 worker or reviewer).

## Plan of Work

### Milestone 1: Centralize Worker-Computing Predicate in `mj-core` and `mj-client`

In `mj-core/src/activity.rs`, add `ActivityState::is_computing(&self, waiting_for_input: bool, has_background: bool) -> bool` as the single worker-local computation predicate: `Turn` and `Tool` return `!waiting_for_input || has_background`; `Background` and `Closing` return `true`; `Unknown { last_known, .. }` recurses on `last_known`; all other variants (`Expecting`, `Idle`, `Goal`, `CheckingQuota`, `Retry`, `Closed`, `Unrecognized`) return `false`. Implement `ActivityState::is_working(&self)` as `self.is_computing(false, false)`.

In `mj-core/src/relay/snapshot.rs`, add two methods on `RelayOperationalState` beside `activity_state()`:

- `is_computing(&self, waiting_for_input: bool) -> bool`: delegates to `self.activity_state().is_computing(waiting_for_input, !self.background_commands.is_empty() || !self.active_user_shells.is_empty())`.
- `has_running_turn_or_retry(&self) -> bool`: returns true when `self.active_prompt.is_some()`, `self.harness_turn.is_some()`, `self.goal.running()`, an unsubmitted `capacity_retry` is armed, or an unsubmitted `continuation.quota_recovery` is armed. This replaces the raw field probing in `mj-daemon/src/daemon/verdicts.rs`.

In `mj-client/src/usage_format.rs`, refactor `SessionActivity::is_working(&self, current_turn_started_at: Option<u64>, waiting_for_input: bool) -> bool` to delegate to `self.state().is_computing(waiting_for_input, has_background)` (handling the one extra materialized case where `current_turn_started_at.is_some()` while the state is not `Closing`/`Closed` by treating it as a foreground turn).

In `mj-client/src/runtime_feed.rs`, add `#[serde(default)] pub working_descendants: usize` to `SessionVerdict` to carry the total recursive count of working descendants across all subagent kinds and native agents.

### Milestone 2: Single-Owner Recursive Busy Rollup and `attention` / `verdicts` Unification

In `mj-daemon/src/daemon/verdicts.rs`:

1. Update `is_active_member` to include `SessionState::Parked` (`(record.state.has_live_worker() || record.state == SessionState::Parked) && !record.archived`) so `children()` traverses through a parked intermediate child to reach live grandchildren. (A parked node itself still has `own_working = false`, `own_unfinished = false`, and `interrupt_member = false`.)
2. Replace `mj_client::usage_format::SessionActivity::of(live).is_working(None, form)` in `own_verdict` with `live.is_computing(form)`, and replace the raw operational field checks in `running` with `live.has_running_turn_or_retry() || transcript_active_turn`.
3. Compute `review_working` per session from `runtime.metadata.reviews` using `mj_client::review::any_working(...)` and include `previous.metadata.reviews != runtime.metadata.reviews` in the invalidation check so review phase transitions always recompute verdicts and ancestor families.
4. Define a session's own working and unfinished state cleanly:
   - `held = state == Running && connected && subagent_waiting_for_background(runtime, committed, id, live)`
   - `launching = matches!(transition, Some(Starting | Resuming | Moving)) || state == SessionState::Checkpointing`
   - `own_working = (state == Running && connected && live.is_computing(form)) || held || launching || review_working`
   - `own_unfinished = unfinished_turn || active_work || held || launching`
   Define a recursive helper (memoized or cycle-guarded DFS over `children`) that computes for every node in `families`:
   - `working_descendants`: total count of distinct working descendants across the entire subtree (managed `Worker`, `Oracle`, and `Reviewer` sessions whose `own_working` is true, plus running native agents deduped by `(owner_session_id, stable_id_or_session_id)`).
   - `subtree_working(node)`: `own_working(node) || working_descendants(node) > 0`.
   - `working_children`: count of direct non-reviewer managed children (`SubagentKind::Worker | SubagentKind::Oracle`) with `subtree_working(child) == true`, plus direct running native agents (deduped by `stable_id_or_session_id`). This keeps the `Subagents · N` badge accurate for both depth-2 grandchildren and `Oracle` children without counting `Reviewer`s in the `Subagents` button.
   - `unfinished_children`: count of direct managed children that are unfinished recursively (`own_unfinished(child) || unfinished_descendants(child) > 0`), plus direct running native agents.
   - Aggregate `verdict.working = own_working || working_descendants > 0`.
5. Split `project` into:
   - `pub(crate) fn project_activity(runtime, previous, committed, previous_committed)`: computes all fields of `SessionVerdict` except `needs_person`, preserving existing `needs_person` values from `previous.verdicts` only until `stamp_needs_person` runs (or defaulting to `false` before `stamp_needs_person`).
   - `pub(crate) fn stamp_needs_person(runtime, previous)`: updates `verdict.needs_person` for any session whose `session_attention` or verdict changed.
   - Keep `pub(crate) fn project(...)` calling `project_activity` then `stamp_needs_person` for tests and the fixture path in `mj-daemon/src/server_runtime/snapshot.rs:474`.

In `mj-daemon/src/daemon/feed.rs` (`Daemon::capture_runtime`):

1. Call `verdicts::project_activity(&mut full, &history.full, committed.as_deref(), history.committed.as_deref())` before `attention::update`.
2. Call `attention::update(&history.attention_source, &mut full, committed.as_deref())`.
3. Copy `session_attention` onto `full.sessions` as today.
4. Call `verdicts::stamp_needs_person(&mut full, &history.full)`.
5. Copy the pre-`needs_person` verdicts (or full verdicts) onto `source.verdicts` before saving `history.attention_source = source`, so `attention::update` can compare `previous.verdicts` vs `next.verdicts` on the next poll without seeing `needs_person` churn.

In `mj-daemon/src/daemon/attention.rs`:

1. In `update()`: include ids whose verdict busy state (`working`, `working_descendants`) changed between `previous.verdicts` and `next.verdicts` in `ids` (which then expands up ancestors via the existing loop). Ignore `needs_person` when comparing verdicts so `stamp_needs_person` never re-dirties attention.
2. In `project_states()`: read `let verdict_working = runtime.verdicts.get(id).is_some_and(|v| v.working);`.
   - Before `else if !record.state.is_active() { Inactive }`: if an active working review exists (`mj_client::review::any_working` for this session's reviews), return `Working` so review work stays visible even when the session's primary worker is `Stopped`.
   - When `record.state == SessionState::Parked` and `!questions.contains(id)`: return `Working` if `verdict_working`, else `Idle`.
   - In the `Some(live)` branch when `live.activity_state().is_idle()`: if `verdict_working` is true, return `Working` (suppressing `Idle` and `Unread`, while `NeedsInput`/`Important` from `notification_card` still take precedence at lines 721–725 and pending forms/failed/unreachable already won earlier).
3. In the `#[cfg(test)] fn project()` helper (and `project_states` test wrapper): ensure `verdicts::project_activity` runs when `runtime.verdicts` has not been populated by the caller, so unit tests calling `attention::project(&runtime)` exercise the exact same single-owner rule.

### Milestone 3: `mj sessions` Dual Reporting, TUI DRY Cleanup, and UI Subagent Labels

In `mj-server/src/server/api/types.rs` (`ApiSession`):

1. Keep `is_idle: bool` strictly worker-local (`session.is_idle`, which comes from `verdict.idle` in snapshots and worker `activity_state().is_idle()` in `mj wait`).
2. Add two fields with `#[serde(default)]`:
   - `pub subtree_idle: bool` — set in `From<&ViewerSession>` as `session.is_idle && !session.verdict.working`.
   - `pub working_subagents: usize` — set in `From<&ViewerSession>` as `session.verdict.working_descendants`.
3. In `mj-server/src/server/api/wait.rs` (`finish_wait`): keep `session.is_idle` strictly worker-local as today, and recompute `session.subtree_idle = session.is_idle && !viewer_session.verdict.working` (so `subtree_idle` agrees with the refreshed `is_idle`).

In `mj-cli/src/api_commands.rs`:

1. In `listed_state(session)`: when `session.working_subagents > 0` and no higher-priority attention label (`NeedsInput`, `Important`, `Failed`, `Unreachable`) is shown, append both own state and subagent state: ` (idle, N subagent[s] working)` when `session.is_idle` is true, or ` (working, N subagent[s] working)` when `session.is_idle` is false. When `session.working_subagents == 0`, keep the existing output unchanged so all existing tests and goldens remain intact.
2. In `session_report_lines(session, now_ms)`: when `session.working_subagents > 0`, add a clear status line such as `subagents: 1 working (session worker is idle)` or `subagents: 2 working (session worker is busy)`.

In `mj-tui`:

1. Remove the three duplicate `|| mj_client::review::any_working(reviews)` predicates in `mj-tui/src/render/sessions.rs:252`, `mj-tui/src/render_changes.rs:378`, and `mj-tui/src/resume.rs:1264`. All three read `dashboard.session_is_working(&session.id)` (which reads `verdict.working`). Update any TUI unit tests that construct reviews without verdicts so they set `verdict.working = true` (matching daemon publication).
2. In `mj-tui/src/render/sessions.rs` (`session_activity_line`): pass `working_descendants = dashboard.verdict(&session.id).working_descendants`. When the session is active, has no higher-priority status (operation, error, parked, unreachable, checking quota, question, review, expecting), and its own worker is idle (`detail.is_none_or(|d| d.activity.is_idle(d.current_turn_started_at))`) while `working_descendants > 0`, display `format!("{}", crate::widgets::counted(working_descendants, "subagent", "subagents"))` (for example `1 subagent`, `2 subagents`) instead of `Idle 2m`.

In `mj-server/src/web/viewer.js`:

1. In `sessionActivityLabel`: when own activity is idle (`kind === 'idle'`, or fallback when no active turn/step/background) and `(session.verdict?.working_descendants ?? 0) > 0`, return `${n} subagent${n === 1 ? '' : 's'} working` instead of `Idle …`.

## Concrete Steps

All commands run from `/home/jonathan/Projects/mjolnir/.mj/clones/442c57c6ac7bc6b171dfe34724a318a8`. Remember to run `cargo test` outside the sandbox (`BypassSandbox: true`) per `AGENTS.md`.

1. Implement Milestone 1 (`mj-core`, `mj-client`) and run targeted tests + clippy on `mj-core` and `mj-client`:

       cargo test -p mj-core activity
       cargo test -p mj-client usage_format
       cargo clippy -p mj-core -p mj-client --all-targets -- -D warnings

2. Implement Milestone 2 (`mj-daemon`) and run targeted tests + clippy on `mj-daemon`:

       cargo test -p mj-daemon verdicts::tests
       cargo test -p mj-daemon attention::tests
       cargo test -p mj-daemon feed::tests
       cargo clippy -p mj-daemon --all-targets -- -D warnings

3. Implement Milestone 3 (`mj-server`, `mj-cli`, `mj-tui`, `mj-chat`) and run targeted tests + goldens + clippy:

       cargo test -p mj-server
       cargo test -p mj-cli api_commands
       cargo test -p mj-tui golden
       cargo test -p mj-tui render::tests
       cargo test -p mj-chat
       cargo clippy -p mj-server -p mj-cli -p mj-tui -p mj-chat --all-targets -- -D warnings

4. Verify web viewer JS tests if Playwright/node tests apply to `viewer.js` (`mj-server` embedded JS tests already run inside `cargo test -p mj-server`).

5. Commit changes on `master`, run `request_review`, address or override any review findings, and push to `origin/master`.

## Validation and Acceptance

Acceptance is verified by the following behaviors and new tests (tricky-condition and integration tests per `AGENTS.md`):

1. **Direct busy child marks idle parent busy**: A `Running` parent with `ActivityState::Idle` and an unread transcript frontier has a `Running` managed `Worker` child in `ActivityState::Turn`. Verify `verdict.working == true`, `verdict.idle == true`, `verdict.working_children == 1`, `verdict.working_descendants == 1`, and `session_attention["parent"] == SessionAttention::Working` (not `Unread` or `Idle`). When the child finishes and becomes `Idle`/`Parked`, verify `verdict.working == false`, `working_descendants == 0`, and `session_attention["parent"]` returns to `Unread` (or `Idle`).
2. **Recursive grandchild busy (depth 2+) and cycle termination**: Parent → idle/parked child → working grandchild (`Worker`, `Oracle`, `Reviewer`, or native subagent). Verify the parent has `verdict.working == true`, `working_children == 1` (for non-reviewer direct child), `working_descendants >= 1`, and `session_attention["parent"] == SessionAttention::Working`, and that a synthetic parent cycle terminates cleanly.
3. **Launching child and held child**: A `Provisioning` child or a child held by `subagent_waiting_for_background` marks both itself and its parent `Working` in both `verdict.working` and `session_attention`, with no flicker and no disagreement between `verdicts` and `attention`.
4. **Priority preservation**: When an idle parent has a busy child AND a pending elicitation or notification-stack `?`/`!` or `Failed`/`Unreachable` state, the higher-priority attention (`NeedsInput`, `Important`, `Failed`, `Unreachable`) still wins over `Working`.
5. **Dual reporting in `mj sessions` and worker-local `mj wait`**: Verify `ApiSession` for an idle parent with a working child serializes `is_idle: true`, `subtree_idle: false`, `working_subagents: 1`, `attention: "working"`, and `listed_state` / `session_report_lines` report both; verify `mj wait` still settles on the worker's own `ActivityState::Idle` even while a child is busy.
6. **TUI and web rendering**: Verify a TUI row for an idle parent with `working_descendants == 1` animates its spinner, shows the working glyph, and displays `1 subagent` instead of `Idle`, and that TUI golden tests pass under `TZ=UTC`. Verify `viewer.js` `sessionActivityLabel` displays `1 subagent working` instead of `Idle`.
7. **Expecting preserved**: Verify `ActivityState::Expecting` continues to report `is_idle() == false`, `is_working() == false`, `has_work_in_flight() == false`, and `SessionAttention::Working`.

## Idempotence and Recovery

All code changes are additive and backward-compatible on the wire (`SessionVerdict` and `ApiSession` new fields use `#[serde(default)]`; `ViewerSession` schema is unchanged; no database migration is required). Any milestone can be re-run or rolled back cleanly with standard Git commands on the current checkout. No live daemon or default instance state is modified during testing.

## Artifacts and Notes

- Reference investigation reports from this session are stored under `.mj/agents/1fcc7e240caf86c585b1eee677d6d089/audit.md` and `.mj/agents/743b3afdf91b912179dcff2160b631dc/busy-rollup-map.md`.

## Interfaces and Dependencies

In `mj-core/src/activity.rs`:

    impl ActivityState {
        #[must_use]
        pub fn is_computing(&self, waiting_for_input: bool, has_background: bool) -> bool;
    }

In `mj-core/src/relay/snapshot.rs`:

    impl RelayOperationalState {
        #[must_use]
        pub fn is_computing(&self, waiting_for_input: bool) -> bool;

        #[must_use]
        pub fn has_running_turn_or_retry(&self) -> bool;
    }

In `mj-client/src/runtime_feed.rs`:

    pub struct SessionVerdict {
        // ... existing fields ...
        #[serde(default)]
        pub working_descendants: usize,
    }

In `mj-daemon/src/daemon/verdicts.rs`:

    pub(crate) fn project_activity(
        runtime: &mut RuntimeProjection,
        previous: &RuntimeProjection,
        committed: Option<&mj_controller::database::CommittedState>,
        previous_committed: Option<&mj_controller::database::CommittedState>,
    );

    pub(crate) fn stamp_needs_person(
        runtime: &mut RuntimeProjection,
        previous: &RuntimeProjection,
    );

In `mj-server/src/server/api/types.rs`:

    pub struct ApiSession {
        // ... existing fields ...
        #[serde(default)]
        pub subtree_idle: bool,
        #[serde(default)]
        pub working_subagents: usize,
    }
