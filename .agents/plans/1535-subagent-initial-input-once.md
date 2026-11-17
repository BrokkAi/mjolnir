# Deliver a sub-agent's initial input at most once per session history

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

Issue: brokkai/mjolnir#1535.


## Purpose / Big Picture

A sub-agent (a "child" session that another session started through the `mj-agents` `spawn` tool) begins with an initial input: optional model and effort settings, followed by its assignment prompt. Today, restoring a child from a checkpoint delivers that input again. Restoring means Restart, suspend and resume, Move (including the automatic quota Move), or plain checkpoint restore. On 2026-11-13, a Restart of two children that had finished their work two days earlier sent their assignment to Claude again. Only an exhausted quota stopped them from redoing the work. The repeated prompt also froze the dashboard view of both sessions with a "projection integrity" error.

After this change, a child's initial input is delivered at most once per session history. Restarting, resuming or moving a child whose assignment has already started continues the existing conversation and sends nothing new. A child whose assignment never started still receives it after a restore. You can see it work by running the new end-to-end test, which spawns a child in an isolated instance, lets it finish, restarts it through the daemon API, and checks that nothing was re-sent and that the session view stays healthy.


## Progress

- [x] (2026-11-13 16:40Z) Diagnosis and fact mapping done; design chosen (see Decision Log).
- [x] (2026-11-13) Milestone 1: `LaunchHistory` and `prompt_started` in `launch.rs`; `materialized_transcript_item_exists` in `database/materialized.rs`; both restore sites in `resume.rs` pass `Restored(canonical_session)`; every other caller passes `Stored`.
- [x] (2026-11-13) Milestone 2: `python3 tests/e2e/delegation.py --restart-regression-only` fails on the unmodified controller (three re-sent `subagent-spawn-*` ids, then the incident's "changed immutable identity fields" error) and passes with the change. Controller test `a_launch_delivers_a_childs_assignment_only_until_its_history_shows_it_started` covers queued-not-started, started (canonical and stored).
- [x] (2026-11-13) Full validation: `cargo test -p brokk-mj-controller` 2628 passed, 0 failed; workspace `cargo clippy --all-targets -- -D warnings` clean. Commit, review and push follow.


## Surprises & Discoveries

- Observation: the worker's only guard against re-sending initial input is relay-local, and every restore deletes the relay state.
  Evidence: `seed_initial_commands` in `mj-worker/src/relay.rs` (about lines 741-777) skips only when `snapshot.native_session_used` is set or `snapshot.handled_commands` holds the last initial command id. The restore reset (`mj-controller/src/targets/worker_daemon.rs`, about lines 391-430) removes `relay-state.json` and the journal, and the restored relay starts from `relay-seed.json`, which carries no command receipts. In the incident, `handled_commands` went from 101 entries to the 3 re-sent ids.

- Observation: the daemon's durable `startup_steps` record is written asynchronously, so it is not a safe owner for this fact.
  Evidence: `drain_startup_queue` in `mj-controller/src/daemon/state.rs` polls the projection every 100 ms through `observe_worker_startup_step` and then writes `accepted` and `done`. The drain holds no lifecycle admission, and Restart takes no startup gate, so a Restart can begin between the worker's receipt and that write.

- Observation: a restore rebuilds the stored projection from the archive only when their frontiers differ, and it writes that projection after it has already prepared the launch file.
  Evidence: `projection_rebuild_required` and `materialized_session_from_canonical` in `mj-controller/src/controller/resume.rs` (about lines 1816-1846). The save is at about line 922, and the launch file is prepared at about lines 807 and 865. During a restore, therefore, only the archive's canonical snapshot is guaranteed to describe the history being restored.


- Observation: caller classification for `session_launch_config` / `stage_worker_files_for`. Restored history: the two `resume.rs` sites in `restore_into_target` (retained-native `prepare_worker_files_for(RefreshProfile)` and the `prepare_worker_files` lane; both delete `relay-state.json` first, and Move and Restart reach them through this function). Stored history: creation (`provisioning.rs` `prepare_worker_files` and `stage_worker_files_for`), and `worker_launch_config_for_move`, which serves `current_worker_launch_config` (worker restart, upgrade, park, container upgrade, checkpoint-only restart, backend probe) and `worker_recovery_plan`. All of those keep `relay-state.json`, so the worker's own receipts still apply. No caller fit neither case.
  Evidence: `rg "session_launch_config|stage_worker_files_for|prepare_worker_files"` on `mj-controller/src`.

- Observation: the automatic completion check can refuse a child's first `handback` with "Not delivered yet ... call handback again with your report unchanged". The e2e script retries once.
  Evidence: `tools.jsonl` in the first failed run's artifacts.

## Decision Log

- Decision: the fact "this child's assignment has started" is read from the session history: the transcript item whose stable id is `user:<assignment prompt command id>`. It is decided in one place, `session_launch_config` in `mj-controller/src/controller/worker_binary/launch.rs`, which is the only producer of a launch file's `initial_commands`.
  Rationale: the worker creates that transcript item when the prompt starts (`mj-transcript` observation handling), and the item travels with every checkpoint in `CanonicalSessionSnapshot.transcript`. Deciding from it needs no new persisted field and no change to the relay seed, archive, relay state, database schema or protocol. It has no race with the startup drain, and it works for sessions whose `startup_steps` rows were pruned or never existed.
  Date/Author: 2026-11-13, parent session (Claude Opus 5.5).

- Decision: during a restore, the rule reads the archive's canonical snapshot. Everywhere else (first creation, and worker restarts or upgrades that keep `relay-state.json`), it reads the stored projection.
  Rationale: during a restore, the stored projection can be older or newer than the archive until it is rebuilt (see Surprises). Outside a restore, the stored projection is the live history. A single function takes the history as an argument, so there is one rule with one meaning.
  Date/Author: 2026-11-13, parent session.

- Decision: a prompt that was accepted but had not started at the checkpoint does not count as delivered.
  Rationale: its assignment never ran. With the queue policy `Restore`, the seed brings it back under its original command id and the worker's existing dedup skips the repeat. With `Defer`, the controller re-admits it under the same id, and the worker's dedup by id prevents a double. With `Discard`, the queue is dropped and the launch file delivers the assignment, which is correct because it never ran.
  Date/Author: 2026-11-13, parent session.

- Decision: the assignment step is found by its id, `startup_step_id(group, "prompt")`, not by its position in the loaded group. When it has started, the whole `initial_commands` list is dropped, including model, effort and fast mode.
  Rationale: the id is the identity the transcript item carries. The settings were sent before the prompt, and the restored conversation and the seed's accepted configuration already hold them, so resending them would only replay old commands.
  Date/Author: 2026-11-13, parent session, from the implementer's proposal.

- Decision: rejected alternatives. Making `startup_steps.phase` the owner (it lags the worker's receipt, as shown above). Restoring a command ledger in the seed (that reverses a deliberate earlier removal and changes a persisted format for no gain over the transcript, which already carries the fact). Relaxing the projection integrity check (it correctly caught the replay).
  Date/Author: 2026-11-13, parent session.


## Outcomes & Retrospective

2026-11-13: both milestones are done. The end-to-end regression (`python3 tests/e2e/delegation.py --restart-regression-only`, isolated instance) reproduced the incident on the old controller. The Restart re-sent `subagent-spawn-<key>-model`, `-effort` and `-prompt`, and the daemon then logged "changed immutable identity fields". With the change, the same run passes: nothing is re-sent, the view has no error, and the transcript holds one assignment prompt. The fix needed no format, schema or protocol change. It touches only the controller's launch builder and its callers, so it takes effect when the daemon is upgraded; workers are unchanged. Remaining limit: a restore of a child whose assignment never started still sends it, by design.


## Context and Orientation

Mjolnir has a daemon (the control plane, crate `mj-controller`) and one worker per session (the data plane, crate `mj-worker`). The worker runs the coding agent's harness, for example Claude Code through its ACP bridge. It records everything the session does in a "relay journal" of numbered events, called ordinals. It keeps its current state in `relay-state.json` inside the session's "worker root" directory, for example `~/.local/share/mjolnir/workers/<session id>/`. The daemon reads those events and keeps a "projection" of them in its SQLite store, for example the table `materialized_transcript_items`, keyed by `(session_id, stable_id)`. The dashboard shows that projection.

A "checkpoint" is a verified archive of a session. Its relay-facing part is `CanonicalSessionSnapshot` (`mj-core/src/archive.rs`, about line 226), which holds the event frontier (the last ordinal included), the transcript, the queued prompts, the configuration values and the assessment state. The controller builds it from its projection at a checkpoint barrier, a point where the worker is held still (`canonical_session_from_materialized`, `mj-transcript/src/projection/materialize.rs`, about lines 310-405).

A "restore" starts a session's worker from a checkpoint. Every restore path ends in `Controller::restore_into_target` (`mj-controller/src/controller/resume.rs`, about line 616). That covers Restart in place (`mj-controller/src/daemon/restart.rs`), the Restart fallback and suspend/resume (`resume_session_with_origin`), Move in place or across targets (`mj-controller/src/controller/move_session.rs`), and legacy recovery (`in_place.rs`). A restore deletes the old relay state. It writes `relay-seed.json`, built by `relay_seed_for_restore` (`mj-checkpoint/src/checkpoint/restore.rs`), and writes a fresh launch file `launch.json`. That file comes from `prepare_worker_files_for` and `stage_worker_files_for`, which call `session_launch_config` (all in `mj-controller/src/controller/worker_binary/launch.rs`). `worker_launch_config_for_move` in the same file also calls `session_launch_config`.

A child's initial input is a "startup group" of rows in the store table `startup_steps`. The group id is `GeneratedPrompt::subagent_spawn_id(request_key)` (`mj-core/src/continuation.rs`), for example `subagent-spawn-9f2bb565...`. The step ids are `<group>-model`, `<group>-effort`, optionally `<group>-fast-mode`, and last `<group>-prompt` (`startup_step_id` in `mj-controller/src/daemon/startup_followup.rs`). `session_launch_config` loads that group with `crate::database::load_startup_group` and turns it into `launch.initial_commands` (about lines 710-748). It does this for every child that is not a reviewer, on every launch. The worker's `seed_initial_commands` submits those commands under their original ids when its relay has not handled them. The worker requires the last initial command to be the prompt.

When the assignment prompt starts, the projection gets a transcript item with stable id `user:<group>-prompt`. A queued prompt that has not started has no transcript item; it is listed in `queued_prompts` under its command id. Settings commands never create transcript items. The function `load_prompt_acceptance` in `mj-controller/src/database/materialized.rs` (about line 393) already queries `materialized_transcript_items` for `user:<command id>`, which shows the SQL to reuse for the stored history.


## Plan of Work

Milestone 1 adds one rule and feeds it the right history. In `mj-controller/src/controller/worker_binary/launch.rs`, add a small type next to `session_launch_config` that says which history a launch continues: either the stored projection or a restored canonical snapshot. Add a function that answers whether that history shows a given prompt command id as started. For a canonical snapshot, it checks whether `transcript` has an item with stable id `user:<id>`. For the stored projection, it runs one indexed query on `materialized_transcript_items` for `user:<id>`; put that query in `mj-controller/src/database/materialized.rs` beside `load_prompt_acceptance`, and reuse its shape. In `session_launch_config`, after loading the child's startup group, find the step whose id is `<group>-prompt`. If the history shows it started, set no `initial_commands` for this launch. Otherwise build them as today. Write a short comment that names the rule and the reason: a restore deletes the worker's receipts, so the session history decides.

Thread the history argument through the callers. `restore_into_target` passes the archive's canonical snapshot (it already holds `archive.canonical_session`) into `prepare_worker_files_for` or `prepare_worker_files` and from there to `session_launch_config`. Every other caller passes the stored projection; this includes creation, `replace_installed_worker_launch_config` for worker restart, upgrade, park and container upgrade, and `worker_launch_config_for_move` unless it is building a launch for a restore whose canonical snapshot is in scope. Check each caller of `stage_worker_files_for` and `session_launch_config` and record in `Surprises & Discoveries` any that do not fit these two cases.

Do not change the worker's `seed_initial_commands` guard. It still protects against a crash during the first start inside a single relay, which is a different question: whether this relay already handled these ids. Do not change `startup_steps` bookkeeping. The drain keeps observing receipts, and on a restored history it finds the prompt already started.

Milestone 2 adds the regression tests described under Validation and Acceptance.


## Concrete Steps

Work in the repository root on the current branch (do not create a branch). Build with plain Cargo; the project uses mbx for build caching, and target directories must not be redirected. Run tests outside the restricted sandbox, because they use loopback sockets.

    cargo test -p brokk-mj-controller <filter for the new test>
    cargo test -p brokk-mj-worker relay::
    python3 tests/e2e/<new or extended script>.py

At the end, once:

    cargo clippy --all-targets -- -D warnings
    cargo test -p brokk-mj-controller
    cargo test -p brokk-mj-checkpoint    (only if that crate was touched)


## Validation and Acceptance

The main acceptance is end-to-end. Extend `tests/e2e/delegation.py`, or add a script beside it in the same style. Do it in an isolated `--instance` with its own `MJ_CONFIG_DIR` and `MJ_DATA_DIR`, and with the fake ACP harness those scripts already use. The test must:

1. Spawn a child.
2. Let the child hand back.
3. Restart the child with `POST /api/v1/sessions/<child id>/restart`, using the instance's API URL and token from `mj api-info` and `mj api-token`.
4. Wait until the child is running again.
5. Assert that the child's new relay journal (`workers/<child id>/relay-journal/active.jsonl` in that instance) has no `command_queued` event whose command id starts with `subagent-spawn-`.
6. Assert that the session view reports no error, which shows the daemon's sync is healthy.
7. Assert that the child's transcript holds exactly one assignment prompt item.

Mark it `# Hard-won: #1535: a Restart re-sent a finished sub-agent's assignment and froze its view.` It must fail on the current code (the journal has the three `subagent-spawn-*` commands) and pass after Milestone 1.

Add one controller test for the tricky case. A restore whose canonical snapshot has the assignment prompt only in `queued_prompts` (not started) still produces `initial_commands`. The same snapshot with the `user:<group>-prompt` transcript item produces none. Put it next to the closest existing launch or restore test (`mj-controller/src/controller/tests.rs` near line 2000, or `mj-controller/src/daemon/tests.rs` near line 10040), and drive it through the real builder, not a copy of the rule.

All touched crates' full suites pass, and workspace clippy passes with `-D warnings`.


## Idempotence and Recovery

The change is code-only. There is no migration, no format change and no protocol change, so it can be applied and reverted freely. The end-to-end test uses its own instance directory; delete that directory to clean up. Never point a test build at the default instance.


## Artifacts and Notes

The incident's evidence (journals before and after the Restart, relay state, seed and launch files, store rows and daemon log lines) is saved under `.mj/agents/parent-restart-respawn/` in the parent's clone. This is not part of the repository. The re-sent commands looked like this in the new journal:

    851 command_queued subagent-spawn-9f2bb565...-model set_config
    853 command_queued subagent-spawn-9f2bb565...-effort set_config
    854 command_queued subagent-spawn-9f2bb565...-prompt prompt
    881 command_completed subagent-spawn-9f2bb565...-prompt {"type":"prompt","data":{"stop_reason":"QuotaLimit",...}}

The daemon then logged, repeatedly:

    session relay sync failed: transcript item "user:subagent-spawn-9f2bb565...-prompt" changed immutable identity fields


## Interfaces and Dependencies

In `mj-controller/src/controller/worker_binary/launch.rs`, define a borrowed history selector and the one rule. Exact names are the implementer's choice, but the shape must be:

    pub(in crate::controller) enum LaunchHistory<'a> {
        Stored,
        Restored(&'a mj_checkpoint::archive::CanonicalSessionSnapshot),
    }

    fn prompt_started(session_id: &str, history: &LaunchHistory<'_>, command_id: &str) -> Result<bool>

`session_launch_config` and the preparation functions above it take a `LaunchHistory` argument. In `mj-controller/src/database/materialized.rs`, add the stored-history query beside `load_prompt_acceptance`.
