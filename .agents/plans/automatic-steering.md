# Steer queued prompts into the running turn automatically

This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective throughout implementation.

## Purpose / Big Picture

Today a prompt typed while an agent is working waits in the session's queue until the running turn ends. The user must press Escape to push it into the running turn ("steering"). After this change the worker steers a queued prompt into the running turn by itself whenever the harness can take it without interruption risk to the turn: the harness must support steering and must hand the prompt back, instead of starting a turn of its own, when no turn can take it. The user sees "Steering…" and then the prompt appears inside the running turn. Nothing is cancelled. Escape keeps working as before.

The same change adopts the Codex bridge release that makes this safe for Codex (`@brokkai/codex-acp` 1.13.2 with Codex 0.156.1), repairs `mj prompt --wait` and `mj wait --turn` for prompts that were steered, and reads the new layout of Codex's "Other" answer field in structured questions.

To see it working: start a session in an isolated instance, send a prompt that makes the agent run for a minute, send a second prompt with `mj prompt --wait` while the first runs, and observe that the second prompt appears in the running turn's transcript within seconds and that the `--wait` call returns when that turn ends.

## Progress

- [x] (2026-09-24) Released `@brokkai/codex-acp` 1.13.2, which honors `_meta.steering.idleBehavior: "promptRequired"` and advertises `_meta.steering.idleBehaviors: ["promptRequired"]`. Live guardian and yolo sessions passed with the packed adapter before publication.
- [x] (2026-09-24) Milestone 1: pinned `@brokkai/codex-acp` 1.13.2 and Codex 0.156.1 in `harness_runtime.rs`, the bridge package files, the agent-dev Containerfile, and the reliability lab; paired Codex's `user_note` field with its question. The regenerated lockfile drops the nested second Codex copy that the old bridge's own 0.153.4 pin required.
- [x] (2026-09-24) Milestone 2: a `promptRequired` reply becomes `RelayCommandOutcome::SteeringReturned` and resolves the steering operation quietly; `MaterializedTurn.steered_into` keeps the turn's identity through a steer, so waits finish and interrupted steered turns clear; migration 48 (breaking) records both. Tests: `a_returned_steer_keeps_the_prompt_queued_for_the_next_turn`, `returned_steering_leaves_the_prompt_to_mj_without_cancellation`, `a_steered_prompt_finishes_with_the_turn_it_joined`, `an_interrupted_steered_turn_clears_the_running_turn`, `a_returned_steer_leaves_the_prompt_queued_and_the_turn_running`, `steering_migration_refuses_builds_that_cannot_read_returned_steers`.
- [x] (2026-09-24) Milestone 3: `RuntimeEvent::Connected.steering_returns_idle_input` (from `steering_returns_idle_input` in `mj-worker/src/acp/drive.rs`) enables `DurableRelay::set_automatic_steering`; `claim_pending_commands_up_to` calls `submit_automatic_steer`. Tests: `queued_prompts_are_steered_into_the_running_turn_one_at_a_time`, `automatic_steering_stops_for_a_turn_that_returned_a_steer`, `slash_commands_and_waiting_checkpoints_are_not_steered`, `bridges_that_start_their_own_turns_are_not_steered_automatically`, `a_path_is_a_message_and_a_command_name_is_not`, `only_bridges_that_return_idle_steers_are_steered_automatically`. Milestones 2 and 3 share files (`mj-core/src/acp.rs`, `mj-worker/src/acp/drive.rs`, `dispatch.rs`), so they are one commit.
- [x] (2026-09-24) Milestone 4: validated live in the isolated instance `steer-1132` with Codex (`codex4`) and Claude (`claude2`); evidence under Artifacts and Notes. The full default Cargo suite, clippy with `-D warnings`, and formatting passed.

## Surprises & Discoveries

The Codex bridge used to drop the steering `_meta` and start a new Codex turn when no turn could take the message. It replied `startedNewTurn`, which mj treated as unconfirmed delivery: it held the prompt and asked the user to review it, although Codex was already running it in a turn that belonged to no mj prompt. Codex 1.13.2 fixes this in the bridge.

`mj prompt --wait` and `mj wait --turn N` never finish for a prompt that was steered. The transcript projection (`mj-transcript/src/projection/observation.rs`) moves the running turn to the steered prompt, and then the completion of the original prompt looks for an active turn with the original command ID, finds none, and records an outcome with no accepted ordinal. The wait rule in `mj-controller/src/server/api/wait_policy.rs` needs an accepted ordinal at or after its target. The same filter means an interrupted steered turn is never cleared from the projection. Escape steering already had this bug; automatic steering would hit it constantly.

A Claude steer is delivered at the SDK's `now` priority and aborts the model response in progress; the turn continues with the new message. The bridge comment says it slots in between tool calls; that detail is not verified. This is the same behavior Escape steering has today, and it is not a cancel: tools, subagents, compaction and the turn itself continue.

A lifecycle `destroy` accepted just before `mj daemon stop` did not finish: after the next daemon start the session came back `running` with a new worker, and a second `destroy` with the daemon left running archived it. Seen for five lab sessions in two instances on 2026-09-24. This is unrelated to steering and was not investigated further.

The `claude4` account was at its session limit during validation; the Claude run used `claude2`.

## Decision Log

2026-09-24: The worker, not the clients, decides to steer. It owns the queue, the one-steer-at-a-time rule, and uncertain-delivery state, and it keeps running when no client is attached. A worker-generated `RelayCommand::Steer` with an `auto-steer-` command ID goes through `DurableRelay::submit_command`, so it is validated and journaled exactly like an Escape steer.

2026-09-24: Steer automatically only when the harness advertises steering and returns idle input. Codex bridges advertise `_meta.steering.idleBehaviors` containing `promptRequired`. The pinned Claude bridge (`claude-agent-acp` 0.81.0) honors `promptRequired` in its `steer` method but does not advertise it, so Claude is treated as supporting it. Harnesses that advertise steering without returning idle input keep today's behavior: prompts queue and Escape steers.

2026-09-24: A `promptRequired` reply becomes a new outcome, `RelayCommandOutcome::SteeringReturned { queued_command_id }`. The steering operation becomes `Resolved`, the prompt stays at the head of the queue, and it runs as the next prompt. No dialog is shown, for Escape or automatic steering, because the turn it targeted has already ended. Reusing a rejection would have shown the "could not be steered" dialog for a normal race.

2026-09-24: Do not retry automatic steering against the same running prompt after a steer for that prompt failed or was returned. Continue with the next queued prompt after a steer is applied.

2026-09-24: Slash commands and context commands (`/compact`) are never steered automatically; they run at turn boundaries as before. A slash command is a first text block whose first word starts with `/` and contains only letters, digits, `_`, `-` or `:`, so `/home/me/file` is not one.

2026-09-24: Automatic steering is suppressed while a checkpoint or close barrier is admitted or queued, while a cancellation is pending, and in checkpoint-only mode, because those states are waiting for the turn to end.

2026-09-24: `MaterializedTurn` gains `steered_into`, the relay prompt command whose execution the turn continues. Completion and interruption of that prompt now match the active turn by either identity. The outcome keeps the original command ID and takes the steered prompt's accepted ordinal, which satisfies waiters for both prompts.

2026-09-24: Migration 48 is breaking. Stored relay JSON can contain the new outcome, and stored `active_turn_json` can contain `steered_into`, which older readers reject because `MaterializedTurn` denies unknown fields.

2026-09-24: The terminal chat's palette "interrupt turn" command now treats a steer in progress from any source as pending turn control (`turn_control_pending` in `mj-chat/src/chat/status.rs`), matching what Escape already did in `mj-chat/src/chat/keys.rs`. Without it, interrupting during an automatic steer would send a second steer that admission rejects.

2026-09-24: Codex's new "Other" field has `_meta.codex.role: "user_note"` and `questionId`, where 1.11.5 had `isOtherAnswer: true`. mj pairs it with its question through the existing `custom_answer_for` link with no `custom_answer_option`, so the note replaces the choice, as the old "Other" field did. Coupling to the bridge's "None of the above" label was rejected.

## Outcomes & Retrospective

Queued prompts now join the running turn without Escape for Codex (bridge 1.13.2 and later) and Claude, and `mj prompt --wait` finishes for a steered prompt when its turn ends. Harnesses that do not return idle steers keep the queue-then-Escape behavior. The race in which a steer arrives just as a turn ends was not reproduced live; unit tests cover it at the bridge (`returned_steering_leaves_the_prompt_to_mj_without_cancellation`), relay, and projection levels. On Claude a steer still aborts the model response in progress, as Escape steering already did; the turn, its tools and its subagents continue.

## Context and Orientation

A session runs one worker process (`mj-worker`). The worker drives the harness through an ACP bridge, a subprocess that speaks the Agent Client Protocol for Claude Code or Codex. The worker keeps a durable journal of relay events; `mj-core/src/relay/snapshot.rs` defines the commands (`RelayCommand`), the journal observations (`RelayObservation`), command outcomes (`RelayCommandOutcome`), and the steering state shown to clients (`SteeringOperation` with `SteeringStatus`). `mj-core/src/relay/snapshot/apply.rs` replays observations into the snapshot deterministically. `mj-worker/src/relay/commands.rs` admits commands (`submit_command`), validates turn controls (`validate_turn_control`), and claims pending commands for dispatch (`claim_pending_commands_up_to`). `mj-worker/src/worker_runtime/unix/dispatch.rs` hands claimed commands to the ACP loop and records runtime events back into the relay. `mj-worker/src/acp/session.rs` runs the ACP loop; it negotiates steering on `initialize` through `steering_supported_from_meta` in `mj-worker/src/acp/drive.rs`, starts a steer with `start_steer`, and settles the reply in `settle_steer`.

A steer is the ACP extension request `_session/steering` with `{sessionId, prompt, _meta: {steering: {idleBehavior: "promptRequired"}}}`. Replies are `{outcome: "injected"}` (the text joined the running turn), `{outcome: "promptRequired", reason: "noRunningTurn"}` (no turn could take it; the client keeps it), `{outcome: "failed"}`, or `startedNewTurn` from bridges that ignore the option.

The daemon (`mj-controller`) mirrors the worker's journal into SQLite through the shared projection in `mj-transcript/src/projection/observation.rs`, which builds `MaterializedTurn` (the running prompt) and `MaterializedTurnOutcome` (the last finished prompt), both defined in `mj-core/src/state.rs`. `mj wait` and `mj prompt --wait` resolve against those records in `mj-controller/src/server/api/wait_policy.rs`. Schema migrations live in `mj-controller/src/database/schema.rs`, with the version in `mj-controller/src/database.rs`.

Clients show steering from `SteeringOperation`: the terminal chat in `mj-chat/src/chat/turn_control.rs` and the web viewer in `mj-controller/src/web/viewer.js`. Neither needs a change for automatic steering, because automatic steers use the same command and states.

Harness pins live in `mj-core/src/harness_runtime.rs`, the bridge package files in `mj-worker/assets/harnesses/codex/`, the container image in `containers/Containerfile.agent-dev`, and a lab fixture in `tests/e2e/reliability_lab.py`. Structured questions are parsed in `mj-core/src/elicitation.rs` (`parse_field`).

## Plan of Work

Milestone 1 changes the Codex pin to `@brokkai/codex-acp` 1.13.2 and Codex 0.156.1 in all five places, regenerating `mj-worker/assets/harnesses/codex/package-lock.json` with npm from the registry. In `mj-core/src/elicitation.rs`, a Codex field whose `_meta.codex` has `role: "user_note"` and a `questionId` becomes a custom answer for that question, like `isOtherAnswer: true`. A unit test parses the 1.13.2 form shape.

Milestone 2 adds `RelayCommandOutcome::SteeringReturned { queued_command_id }`. `settle_steer` emits a new `RuntimeEvent::SteerReturned` for a `promptRequired` reply; `dispatch.rs` records it as `CommandCompleted` with the new outcome; `apply.rs` marks the matching steering operation `Resolved` and leaves the queue alone. The transcript projection accepts the new outcome without changing the queue. `MaterializedTurn` gains `steered_into: Option<String>`; the `Steered` projection sets it to the running prompt's identity; prompt completion, rejection and interruption match the active turn by `command_id` or `steered_into`. Migration 48 records the breaking change.

Milestone 3 adds automatic steering. `RuntimeEvent::Connected` carries whether the bridge returns idle input; `DurableRelay` keeps it next to `steering_supported`. At the start of `claim_pending_commands_up_to`, after stale controls are rejected, the relay submits a steer when every condition in the Decision Log holds and the queue head is a plain prompt in the `Queued` dispatch state.

Milestone 4 validates in the isolated instance `steer-1132` with real Codex and Claude profiles and records evidence here.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir3` on the current branch. Run focused tests while implementing, then `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and the affected packages' tests outside the sandbox, for example:

    cargo test -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-worker -p brokk-mj-controller

For the live check, build `cargo build -p brokk-mjolnir -p brokk-mj-worker`, write `~/.config/mjolnir/instances/steer-1132/config.toml` with copies of a Codex and a Claude profile, and drive sessions with `./target/debug/mj --instance steer-1132 new|prompt|wait|transcript`.

## Validation and Acceptance

Unit tests must show: a returned steer resolves the steering operation and keeps the prompt queued; a steered prompt's `--wait` target finishes when the turn ends; an interrupted steered turn clears the active turn; the relay submits an automatic steer only when every condition holds and never twice against the same running prompt after a failure or return; slash commands are not steered; the 1.13.2 note field pairs with its question; the migration ledger reaches 48 with minimum compatible 48.

Live acceptance: with Codex and with Claude, a second prompt sent during a long turn is steered without Escape and without cancelling the turn, `mj prompt --wait` for it returns when the turn ends, and a prompt sent just as a turn ends runs as the next turn with no dialog.

## Idempotence and Recovery

All live testing uses the isolated instance and a cache directory under the session scratchpad, set through the profile environment `XDG_CACHE_HOME`, so the user's daemon and harness cache are untouched. The migration runs only against isolated test stores until the user upgrades. Rerunning the steps is safe.

## Artifacts and Notes

Release evidence for 1.13.2 (2026-09-24): a guardian session wrote in its workspace on two successive turns without approval, and a sandbox-blocked `curl` was escalated through Codex's automatic review ("Guardian" tool entry) and returned 200 with no question to mj. A yolo session over SSH ran network access and a write to `$HOME` with no approval.

Live validation (2026-09-24, build of this branch, instance `steer-1132`, cache under the session scratchpad):

    Codex, session 2e772c58: first prompt ran `sleep 40`; the follow-up sent during it was
    admitted as auto-steer-1dd595d3… and applied. Transcript: [29] agent, [30] tool (sleep),
    [34] user "Also add a second line…", [49] agent "FIRST-DONE / SECOND-DONE Thursday".
    `mj prompt --wait` for the follow-up: "finished (EndTurn) turn 31 in 44.3s".
    The managed install came from npm: codex-acp-1.13.2.tgz in the committed lockfile.

    Claude, session 1821335f: auto-steer-1cfb15c5… applied; [21] user follow-up inside the
    turn. Claude Code moved the 40-second sleep to the background and ended the turn;
    `mj prompt --wait` returned "finished (EndTurn) turn 18 in 6.3s". Its next cycle answered
    "FIRST-DONE … / SECOND-DONE: Today, 2026-09-24, is a Thursday."

## Interfaces and Dependencies

New or changed interfaces, all in existing crates:

    mj_core::relay::RelayCommandOutcome::SteeringReturned { queued_command_id: String }
    mj_core::acp::RuntimeEvent::SteerReturned { request_id: String, queued_command_id: String }
    mj_core::acp::RuntimeEvent::Connected { .., steering_returns_idle_input: bool }
    mj_core::state::MaterializedTurn { .., steered_into: Option<String> }
    mj_core::acp::prompt_is_slash_command(prompt: &[ContentBlock]) -> bool

Plan written 2026-09-24 from the conversation in which the user asked for automatic steering, the Codex bridge fix, and its release.
