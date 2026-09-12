# Native subagent API and thin CLI (issue #986, core requirements)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

An orchestrating agent (for example a Claude Code session) should be able to run an mj session as a subagent: start a session with a prompt in one call and get its id back, block until the turn ends and read a structured outcome with the final assistant message, send the next prompt, page through the transcript live or after the session stopped, and get the work out as a diff, a file, a pushed branch, or the checkpoint's git bundle, all without instructing the model to do any of it. Today that only works through the web viewer's undocumented HTTP API plus polling and scraping.

After this change, with the daemon running, this works end to end:

    mj new --profile codex --target local --project-directory . --prompt "add a README line"
    mj wait --session <id>            # prints outcome, turn number, final message
    mj prompt --session <id> --wait "now add a test"
    mj transcript --session <id> --json
    mj diff --session <id>
    mj export --session <id> --kind bundle --out work.bundle
    mj close --session <id>

and the same operations are documented HTTP routes under `/api/v1/` with bearer-token auth, a version header, and idempotency keys on creation.

Out of scope: the issue's nice-to-haves (an `asked_question` outcome, usage and cost, a typed event stream) and the follow-up comment's extras (target facts, per-session env vars, run ids, workspace CRUD).

## Progress

- [x] (2026-09-11) Design fixed and ExecPlan written.
- [x] (2026-09-11) M1: persist the per-turn outcome in the projection and database; API module with auth, version header, sessions list/get, prompt, wait, close, cancel-turn; daemon-side backend.
- [x] (2026-09-11) M2: start with model/effort/prompt follow-up and durable idempotency keys.
- [x] (2026-09-11) M3: transcript paged by seq from SQLite.
- [x] (2026-09-11) M4: export: worker subcommands, diff, file fetch, branch push, bundle.
- [x] (2026-09-11) M5: CLI subcommands, the API client, and the API reference page.

## Surprises & Discoveries

- Observation: the projection drops the prompt stop reason. `src/hel_projection.rs` (CommandCompleted arm, around line 549) only sets execution to Idle.
  Evidence: `RelayCommandOutcome::Prompt { .. } => { close_streams(...); mutation.execution = Some(Idle); }`.
- Observation: `SessionLaunchOptions.initial_prompt` is never submitted; it becomes `draft_input` (`mj-controller/src/hel_controller.rs` around line 811).
- Observation: the worker re-arms capacity retries indefinitely until a cancel or close (`src/hel_worker/snapshot.rs` around 1437-1454; backoff in `src/hel_worker/capacity.rs` 29-37). A retry is a new prompt command `capacity-retry-<ordinal>` with its own accepted ordinal.
- Observation: with queued prompts, "idle and newest turn start >= my ordinal" can return an earlier prompt's outcome (prompt A accepted at 10, B at 12, A starts at 13). Persisting the accepted ordinal on queue entries and outcomes fixes it.
- Observation: `backend_locator` and `workspace_root` are `pub(super)` inside `mj-controller/src/hel_controller/`, so the daemon-side backend cannot derive the target locator or repository paths without a new public helper.
- Observation: `create_managed_worktree` (`mj-controller/src/hel_controller/worktree.rs` around 970) records no base commit; the branch is created from HEAD.
- Observation: there is no separate "fresh schema" creation path in `src/hel_database/schema.rs`. A new database starts at `user_version = 0` and runs the whole migration ladder, so migration 28 alone is enough for both new and existing stores.
  Evidence: the tables migration 28 alters are created by the `version < 6` block and `ensure_relay_projection_schema`, both of which run first.
- Observation: several existing migration tests rewind `user_version` without dropping the artifacts of later migrations (for example `migration_twenty_five_adds_pane_sizes_without_losing_workspaces` sets `PRAGMA user_version = 24` but leaves every later table in place). A plain `ALTER TABLE ... ADD COLUMN` in migration 28 therefore failed those tests with "duplicate column name".
  Evidence: `migration_twenty_five_adds_pane_sizes_without_losing_workspaces`, `migration_twenty_one_drops_the_workspace_review_settings`, `version_fourteen_database_gains_empty_host_container_sizes` and three others failed until migration 28 was made structurally guarded.
- Observation: `muse_migration_preserves_existing_sessions_hidden_entries_and_indexes` deleted only ledger row 27 while rewinding to 26, so migration 28's ledger insert collided. The test was corrected to `DELETE FROM schema_migrations WHERE version >= 27`, which is what rewinding to 26 means.
- Observation: `replacement_session_test_fixture` in `mj-controller/src/hel_session_manager.rs` is `#[cfg(test)]` and private, despite a comment claiming it is compiled unconditionally for other crates. It is not usable from `mj-cli` tests.
  Evidence: `grep -rn replacement_session_test_fixture` finds uses only inside `hel_session_manager.rs`.
- Observation: the sample session in `hel_server`'s test config carries a recorded error, so every wait resolved to `error` until the API test factory cleared `has_error`.
- Observation: `validate_action`'s prompt arm reads the session record, so session creation — which carries a first prompt before any record exists — could not reuse it. The text rules are now `validate_prompt_text` in `hel_server.rs`, called from both.
- Observation: `create_bundle` was a handler with its validation inline, so "create the quick bundle exactly as the viewer does" meant factoring `create_quick_bundle(state, source)` out of it rather than restating 1024-character and empty-source checks in the API module.
- Observation: the follow-up task can finish before `start_followup` returns, so the `Pending` entry has to be inserted before the task is spawned; inserting it afterwards lost the `Submitted { turn_id }` a fast fake session had already recorded.

- Observation: the transcript sequence (`COALESCE(latest_content_event_ordinal, position)`) was already computed ad hoc in `mj-chat`'s `item_update_ordinal`, so M3 put it on `TranscriptItem::seq()` and the API reads that rather than recomputing the rule at the wire boundary.
- Observation: `TranscriptPage` was declared in `hel_server/api.rs` in M1 but the loader that fills it belongs in `hel_database`, so M3 moved the type there and the API module re-exports it, the way it already re-exports `TurnSummary`.

- Observation: a closed `watch` channel reports "changed" immediately and forever, so the first wait loop spun without ever letting a timer fire. The test hung rather than failing.
  Evidence: `pstree` showed the test binary alive with one busy thread and no progress; `tokio::select!` was taking the `snapshot_rx.changed()` branch on every pass because the test factory dropped the sender.
  Resolution: the loop now returns 503 when the snapshot channel closes, which is the honest answer when the control loop that publishes session facts is gone.

- Observation: `plan_workspace_to_raw` can reattach a branch a previous move retired (`reuse_existing_branch`), so its base commit is the branch's original creation point, not the repository's current HEAD. Only a freshly created branch records a base; a reattached one keeps `None` and falls back to the reflog.
- Observation: `verify_repository_bundles_streaming` returned the repository bundles but not the archive manifest's `bundle.primary_repository`, so a bundle export of a stopped session had no way to name the primary repository. The type is used in exactly one place, so it gained that field rather than the caller re-reading the archive.
- Observation: `session_export_layout` cannot serve a stopped session: it derives the target locator, and a stopped record has none. The bundle export therefore reads the primary repository id from the archive instead of the layout, which is also what makes it work after the target is gone.
- Observation: two workspace test failures during validation (`node_preflight_checks_missing_old_and_supported_tools_on_profile_path`, `npm_upgrade_restarts_after_the_running_package_is_removed`, the latter failing to spawn a child process at all) were load flakes: both passed on their own and in a second full run.

- Observation: the exit code was the only thing a worker refusal carried across the process boundary, so `PushBranchError::NoRemote` and `session_diff`'s "no session base recorded" both reached the API as 500. M5 gave the three export subcommands exit code 3 for typed refusals; `hel_server/api.rs` already tested `Refused` -> 409, so only the worker and `worker_command` changed.
- Observation: `mj-cli` had no `axum` dependency, so the API client test that answers itself from a canned router needed one as a dev-dependency.
- Observation: clippy's `dead_code` is judged on the non-test build, so the client's `session()` and `read_file()` could not be justified by their tests alone. They gained real callers: `mj sessions --session <id>` and `mj export --kind file --path <path>`.
- Observation: the build disk (`/mnt/optane`, which holds the symlinked `target/` and the shared build cache) was 100% full during validation. Validation ran with `CARGO_TARGET_DIR` pointed at a gitignored `target-local/` on the roomy filesystem; nothing was redirected to `/tmp` and the shared cache was left alone.

- Observation: the wait loop's launch-failure rule was dead code. `build_observation` compared `ViewerLaunchFailure.id` to the session id, but `record_launch_failure` builds that id as `format!("{}-{action_id}", std::process::id())` -- a notice id for the browser's dismiss button, never a session id.
  Evidence: `mj-cli/src/server.rs` `record_launch_failure`; `mj-controller/src/web/viewer.js` `renderLaunchFailures` uses `failure.id` only to dismiss.
  Resolution: `ViewerLaunchFailure` carries an optional `session_id`, populated from the session already in scope at the completion arm, and the rule matches on that.
- Observation: `pending_action_errors` in `mj-cli/src/server.rs` was inserted into and never removed from, so one failed action badged a session as errored for the daemon's whole lifetime, through every later durable reload.
  Evidence: the only mention of the map besides the two overlay loops was the `insert` in the action-completion arm.
  Resolution: `record_action_result` inserts on failure and removes on success, so a later successful action for the same session clears the overlay.
- Observation: because `wait` mapped that session-scoped `has_error` to `WaitOutcome::Error`, a single earlier failure -- including a transient relay hiccup -- made every later wait on the session return `error` immediately while the turn ran on fine. The API test factory had been clearing `has_error` on its sample session to work around exactly this.
  Evidence: `resolve_wait`'s third rule; the `snapshot.sessions[0].has_error = false` line in `api_app`.
  Resolution: the rule is gone and the workaround with it, so every wait test now runs against a session that is carrying an error.
- Observation: a wait on a session whose relay is unreachable blocked to its deadline and answered `timeout` with no reason, which is indistinguishable from a slow turn.
  Resolution: `WaitResponse.relay` reports the live view's health on every response; it changes no outcome.

## Decision Log

- Decision: expose the API as `/api/v1/` routes on the existing axum web viewer server rather than a new listener or the daemon's frame protocol.
  Rationale: the orchestrating agent already reaches this server with curl; TLS and Tailscale handling exist; the CLI stays a thin HTTP client as the issue asks.
  Date/Author: 2026-09-11, Fable.
- Decision: same-user auth is a bearer token read from a persistent 0600 file at `<data dir>/api-token`, also accepting the existing viewer cookie. No unix socket.
  Rationale: one listener, curl-friendly, survives daemon restarts; the cookie key already uses the same file pattern.
  Date/Author: 2026-09-11, Fable.
- Decision: `turn_id` is the relay acceptance ordinal returned by `SessionHandle::submit`, and the projection persists that ordinal on queue entries, the active turn, and the last outcome.
  Rationale: it is the only identity the caller has at prompt time and it makes wait correct under queued prompts and capacity retries.
  Date/Author: 2026-09-11, Fable.
- Decision: wait keeps waiting while a capacity retry is pending and reports `quota_limit` only when the newest outcome is `ModelCapacity` with no retry pending; on timeout it reports the pending retry.
  Rationale: the worker never gives up on its own; returning `quota_limit` while a retry is armed would make the caller's next prompt collide with the retry.
  Date/Author: 2026-09-11, Fable.
- Decision: `mj-controller` defines a `SubagentBackend` trait; `mj-cli` implements it over the daemon runtime.
  Rationale: `mj-controller` cannot depend on the daemon `RuntimeState` in `mj-cli`; the existing web server already uses channels for the same reason.
  Date/Author: 2026-09-11, Fable.
- Decision: API errors carry dynamic messages (`ApiFailure { status, message: String }`), unlike the phone surface's fixed messages.
  Rationale: the caller is the same user, and the value of the API is in knowing why a turn or export failed.
  Date/Author: 2026-09-11, Fable.
- Decision: export git operations run on the target through new `hel worker diff`, `read-file`, and `push-branch` subcommands rather than through the session's shell.
  Rationale: the session shell inserts transcript items the model sees; the worker binary is already installed on every target kind.
  Date/Author: 2026-09-11, Fable.
- Decision: managed worktrees record `base_commit` at creation, with the branch reflog as the fallback for sessions created earlier.
  Rationale: reflogs expire and remote worktrees would need a second git call; a durable field is one read.
  Date/Author: 2026-09-11, Fable.
- Decision: "a CLI generated from the same definitions" means the CLI serializes the request and response structs exported from `mj_controller::hel_server::api`; there is no code generator.
  Rationale: one source of truth for the wire shapes without adding build tooling.
  Date/Author: 2026-09-11, Fable.
- Decision: the checkpoint archive's canonical session state does not carry the new turn fields; a resumed session starts with no last outcome.
  Rationale: wait is only meaningful on a session the caller drove; keeping the archive format unchanged avoids a schema bump there.
  Date/Author: 2026-09-11, Fable.
- Decision: migration 28 guards each `ALTER TABLE` with `table_has_column` and creates `api_idempotency` with `IF NOT EXISTS`, following the existing `ensure_*_column` helpers, instead of running a bare batch.
  Rationale: databases rebuilt by another build's ladder — and the migration tests that rewind `user_version` — already carry some of these, and a bare `ALTER TABLE` fails the whole open rather than just that step.
  Date/Author: 2026-09-11, M1 implementation.
- Decision: `ApiBackend` holds a `mj_client::session::SessionControl` rather than the controller's `SessionManagerControl` and the daemon `RuntimeState`.
  Rationale: M1 needs only session lookup and submit plus free-function database reads, and `SessionControl` is a trait boundary a hand-written test fake can implement, so the backend's prompt path is testable without building a daemon runtime. M4 adds the runtime when the export work needs it.
  Date/Author: 2026-09-11, M1 implementation.
- Decision: `ApiSession.last_turn_outcome` is filled only by `GET /sessions/{id}` and by wait, not by the list.
  Rationale: the list is built from the viewer snapshot, which carries no turn identity; reading the durable projection once per session to build a list would make listing cost proportional to the number of sessions.
  Date/Author: 2026-09-11, M1 implementation.
- Decision: `TurnOutcomeKind::Interrupted` maps to the `cancelled` wait outcome and `Rejected` to `error`.
  Rationale: an interruption is what a cancel-turn produces, and reporting it as an error would make a deliberate cancel look like a failure.
  Date/Author: 2026-09-11, M1 implementation.
- Decision: the `SubagentBackend` methods for M2-M4 return `anyhow::bail!`-style errors rather than panicking.
  Rationale: a `todo!()` in a trait object reachable from an HTTP handler turns a caller's mistake into a daemon panic.
  Date/Author: 2026-09-11, M1 implementation.

- Decision: `ApiBackend` reads session record state through a `SessionStateSource` function (`Arc<dyn Fn(&str) -> Option<SessionState>>`) that the daemon builds from `RuntimeState::session_state`, rather than holding `Arc<RuntimeState>`.
  Rationale: the follow-up needs one field of one record, a new `RuntimeState::session_state` reads it under one lock instead of cloning every record the way `session_projection` does, and a function keeps the backend constructible in a test without a daemon runtime.
  Date/Author: 2026-09-11, M2 implementation.
- Decision: a follow-up with no prompt removes its start entry instead of leaving it `Pending`.
  Rationale: `StartStatus` has no "nothing to wait for" state, and a permanent `Pending` would make every later wait on that session look like a start still in flight; with no entry, wait falls back to its newest-turn rules, which is the truth.
  Date/Author: 2026-09-11, M2 implementation.
- Decision: creation answers 201 as soon as the controller publishes an id, and the prompt is submitted by the backend's follow-up task.
  Rationale: provisioning a target takes minutes; holding the HTTP request open for it would make every creation look like a timeout, and the caller's next call is a wait that reports the follow-up's outcome anyway.
  Date/Author: 2026-09-11, M2 implementation.

- Decision: an oversized `limit` on the transcript route is clamped to 1000 rather than refused.
  Rationale: paging is the route's purpose, and a 400 would only make the caller retry with a smaller number it has to guess at; the response says how far the page reached through `latest_seq`.
  Date/Author: 2026-09-11, M3 implementation.
- Decision: `ApiBackend` reaches the daemon's records and checkpoints through an `ExportRuntime` trait that `RuntimeState` implements, rather than holding `Arc<RuntimeState>`.
  Rationale: the export path needs exactly two things from the runtime, and a trait keeps the backend constructible in the existing follow-up tests, which have no daemon runtime - the same reasoning that made session state a function in M1.
  Date/Author: 2026-09-11, M4 implementation.
- Decision: the worker's push-branch reports "no push remote configured" as a typed `PushBranchError::NoRemote`, but the API maps a failed worker command to 500 rather than 409.
  Rationale: the type is what the git helper's own test asserts; across the process boundary only an exit code survives, and inventing a second exit-code convention to recover the distinction would buy less than it costs. Exit code 2 is already reserved for clap's usage failure, which is how a worker too old to know these subcommands is told apart.
  Date/Author: 2026-09-11, M4 implementation.
- Decision: the bundle export names the primary repository from the checkpoint archive's own manifest, not from the session's export layout.
  Rationale: a stopped session has no target and therefore no layout, and the archive is the only thing a stopped session's bundle can come from; reading the name from the same place as the bytes keeps the two from disagreeing.
  Date/Author: 2026-09-11, M4 implementation.
- Decision: each transcript item carries both flattened `text` and its raw `body`.
  Rationale: the common caller wants to read the conversation, and flattening a tool call or a plan correctly needs the same helpers every other surface uses; a caller that needs the structure should not have to re-derive it from prose.
  Date/Author: 2026-09-11, M3 implementation.

- Decision: worker export refusals use exit code 3 with the reason on standard error, and `session_diff` and `read_session_file` return a typed `SessionExportError`.
  Rationale: M4 decided the distinction was not worth a second exit-code convention, but the result was that the most likely user mistakes -- an unconfigured push remote, a file outside the workspace -- looked like daemon failures. One named constant shared by the worker and `worker_command` costs less than the wrong status code.
  Date/Author: 2026-09-11, M5 implementation.
- Decision: the CLI reads one file through `mj export --kind file --path`, rather than a separate subcommand.
  Rationale: `GET /files` is one of the ways to get a session's work out, which is what `mj export` is for, and a fourth kind reads better than a twelfth top-level subcommand.
  Date/Author: 2026-09-11, M5 implementation.
- Decision: `mj new` takes the session-scoped workspace as `--workspace-id`, and resolves the global `--workspace NAME` through the existing `resolve_store_workspace`.
  Rationale: `--workspace` is already a global flag naming a workspace by name; reusing it for an id would make one spelling mean two things.
  Date/Author: 2026-09-11, M5 implementation.
- Decision: `mj wait` and `mj prompt --wait` exit non-zero on any outcome but `finished`.
  Rationale: the caller is a script or an orchestrating agent, and a turn that errored, was cancelled, timed out, or hit a quota limit is not a success it should continue past without looking.
  Date/Author: 2026-09-11, M5 implementation.

- Decision: `wait` is turn-scoped. A launch failure for this session, a failed start, a stopped session, and the turn's own record end a wait; a session-scoped error badge does not.
  Rationale: `has_error` is a durable property of the session with no expiry, while a wait answers one question -- how did this turn end. Reporting the badge answered the wrong question and did it wrong, because the badge outlived the failure that set it. The endings that must still be `error` all reach the caller through the turn record or the two start rules.
  Date/Author: 2026-09-11, defect fix.
- Decision: `WaitResponse.relay` reports the live view's health (`connected`, `disconnected`, `unreachable`, `target_missing`, `projection_integrity`) and never changes the outcome. A session with no live actor carries no `relay` field.
  Rationale: a caller needs to tell "still working" from "the daemon cannot see the worker", and those are identical from a `timeout` alone. Making it an outcome instead would invent an ending the turn never reached; omitting it for a session with no live actor is honest where a fabricated `disconnected` would not be.
  Date/Author: 2026-09-11, defect fix.
- Decision: a recorded `ViewError` outranks `connected` when building `RelayHealth`.
  Rationale: a relay that dropped mid-turn can still read as connected; the error is the specific thing standing between the caller and a finished turn.
  Date/Author: 2026-09-11, defect fix.

## Outcomes & Retrospective

M1 (2026-09-11). The projection and the SQLite store now carry per-turn identity and outcome, and the daemon serves `/api/v1/sessions`, `/sessions/{id}`, `/sessions/{id}/prompt`, `/wait`, `/close` and `/cancel-turn` behind a bearer token with a `Mj-Api-Version: 1` header and `Cache-Control: no-store` on every response, 401s included. A caller can submit a prompt, receive its relay acceptance ordinal as `turn_id`, and block on that specific turn; queued prompts and capacity retries are handled by the pure `resolve_wait`, which is tested directly rather than through the HTTP loop.

What remains for later milestones is what the plan already scheduled: session creation with model, effort and a first prompt (M2), transcript paging (M3), export (M4), and the CLI and documentation (M5). The `SubagentBackend` trait already declares those methods, so adding them changes implementations rather than the trait.

The lesson worth carrying forward is that this repository's schema migrations must be structurally guarded rather than version-guarded: six existing tests rewind `user_version` without removing later migrations' artifacts, and a bare `ALTER TABLE` in a new migration fails all of them.

M5 (2026-09-11). The feature is complete. `mj` now carries the subagent commands the issue asked for -- `new`, `prompt` (with `--wait`), `wait`, `transcript`, `diff`, `export`, `sessions`, `close`, `cancel-turn`, and `api-info` -- each a thin client over the `/api/v1` routes through `mj-cli/src/api_client.rs`, each with `--json` printing the route's response unchanged. The client resolves the viewer URL the way the desktop bootstrap does, reads the bearer token from the file the daemon mints, and refuses any response that does not name contract version 1, so a `mj` talking to a daemon of another major version says so instead of guessing at the body. `docs/src/content/docs/api-reference.md` documents every route, the wait outcomes, transcript paging by `seq`, idempotency keys, the export preconditions, and the CLI-to-route mapping.

Two things were worth the detour. The first is the refusal exit code: M4 had judged the distinction not worth a convention, but the practical effect was that a missing push remote read as a server failure, so M5 reversed that decision and carried the reason across the process boundary. The second is that clippy judges dead code on the non-test build: two client methods the plan required had to earn real callers rather than living on their tests, which produced `mj sessions --session` and `mj export --kind file` -- both better than the alternative of deleting them.

What is still untested by machine is the end-to-end acceptance run in "Validation and Acceptance": it needs a daemon, a configured profile, and a real target, so it remains a manual check.

## Context and Orientation

This is a Rust workspace. The crates that matter here:

- `src/` is the core library crate (`hel`). It holds the session state model (`src/hel_state.rs`), the transcript model (`src/hel_transcript.rs`), the projection that turns worker relay events into durable state (`src/hel_projection.rs`), the SQLite store (`src/hel_database.rs`, schema and migrations in `src/hel_database/schema.rs`), target command construction (`src/hel_targets.rs`), the checkpoint archive (`src/hel_archive.rs`, git helpers in `src/hel_archive/git.rs`), and the worker relay (`src/hel_worker.rs`, `src/hel_worker/snapshot.rs`, `src/hel_worker/capacity.rs`).
- `mj-controller/` owns the `Controller` (session registration, provisioning, checkpoints, close) and the web viewer HTTP server (`mj-controller/src/hel_server.rs`, about 7,500 lines, axum 0.8).
- `mj-client/` defines the surface-facing session handle (`mj-client/src/session.rs`: `SessionHandle` with `view()`, `changed()`, `submit()`, `SessionControl::session()` and `wait_for_session()`).
- `mj-cli/` is the `mj` binary: clap subcommands in `mj-cli/src/main.rs`, the per-user daemon in `mj-cli/src/daemon.rs` (`RuntimeState`), and the web server's control loop in `mj-cli/src/server.rs`, which owns the channels the HTTP handlers use.
- `mj-worker/` is the binary installed on every target as `<worker_root>/hel`; `mj-worker/src/main.rs` lists its `worker` subcommands (export-checkpoint, restore-checkpoint, and so on).
- `docs/` is an Astro documentation site; pages live in `docs/src/content/docs/` and the sidebar in `docs/astro.config.mjs`.

Terms used below:

- A **session** is one agent conversation running on a **target** (a local directory, a container, an SSH host). Its durable record is `SessionRecord` (`src/hel_state.rs` around 968) with `state` (Provisioning, Running, Stopped, ...) and `target: Option<TargetLocator>` (the provisioned instance; `None` once stopped).
- The **relay** is the worker-side durable command journal. Submitting a command appends a `CommandQueued` event and returns its ordinal (the **acceptance ordinal**). When the command starts, `CommandStarted` is appended; when a prompt finishes, `CommandCompleted { outcome: Prompt { stop_reason } }`.
- The **projection** (`src/hel_projection.rs`) folds relay events into `MaterializedSession` (`src/hel_state.rs` around 157): `execution` (Idle, Running{started_at_ms}, Closing, Closed), `transcript: Vec<Arc<TranscriptItem>>`, and `queued_prompts`. Each `TranscriptItem` has a `position` equal to the ordinal of the event that created it and, for agent messages, `latest_content_event_ordinal`. The projection is written to SQLite continuously (`materialized_sessions`, `materialized_transcript_items`), so it is readable after the session stops.
- The **session manager** (`mj-controller/src/hel_session_manager.rs`) runs one actor per pollable session and publishes a watch channel of `ManagedSessionView { snapshot: Option<ManagedSessionSnapshot>, connected, error }`; the snapshot holds `materialized`, a `window` (`latest_turn_start_position`), and `operational: RelayOperationalState` (`execution`, `capacity_retry`, `config_options`, `native_session_is_ready()`).
- The **web viewer server** exposes `/api/snapshot` (a `ViewerSnapshot` with `sessions: Vec<ViewerSession>`), `/api/actions` (a tagged `ControllerAction` enum: `new`, `prompt`, `close`, `cancel-turn`, `set-config`, ...), `/api/conversations/{id}`, and `/api/events`. Handlers hold `ServerState` (`hel_server.rs` around 1572) with `snapshot_rx`, `action_tx`, `bundle_tx`, and other channels; the control loop in `mj-cli/src/server.rs` consumes them and has `daemon_runtime: Arc<RuntimeState>` and `worker_commands_tx` (a `SessionManagerControl`). The `new` action's reply is parked in `PendingActionReplies` (`mj-cli/src/server.rs` around 463) until the provisional record is published by `PhoneActionStarted` (around 1861-1900), but the handler discards the id because `ActionOutcome::Accepted` carries none.
- A **turn** is one prompt and everything until `CommandCompleted`. A **turn start item** is a user message or a harness-turn marker (`TranscriptItem::is_turn_start`, `src/hel_transcript.rs` around 213).

## Plan of Work

### M1: turn outcome persistence, API module, sessions, prompt, wait

Core (`src/`). In `src/hel_state.rs`, add `accepted_ordinal: Option<u64>` (serde default, skipped when none) to `MaterializedQueuedPrompt`. Add three types next to `MaterializedSession`: `MaterializedTurn { command_id, accepted_ordinal: Option<u64>, turn_start_position: u64, started_at_ms: i64 }`, `TurnOutcomeKind` (tagged `kind`, snake_case: `Completed { stop_reason }`, `Rejected { message }`, `Interrupted { message }`), and `MaterializedTurnOutcome { command_id, accepted_ordinal, turn_start_position: Option<u64>, completed_ordinal, completed_at_ms, outcome }`. Add `active_turn: Option<MaterializedTurn>` and `last_turn_outcome: Option<MaterializedTurnOutcome>` to `MaterializedSession` with `#[serde(default)]`. Every struct literal must gain the fields: `src/hel_projection.rs` (two sites near 1815 and 2078), `src/hel_database.rs` (near 2526 and 2655), `src/hel_database/tests.rs` (335), `mj-controller/src/hel_recovery.rs` (379), `mj-cli/src/server.rs` (4598), `mj-cli/src/session_presentation.rs` (218), `mj-chat/src/hel_chat.rs` (1697), `mj-chat/src/hel_chat/active.rs` (5615), `mj-chat/src/hel_chat/transcript/tests.rs` (six sites), `mj-tui/src/test_support.rs` (300). Use `grep -rn "MaterializedSession {"` to find them all. `canonical_session_from_materialized` and `materialized_session_from_canonical` in `src/hel_projection.rs` ignore the fields (resume yields `None`).

In `src/hel_projection.rs` (`project_observation`, roughly 373-660): the `CommandQueued` prompt and set-config arms (around 442-483) set `accepted_ordinal: Some(event.ordinal)`; `CommandStarted` for a prompt (around 504-537) sets `mutation.active_turn = Some(Some(MaterializedTurn { .. turn_start_position: event.ordinal, started_at_ms }))` carrying the entry's `accepted_ordinal`; `CommandCompleted` with `Prompt { stop_reason }` (around 549-557) sets `last_turn_outcome` from `current.active_turn` plus the event ordinal and `recorded_at_ms`, then clears `active_turn`; `Steered { queued_command_id }` (around 579-604) moves `active_turn` to the steered entry; `CommandRejected`/`CommandInterrupted` (around 618-637) record `Rejected`/`Interrupted` when the command was a prompt (accepted ordinal from the queue entry when not started, from `active_turn` when started, clearing it). `apply_committed_projection_event_inner` (around 236-372) applies the two new mutation fields to the in-memory session.

In `src/hel_database.rs`: `MaterializedSessionMutation` (around 111) gains `active_turn: Option<Option<MaterializedTurn>>` and `last_turn_outcome: Option<MaterializedTurnOutcome>`; `ProjectionPage::apply` and `flush` (around 2886-3089) carry and write them as JSON columns; `read_materialized_session_fields` (around 2672-2735), `load_materialized_projection_tail_from` (around 2505-2545), `load_materialized_session_with` (around 2648), and `write_materialized_session` (around 3861-3903) read and write `active_turn_json` and `last_turn_outcome_json`; `replace_materialized_queue` (around 3984) and `read_materialized_queued_prompts` (around 2809) carry `accepted_ordinal`. Add public readers: `load_materialized_turn_outcome(session_id) -> Result<Option<(MaterializedExecutionState, Option<MaterializedTurn>, Option<MaterializedTurnOutcome>)>>`; `TurnSummary { turn_number: u64, turn_started_at_ms: i64, last_changed_at_ms: i64, final_message: Option<String> }` with `load_materialized_turn_summary(session_id, turn_start_position)` (turn number = count of turn-start items with `position <= ?`, using the predicate already in `last_materialized_turn_start` around 2271; last changed = max `last_changed_at_ms` at or after the start; final message = the `last_materialized_agent_message` query around 2299 restricted to `position > ?`, flattened with `hel_transcript::materialized_chunks_text`); `lookup_api_idempotency(key) -> Result<Option<String>>` and `record_api_idempotency(key, session_id)`.

Schema: bump `SCHEMA_VERSION` (`src/hel_database.rs:31`) from 27 to 28 and add an `if version < 28` block in `src/hel_database/schema.rs` after the `< 27` block (around 676), following the pattern at 646-675: `ALTER TABLE materialized_sessions ADD COLUMN active_turn_json TEXT` and `last_turn_outcome_json TEXT` (each `CHECK(x IS NULL OR json_valid(x))`), `ALTER TABLE materialized_queued_prompts ADD COLUMN accepted_ordinal INTEGER CHECK(accepted_ordinal IS NULL OR accepted_ordinal > 0)`, `CREATE TABLE api_idempotency (key TEXT PRIMARY KEY CHECK(length(trim(key)) BETWEEN 1 AND 128), session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE, created_at_ms INTEGER NOT NULL) STRICT`, the `schema_migrations` insert, and `PRAGMA user_version = 28`. Also add the columns to the fresh-schema creation path so new databases match. `ensure_relay_projection_schema` runs before this block, so the tables exist.

Expose `pub fn is_capacity_stop_reason(stop_reason: &str) -> bool` in `src/hel_worker/capacity.rs` (re-export from `hel_worker`).

Controller (`mj-controller`). In `hel_server.rs`: declare `mod api;` and re-export its wire types; `ServerOptions` (around 158-182) gains `api_token: String` and `subagent: Option<Arc<dyn api::SubagentBackend>>` with `set_api_token` and `set_subagent_backend`; `with_test_credentials` sets token `test-api-token`. Add `pub fn api_token_path() -> PathBuf` (`data_dir().join("api-token")`) and `pub fn load_or_create_api_token(path) -> Result<String>` following `load_or_create_cookie_key` (around 130-148; `hel_config::atomic_write` creates 0600 files). `ServerState` (around 1572) gains `api_token: Arc<str>` and `subagent`. `router` (around 1645) nests `api::router(state.clone())` at `/api/v1` before the body-limit layer. Change `ActionOutcome` (around 1349) to `Accepted { session_id: Option<String> }`, drop `Copy`, make `rejection(&self)`, and update the `action` handler and every constructor in `mj-cli/src/server.rs` (around 486, 1611, 1637, 1657, 1889-1895, 1912, and tests 4291-4340). In `PendingActionReplies` and the `action_started_rx` arm, resolve the parked New reply with `Accepted { session_id: Some(started.session.id) }`.

New file `mj-controller/src/hel_server/api.rs`: constants (`Mj-Api-Version`, `1`, default wait 600 s, max 3600 s, idempotency key max 128 chars); wire types (`StartSessionRequest`, `StartSessionResponse { session_id, turn_id: Option<u64> }`, `PromptRequest { text }`, `PromptResponse { turn_id }`, `WaitRequest { turn_id, timeout_secs }`, `WaitOutcome` enum `finished | error | cancelled | quota_limit | timeout | stopped`, `WaitResponse { outcome, stop_reason, message, final_message, turn_id, turn_number, elapsed_ms, capacity_retry: Option<{attempt, retry_at_ms}>, session: ApiSession }`, `ApiSession` (id, workspace_id, title, harness_kind, profile_id, target_id, bundle_id, state, lifecycle, chat_phase, is_idle, has_error, created_at, updated_at, last_turn_outcome) with `From<&ViewerSession>`, `SessionListResponse`, plus M3/M4 types later); `ApiFailure { status, message: String }` with `IntoResponse` producing `{"error": ...}`, `From<ApiError>`, `From<anyhow::Error>` (500). The trait:

    pub trait SubagentBackend: Send + Sync {
        fn session_handle(&self, session_id: String) -> BoxFuture<'_, Result<Option<SessionHandle>>>;
        fn prompt(&self, session_id: String, text: String) -> BoxFuture<'_, Result<u64>>;
        fn turn_state(&self, session_id: String) -> BoxFuture<'_, Result<Option<TurnState>>>;
        fn turn_summary(&self, session_id: String, turn_start_position: u64) -> BoxFuture<'_, Result<TurnSummary>>;
        fn start_followup(&self, session_id: String, followup: StartFollowup) -> BoxFuture<'_, Result<()>>;   // M2
        fn start_status(&self, session_id: String) -> BoxFuture<'_, Result<Option<StartStatus>>>;            // M2
        fn lookup_idempotency(&self, key: String) -> BoxFuture<'_, Result<Option<String>>>;                   // M2
        fn record_idempotency(&self, key: String, session_id: String) -> BoxFuture<'_, Result<()>>;          // M2
        fn transcript(&self, session_id: String, after_seq: u64, limit: usize) -> BoxFuture<'_, Result<Option<TranscriptPage>>>; // M3
        fn diff(&self, session_id: String) -> BoxFuture<'_, Result<String, ExportError>>;                    // M4
        fn read_file(&self, session_id: String, path: PathBuf) -> BoxFuture<'_, Result<Vec<u8>, ExportError>>; // M4
        fn push_branch(&self, session_id: String, branch: String) -> BoxFuture<'_, Result<PushedBranch, ExportError>>; // M4
        fn bundle(&self, session_id: String) -> BoxFuture<'_, Result<BundleExport, ExportError>>;            // M4
    }

(`BoxFuture` is `mj_client::session::BoxFuture`.) Methods for later milestones may be added in those milestones. The router applies `require_api_auth` (bearer compared with `constant_time_eq`, else the viewer cookie via `cookie_value` and `session_cookie_valid`) as a route layer and an outer layer adding `Mj-Api-Version: 1` and `Cache-Control: no-store` to every response, including 401s. Handlers: `GET /sessions`, `GET /sessions/{id}` (404 via `require_session_record`), `POST /sessions/{id}/prompt` (validate with `validate_action(&ControllerAction::Prompt { .. images: vec![] }, &snapshot)`, 409 when the session lacks the prompt capability, then `backend.prompt` → 202), `POST /sessions/{id}/close` and `/cancel-turn` (build the `ControllerAction`, validate, send a `ControllerRequest` on `action_tx`, map `rejection()` → 202), `POST /sessions/{id}/wait`.

Wait: two pure functions, `map_stop_reason(&str) -> (WaitOutcome, Option<String>)` (case-insensitive `end_turn`/`endturn` → finished, `cancelled` → cancelled, `is_capacity_stop_reason` → quota_limit, else error carrying the raw reason) and `resolve_wait(&WaitObservation, &WaitRequest) -> Option<WaitDecision>`. `WaitObservation` holds the session's lifecycle, `has_error`, whether a launch failure names it, `execution`, `active_turn`, `last_turn_outcome`, queue length, `capacity_retry`, and `start_status`. Rules in order: launch failure, `has_error`, or start `Failed { message }` → error; lifecycle stopped/failed/stopping or execution closing/closed → stopped; the target turn is the explicit `turn_id`, else the start `Submitted { turn_id }`, else "newest" (which additionally requires idle, no active turn, empty queue); done when `last_turn_outcome.accepted_ordinal >= target` unless the outcome is capacity and a retry is pending; idle with nothing queued and no outcome → finished with `turn_id: null`. The handler loops in its own task: obtain `backend.session_handle`; build the observation from `handle.view()` (materialized fields, `queued_prompts.len()`, `operational.capacity_retry`) plus `snapshot_rx.borrow()` session facts and `backend.start_status`; when no handle exists, poll `backend.turn_state` every 500 ms; `tokio::select!` over `handle.changed()`, `snapshot_rx.changed()`, `sleep_until(deadline)`, and `state.shutdown.cancelled()`; re-acquire the handle when `is_stopped()`. On a decision, fill `turn_number`, `elapsed_ms` (`last_changed_at_ms - turn_started_at_ms`) and `final_message` from `backend.turn_summary`. Deadline → `timeout` with the pending `capacity_retry`.

Daemon (`mj-cli`). New file `mj-cli/src/server/api.rs` (declare `mod api;` in `server.rs`): `ApiBackend { runtime: Arc<RuntimeState>, sessions: SessionManagerControl, starts: Mutex<BTreeMap<String, StartStatus>> }` implementing the trait: `session_handle` via `sessions.session(id).await.ok().map(|h| h.client())`; `prompt` submits `RelayCommand::Prompt { prompt: vec![ContentBlock::Text(..)] }` with `new_command_id("api")` (pattern: the Prompt arm of `apply_phone_action` around 2336-2353); DB readers on `spawn_blocking`. In `run_server` (around 775-797) load the token and install the backend.

### M2: start

`StartSessionRequest { workspace_id, profile_id, target_id, bundle_id, project_directory, title, model, effort, prompt, idempotency_key }`. Handler: validate the prompt with the Prompt rules and the key length; if the key exists (`lookup_idempotency`) reply 200 with the existing id and its start `turn_id` if `start_status` is `Submitted`; if `project_directory` is given without `bundle_id`, create a quick bundle through `state.bundle_tx` exactly as `create_bundle` does (`hel_server.rs` around 1963) and use its id; build `ControllerAction::New { workspace_id: unwrap_or_default, profile_id, bundle_id, target_id, title, project_directory, dirty_ack: vec![] }`, `validate_action`, send on `action_tx`; on `Accepted { session_id: Some(id) }` call `backend.start_followup(id, StartFollowup { model, effort, prompt })`, record the key, reply 201 `{ session_id, turn_id: null }`.

`ApiBackend::start_followup` inserts `Pending` and spawns a supervised task (join handle kept; a panic becomes `Failed`): loop `sessions.wait_for_session(&id, 5 s)` until the actor exists, failing if the record's state leaves `Provisioning | Running | Disconnected | Checkpointing` or after 30 minutes; await `handle.changed()` until `view.connected && view.snapshot.is_some()` (and, when model or effort were given, `operational.native_session_is_ready()`); validate model/effort against `operational.config_options` choices (same rule as `validate_action` for set-config, `hel_server.rs` around 2992-3017) and submit `RelayCommand::SetConfig` for each; submit the prompt and store `Submitted { turn_id }`. `view.error` of `TargetMissing` fails immediately. Prune entries for sessions no longer in the snapshot.

### M3: transcript

`src/hel_database.rs`: `TranscriptPage { items: Vec<Arc<TranscriptItem>>, latest_seq: u64, execution }` and `load_materialized_transcript_after(session_id, after_seq, limit) -> Result<Option<TranscriptPage>>` using `seq = COALESCE(latest_content_event_ordinal, position)`, `WHERE seq > ?` ordered by seq then `stable_id`, `LIMIT ?`; `latest_seq` is the max seq. `src/hel_transcript.rs`: `transcript_item_role(&TranscriptBody) -> &'static str` and `transcript_item_text(&TranscriptItem) -> String` built on `materialized_content_text`, `materialized_chunks_text`, the tool presentation summary (else the call title), `terminal_output_detail`, plan entry lines, and system text. Handler `GET /sessions/{id}/transcript?after_seq&limit` (default 200, max 1000) returns `{ session_id, latest_seq, execution, items: [{ stable_id, position, seq, role, text, created_at_ms, last_changed_at_ms, body }] }` where `body` is the raw `TranscriptBody` JSON; 404 when no row.

### M4: export

Worker: in `mj-worker/src/main.rs` add `Diff { --repository, --base?, --branch? }`, `ReadFile { --root, --path }`, `PushBranch { --repository, --branch }`. Git logic in `src/hel_archive/git.rs`: `session_diff(runner, repository, base: Option<&str>, branch: Option<&str>) -> Result<String>` resolving the base as `--base`, else `remote_workspace_base` (git config `mj.baseCommit`), else the last line of `git reflog show --format=%H refs/heads/<branch>` (the creation commit), else an error "no session base recorded"; then `capture_worktree_tree` and `diff_between_trees` against `rev-parse <base>^{tree}`. `read-file` validates the path with `hel_config::validate_relative_destination`, requires the canonical path to stay under the canonical root, caps at 16 MiB, and writes bytes to stdout. `push-branch` validates with `git check-ref-format --branch`, picks `remote.pushDefault` else `origin` (error when neither), and runs `git push <remote> HEAD:refs/heads/<branch>` through `SystemGit`. Re-export from `hel_archive`.

Controller: add `base_commit: Option<String>` (serde default) to `ManagedWorktree` (`src/hel_state.rs` around 779) and set it at both creation sites in `mj-controller/src/hel_controller/worktree.rs` (around 170-206 and 237-263). Factor the locator, workspace root, and primary repository derivation inline in `mj-controller/src/hel_controller/checkpoint.rs` (around 638-712) into `pub fn Controller::session_export_layout(&self, session_id, executor) -> Result<SessionExportLayout { backend: TargetLocator, workspace_root: String, primary_repository: String, repositories: Vec<CheckpointRepositorySpec>, managed_worktree: Option<ManagedWorktree> }>` used by both. Factor the `DaemonAction::CheckpointSession` body (`mj-cli/src/daemon.rs` around 4804) into `RuntimeState::checkpoint_session_now(id)`.

Backend: `target_command(session_id, args, purpose)` on `spawn_blocking`: `Controller::load()?.session_export_layout(..)`, binary `<worker_root>/hel` (`hel_targets::worker_root`), `command_on_locator(&backend, id, [binary, "worker", ...], purpose)`, `CancellableProcessExecutor::with_timeout(5 min)`; non-zero status → error with stderr; a clap usage failure (exit 2) → `ExportError::Refused("worker on this target predates export support; resume the session to upgrade")`. `diff` passes `--base <managed_worktree.base_commit>` when present, else `--branch mj/<id>` for managed worktrees; the repository path is `workspace_root/<primary relative_destination>`. `push_branch` refuses while the live view shows a running turn. `bundle`: live → `checkpoint_session_now`, stopped → `session.checkpoint.archive_path`; then `hel_archive::verify_repository_bundles_streaming` and the primary repository's `committed_bundle` (refused when empty). All of diff, files, branch are refused when `session.target` is `None`. `ExportError { Refused(String), Failed(anyhow::Error) }` maps to 409 and 500.

Handlers: `GET /sessions/{id}/diff` (`text/x-diff`), `GET /sessions/{id}/files?path=` (`application/octet-stream`), `POST /sessions/{id}/export { kind: patch | branch | bundle, branch? }` (patch = diff body; branch = `{ branch, remote }`; bundle = octet stream with `Content-Disposition: attachment; filename="<session>-<repo>.bundle"`).

### M5: CLI and docs

Add `reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }` to `mj-cli/Cargo.toml`. New `mj-cli/src/api_client.rs`: `ApiClient::connect()` resolves the viewer URL from `daemon::connect_or_start().await?.status().await?.phone_status` (`WebViewerStatus::Ready { viewer_url, .. }`; reuse the error handling in `mj-cli/src/desktop.rs` around 36-55 for the other states) and reads the token file; typed methods using the `mj_controller::hel_server::api` structs; non-2xx becomes an error carrying the `error` field. New `mj-cli/src/api_commands.rs` and clap variants in `mj-cli/src/main.rs`: `New`, `Prompt` (`--wait`), `Wait`, `Transcript`, `Diff`, `Export` (`--kind`, `--branch`, `--out`), `Sessions`, `Close`, `ApiInfo`, each with `--json`; update `command_name`. Docs: `docs/src/content/docs/api-reference.md`, sidebar entry in `docs/astro.config.mjs` (Reference group, around 114-125), a section in `cli-reference.md`.

## Concrete Steps

All commands run from `/home/jonathan/Projects/hel2`. Cargo tests must run outside the sandbox.

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    npm --prefix docs run build        # after M5

Commit after each milestone with only the files changed, and update the Progress section in this file in the same commit.

## Validation and Acceptance

Unit and behavior tests (colocated `#[cfg(test)]`):

- Projection: a completed prompt records its stop reason and accepted ordinal and clears the active turn; a rejected queued prompt records `Rejected` with no turn start.
- Database: a projection page persists the outcome and the queue's accepted ordinal; a version 27 database migrates to 28 and `load_materialized_turn_outcome` returns `None`; transcript paging by seq returns an updated agent item once, after its update.
- `hel_server/api.rs` (router built with `with_test_credentials` and a hand-written `FakeBackend`, driven with `tower::ServiceExt::oneshot`, following the tests around `hel_server.rs` 3787-3910): 401 without bearer or cookie and 200 with either; the version header on every response including 401; prompt validation (leading `!` → 400; no prompt capability → 409); `map_stop_reason` and `resolve_wait` cases (queued ordering, capacity retry pending keeps waiting, capacity without retry → `quota_limit`, rejected → `error`, stopped lifecycle → `stopped`, idle empty queue → immediate `finished`); wait completes when the fake backend publishes an outcome (use `tokio::time::pause`); close and cancel-turn arrive on `action_rx`; start returns the published id and records the key; a repeated key returns 200; transcript clamps the limit and flattens text; export content types and the 409 mapping.
- `mj-cli`: `ApiBackend::prompt` submits a text block and the start follow-up submits set-config before the prompt (use `hel_session_manager::replacement_session_test_fixture`, around 3186); clap parse tests; the API client sends the bearer header against an in-test axum router.
- Worker and git (`src/hel_archive/tests.rs` helpers `initialize_repository`, `git`, `git_line`): the diff includes a tracked change and an untracked file against `mj.baseCommit`; the reflog fallback works; `read-file` rejects `..` and a symlink escaping the root.

No test launches a real harness.

Manual acceptance after M5, with the daemon running, a configured profile, and a local bare target: `mj api-info` prints the URL and token path; `mj new ... --prompt "add a README line" --json` prints a session id; `mj wait --session <id>` prints `finished`, a turn number, and the final message; `mj transcript --session <id> --json` prints items with increasing `seq`; `mj diff --session <id>` prints a unified diff of the change; `mj export --session <id> --kind bundle --out work.bundle` writes a file `git bundle verify` accepts; `mj close --session <id>` closes; the transcript command still answers afterwards. With curl, `GET /api/v1/sessions` with the bearer header returns 200 and `Mj-Api-Version: 1`; without it, 401 with the same header.

## Idempotence and Recovery

Every step is additive. The schema migration runs once, guarded by `user_version`. Re-running any `mj` command is safe; `mj new --idempotency-key` returns the same session on retry. If a milestone fails validation, fix forward on the same branch; do not rebase.

## Artifacts and Notes

(Record test output excerpts and transcripts here as milestones complete.)

## Interfaces and Dependencies

New public items by the end: in `hel` (`src/`): `hel_state::{MaterializedTurn, TurnOutcomeKind, MaterializedTurnOutcome}`, `hel_database::{load_materialized_turn_outcome, load_materialized_turn_summary, TurnSummary, load_materialized_transcript_after, TranscriptPage, lookup_api_idempotency, record_api_idempotency}`, `hel_transcript::{transcript_item_role, transcript_item_text}`, `hel_worker::is_capacity_stop_reason`, `hel_archive::session_diff`. In `mj-controller`: `hel_server::api::{SubagentBackend, ApiFailure, ExportError, StartSessionRequest, StartSessionResponse, PromptRequest, PromptResponse, WaitRequest, WaitResponse, WaitOutcome, ApiSession, SessionListResponse, TranscriptResponse, TranscriptItemView, ExportRequest, ExportKind, map_stop_reason, resolve_wait}`, `hel_server::{api_token_path, load_or_create_api_token}`, `hel_controller::Controller::session_export_layout`. In `mj-cli`: `server::api::ApiBackend`, `RuntimeState::checkpoint_session_now`, `api_client::ApiClient`. New dependency: `reqwest` in `mj-cli`. Worker subcommands: `hel worker diff`, `hel worker read-file`, `hel worker push-branch`.
