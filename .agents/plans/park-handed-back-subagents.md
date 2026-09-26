# Park sub-agents whose turn has ended, so idle children hold no processes

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

A Mjolnir parent session can start sub-agents ("children"): further agent sessions that run inside the parent's own container and report back to it. Mjolnir creates that container with `--pids-limit=8192` (`CONTAINER_PIDS_LIMIT` in `mj-controller/src/targets/container.rs`), and every thread counts against that limit. Each live child keeps a harness process tree of hundreds of threads even when it has finished its work and is only waiting. GitHub issue #1161 records a parent that had twelve live children, ten of them idle after handing back their reports; the container ran out of process slots (`EAGAIN`, "os error 11") and the parent's own harness died, which cannot be recovered.

After this change a child whose turn has ended, and whose parent has been told so, is "parked": its worker process tree is stopped, but everything else about it stays (its session record, its relation to the parent, its target locator and worker root on disk with the relay journal and the harness's native session id, its conversation, and its report). A parked child holds no processes. When the parent calls `send_input` on it, Mjolnir starts its worker again in place, waits until the harness has loaded its old conversation and is idle, and then delivers the prompt. The concurrency cap (`subagents.max_concurrent`, default 6) now counts every child that holds processes, including idle ones, and a refusal tells the parent model which children are live and how to free a slot. When a spawn or a restart fails because the container is out of process slots, the parent is told that plainly, with the container's `pids.current` and `pids.max` when they can be read.

To see it working: start a parent with Mjolnir sub-agents in a named test instance, spawn a child, let it hand back, and observe that the child's row reads "Parked", that `list_agents` and `wait` report it with `"parked": true`, and that `ps` in the container no longer shows its harness. Calling `send_input` on it restarts it and the prompt runs.

## Progress

- [x] (2026-09-25) Read AGENTS.md, the issue, and the code paths named in the task.
- [x] (2026-09-25) Chose the representation: a new persisted `SessionState::Parked`.
- [x] (2026-09-26) Milestone 1: state variant, migration 53 (breaking, floor 53), daemon protocol 36, exhaustive matches, TUI and web viewer label "Parked" with the idle symbol, `Parked` excluded from `session_target_is_pollable` and from the actor's reconnect loop.
- [x] (2026-09-26) Milestone 2: `ensure_subagent_slot_available` counts `SessionState::has_live_worker()` children and lists them in its refusal; spawn and send_input share it.
- [x] (2026-09-26) Milestone 3: `Controller::park_subagent_worker` (`mj-controller/src/controller/subagent_park.rs`), `RuntimeState::park_subagent` (`mj-controller/src/daemon/subagent_park.rs`), called from the completion job in `server_runtime/run.rs` when no reminder went out and nothing was in flight.
- [x] (2026-09-26) Milestone 4: `Controller::start_installed_worker` factored out of `restart_worker_with_installed_binary`; `Controller::unpark_subagent_worker` and `RuntimeState::unpark_subagent`; `send_input` unparks after the cap check and resends a prompt a park turned away. `crate::targets::process_limit` classifies EAGAIN and writes the parent-facing message for spawn (the child's `last_error`) and unpark (the tool error).
- [x] (2026-09-26) Milestone 5: parent suspend and destroy remove parked children without starting them and count them as handed back; closing a parked child settles it `Stopped` through `stop_target_and_settle`, which now cleans up a child's borrowed worker; move refuses a parent with parked children as it does with live ones; recovery, pollers, credential sync and worker upgrades skip `Parked`.
- [x] (2026-09-26) Milestone 6: `wait` and `list_agents` add `"parked": true`; `interrupt` on a parked child answers without starting it; MCP tool descriptions and `docs/src/content/docs/sessions.md` and `configuration.md` updated.
- [x] (2026-09-26) Tests written; `cargo test` (5016 passed) and `cargo clippy --all-targets -- -D warnings` clean; isolated-instance daemon smoke (`mj --instance park1161`) migrated a fresh store to revision 53.
- [ ] Not done: a live end-to-end run with a real harness in a Podman container (park, `ps` in the container, unpark with a real `send_input`). It needs harness credentials in an isolated instance, which this host's notes say must not be pointed at the user's own harness homes.

## Surprises & Discoveries

- Observation: the session manager already drops an actor as soon as its session stops being "pollable" (`crate::pollers::session_target_is_pollable`), and the actor's own dead-worker recovery re-reads the durable record and stands down when that predicate is false. A state that predicate excludes is therefore skipped by reconnect, recovery and polling without any further check.
  Evidence: `mj-controller/src/session_manager/recovery.rs` `recover_worker_controlled` and `mj-controller/src/pollers/worker_targets.rs` `dashboard_worker_targets`.
- Observation: an actor never retires while a lease is out (`ActorLifecycle::should_stop`), and a lease returned to a retiring actor rejects every prompt queued during the lease with "session target is changing", a definite non-delivery. That is what makes a `send_input` racing a park safe to retry.
  Evidence: `mj-controller/src/session_manager/actor.rs`, the `Event::Returned` arm.
- Observation: closing a child through the no-checkpoint route (`stop_target_and_settle`) would have refused a container child, because `quiesce_plan` and `close_plan` refuse borrowed locators. The verified-close and force-destroy paths already special-case children with `borrowed_worker_cleanup_plan`; the settle path now does too.
- Observation: an actor that stopped with a submission still in its command channel dropped the reply, which the caller reads as "delivery unconfirmed". That would have made a `send_input` racing a park unrecoverable. The actor now answers every queued submission with a definite rejection when it stops.
  Evidence: `mj-controller/src/session_manager/actor.rs`, end of `run_session_actor`.
- Observation: `"os error 11"` is a prefix of `"os error 110"` (timed out) and `"os error 111"` (connection refused), so the classifier matches `"(os error 11)"`.

## Decision Log

- Decision: represent parking as a new persisted `SessionState::Parked`, not a marker on the sub-agent relation.
  Rationale: every piece of code that connects to, reconnects, recovers, polls or upgrades a session selects it by state (`session_target_is_pollable`, `Running | Disconnected` matches, the worker upgrade's `state == Running`). A new state is skipped by all of them by default, so a call site nobody updated fails safe. A marker beside a `Running` record would need every one of those sites to learn about it, and one missed site would reconnect to, or restart, a stopped worker. `Parked` counts as `is_active()` because a parked child is still on the dashboard and its parent's suspend, destroy and workspace close must still end it; `session_target_is_pollable` excludes it explicitly, and a new `has_live_worker()` names the states that hold processes.
  Date/Author: 2026-09-25, implementer.
- Decision: schema migration 53 rebuilds the `sessions` table to add `'parked'` to its `state` CHECK constraint, and is breaking: it raises the minimum compatible revision to 53.
  Rationale: an older daemon reads `sessions.state` with `SessionState::from_stored(...).expect(...)` and would panic on `"parked"`; the CHECK constraint also refuses the value until it is rebuilt. The rebuild follows the precedent of migration 32 (`migrate_zcode_harness_kind`).
- Decision: the daemon protocol goes to 36, because `SessionState` is on the wire to terminal clients.
- Decision: parking and unparking are daemon lifecycle operations (`LifecycleKind::Park`, `LifecycleKind::Unpark`), so they are serialized with close, suspend, destroy and each other by the existing per-session lifecycle map. Surfaces see a park as a sub-agent stop ("Stopping") and an unpark as a resume ("Resuming"), so no new wire lifecycle kind is needed.
- Decision: a park reuses the worker upgrade's idle admission (`IdleWorkspaceLease::acquire_for_upgrade` and `verify_for_upgrade`): it takes the actor's connection only when the worker reports it safe to replace, holds a worker-side idle barrier, stops the worker, records `Parked`, and keeps the lease until the session manager has dropped the child's target, so any prompt that arrived meanwhile is rejected, not delivered to a stopped worker. `send_input` retries once through an unpark when its prompt was rejected that way.
- Decision: closing a parked child settles it to `Stopped` without a checkpoint, by stopping and removing its private worker state. A child is never resumed on its own (see `mj-tui/src/resume.rs`), so it needs no archive, and making one would need its worker.

## Outcomes & Retrospective

A child whose turn ended, whose parent was told, and which has nothing queued is parked: its worker process tree is stopped and its record says `parked`. Its record, relation, target locator, worker root, conversation and report stay. `send_input` starts it again in place and then delivers the prompt; a failed restart leaves it parked and tells the parent why. The cap counts every child that holds processes, and its refusal names them. Process exhaustion is reported to the parent in plain words with the container's pid counts when they can be read.

Tests that prove the behavior:

- `controller::subagent_park::tests::parking_stops_an_idle_child_keeps_its_record_and_turns_away_a_racing_prompt` and `a_child_with_work_in_flight_is_not_parked` (stand-in relay and a real session manager).
- `server_runtime::api::tests::send_input_starts_a_parked_child_again_and_resends_only_a_prompt_a_park_turned_away`, `a_failed_restart_leaves_the_child_parked_and_tells_the_parent_why`, `wait_and_list_agents_report_a_parked_child_with_its_report`.
- `controller::subagents::tests::the_cap_counts_every_child_holding_processes_and_names_them`.
- `targets::process_limit::tests::*` and `controller::subagent_park::tests::only_a_full_target_is_rewritten_and_a_bare_one_reads_no_container_counts`.
- `daemon::tests::suspending_a_parent_removes_its_parked_sub_agents_without_starting_or_warning` and `closing_a_parked_sub_agent_stops_it_without_starting_its_worker`.
- `pollers::tests::a_parked_sub_agent_stays_out_of_live_target_pollers_and_startup_repair`, and `Parked` added to `session_manager::tests::stale_recovery_checks_durable_state_under_target_ownership`.
- `database::tests::the_parked_state_migration_keeps_every_session_and_refuses_older_builds`.
- `render::tests::a_parked_sub_agent_is_idle_and_reads_parked` (TUI), `server::tests::embedded_viewer_labels_a_parked_sub_agent_parked` (web), `subagent_mcp::tests::the_tools_explain_parked_children_and_the_live_child_limit`.

Remaining limits: only the parent's `send_input` starts a parked child; a prompt typed into it from the TUI, the web viewer or `mj prompt` fails as it would for any session without a live worker. A parked child's staged credentials are not refreshed until it runs again and the credential sync reaches it. A daemon that stops between a completion notice and the park leaves that child live until its next turn ends.

## Context and Orientation

The daemon (`mj daemon-run`, crate `mj-controller`) owns the database and every lifecycle operation. Each session has a worker process (crate `mj-worker`) on its target that runs the harness (Claude, Codex, ...). A sub-agent child borrows its parent's target: its `TargetLocator` names the parent's container with `borrowed_from` set, and its worker lives in its own worker root inside that container.

Key files:

- `mj-core/src/state.rs`: `SessionState` and its persisted spelling (`as_str`, `from_stored`), `is_active`.
- `mj-controller/src/database/schema.rs`: forward migrations; `mj-controller/src/database.rs` `SCHEMA_VERSION`.
- `mj-client/src/daemon.rs`: `PROTOCOL_VERSION`.
- `mj-controller/src/controller/subagents.rs`: registration and the concurrency cap (`subagent_occupies_slot`, `ensure_subagent_slot_available`).
- `mj-controller/src/server_runtime/run.rs`: the daemon's web/API loop; the sub-agent completion job sends the parent its completion notice.
- `mj-controller/src/server_runtime/api.rs`: the sub-agent tool actions (`spawn`, `list_agents`, `send_input`, `wait`, `close`) and `subagent_status`.
- `mj-controller/src/daemon/*.rs`: lifecycle operations (`start_or_join_lifecycle_controlled`, `run_lifecycle`), close and suspend (`close.rs`), the session manager target refresher (`process.rs`).
- `mj-controller/src/controller/worker_restart.rs`: in-place worker restart, used by checkpoint recovery and worker upgrades.
- `mj-controller/src/pollers/resources.rs`: `session_target_is_pollable`, which decides which sessions the session manager keeps an actor for.
- `mj-worker/src/subagent_mcp.rs`: the tool descriptions the parent model reads.
- `docs/src/content/docs/sessions.md`: user documentation of sub-agents.

## Plan of Work

Milestone 1 adds `SessionState::Parked` with spelling `"parked"`, migration 53, protocol 36, and fills every exhaustive match: it is idle for attention purposes, shows the idle symbol with the label "Parked" in the TUI and web viewer, and is excluded from `session_target_is_pollable` and from the actor's reconnect loop.

Milestone 2 changes `subagent_occupies_slot` to count `SessionState::has_live_worker()` states plus idle `Running` children, and rewrites the refusal to list the live children.

Milestone 3 adds `RuntimeState::park_subagent` in `mj-controller/src/daemon/subagent_park.rs`, reached from `ApiBackend` through `ExportRuntime::park_subagent`, and calls it from the completion job when no reminder went out and nothing was in flight. Failures are logged and leave the child live.

Milestone 4 factors the part of `restart_worker_with_installed_binary` after the stop into `start_installed_worker`, adds `RuntimeState::unpark_subagent`, and calls it from the `SendInput` arm after the cap check. A shared `process_limit` classifier recognises fork and thread exhaustion and rewrites the message for spawn and unpark failures.

Milestone 5 makes parent suspend, destroy, workspace close and child close handle parked children without starting them.

Milestone 6 adds `"parked": true` to `wait` and `list_agents` entries, updates the MCP tool descriptions and the docs.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir3`:

    cargo test
    cargo clippy --all-targets -- -D warnings

Both run outside the sandbox, because the suite uses loopback sockets.

## Validation and Acceptance

Behavior tests (named in the Outcomes section once written) prove: a child whose turn ends with its report delivered is parked and `wait` still returns its report with `parked: true`; `send_input` to a parked child unparks it and the prompt runs; the cap blocks a spawn and an unpark with N live idle children but ignores parked ones and names the live children; a failed unpark leaves the child parked and returns the error; the EAGAIN message, with and without readable pid counts; a parent suspend removes parked children without warning; daemon recovery leaves a parked child parked; migration 53 upgrades a store and refuses older readers.

## Idempotence and Recovery

Migration 53 checks whether the constraint already admits `'parked'` before rebuilding, so it can be re-run. A park that fails leaves the child live; an unpark that fails stops whatever it started and leaves the child parked.

## Artifacts and Notes

(Test transcripts are added as the work lands.)

## Interfaces and Dependencies

In `mj-core/src/state.rs`: `SessionState::Parked`, `SessionState::has_live_worker(self) -> bool`.

In `mj-controller/src/server_runtime/api.rs`, on `ExportRuntime`: `fn park_subagent(self: Arc<Self>, child: String) -> BoxFuture<'static, Result<bool>>` and `fn unpark_subagent(self: Arc<Self>, child: String) -> BoxFuture<'static, Result<()>>`.

In `mj-controller/src/controller/worker_restart.rs`: `Controller::start_installed_worker`, the steps of a restart after its stop.
