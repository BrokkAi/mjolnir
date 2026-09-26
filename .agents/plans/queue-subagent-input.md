# Queue sub-agent input in the existing worker queue


This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture


An agent can call `send_input` immediately after spawning a child without receiving a startup error. The parent worker persists the request in its existing `subagents.json` queue and immediately returns a queue receipt. The daemon delivers it after initialization. `wait` and `list_agents` distinguish pending input and delivery failure from a completed previous turn. Interrupt affects only an active turn, never queued input.

## Progress


- [x] Inspected worker queue, daemon dispatch, startup, parking, interrupt, and projection paths.
- [x] Implement immediate worker acknowledgement and ordered deferred delivery.
- [x] Expose pending input and delivery results; implement targeted interrupt.
- [x] Validate startup, restart, close, ordering, replay, and handoff behavior in isolated tests.
- [x] Run workspace tests and Clippy in the dev profile.
- [ ] Commit, push to origin/master, and run scripts/install.sh.

## Surprises & Discoveries


The worker already persists pending requests and completed results. Completing a request removes it from the queue, so a queue acknowledgement must be a socket response rather than a call to `complete`. Startup currently rejects ordinary prompts while its configuration/first-prompt task runs. The sub-agent interrupt currently uses untargeted `CancelTurn`, although `CancelTurnFor` already exists. Worker command IDs are eventually pruned, so replay must also consult existing durable projections.

## Decision Log


The user selected immediate queue receipts for every send_input, including ready children, and explicitly rejected a second database queue. Reuse existing storage and protocol; no database migration or general lifecycle framework. Interrupt during startup reports no active turn without cancelling input. Close cancels delivery. The user authorized pushing to origin/master after validation and running scripts/install.sh afterward.

## Outcomes & Retrospective


Implemented queue acknowledgements after worker persistence, per-child delivery scheduling, startup readiness, stable prompt identities reconciled against existing projections, pending/failure visibility, and targeted interrupts. No database migration or relay protocol change was needed. Existing workers retain their old socket response behavior until upgraded; the daemon remains compatible with them.

Validation passed: `cargo test`, `cargo clippy --all-targets -- -D warnings`, and the focused controller runtime run (94 passed). The worker socket regression sends 128 KiB and proves acknowledgement precedes daemon completion and survives endpoint reopening. Controller regressions exercise startup ordering, close and startup failure, pending status through both public tools, active-turn interruption, and replay after active, completed, and historical projection recovery. The final full-suite rerun also passed after the last replay guard refinement. The implementation is ready for the authorized push and installation; their command results will be reported after this plan is committed.

## Context and Orientation


`mj-worker/src/worker_runtime/subagents.rs` owns the file-backed MCP request queue and socket replies. `mj-worker/src/subagent_mcp.rs` exposes those replies to the model. `mj-controller/src/server_runtime/run.rs` schedules requests observed in worker snapshots. `mj-controller/src/server_runtime/api.rs` executes them, owns startup follow-ups, and calculates child status. A relay acceptance ordinal identifies a delivered prompt; it is not available at queue admission. Existing database projections retain queued, active, and finished turns after worker journal collection.

## Plan of Work


### Milestone 1: Persistent admission and deferred delivery


First, acknowledge SendInput after queue persistence without completing it. Preserve cached completions and the existing response behavior of other tools. Then schedule only the oldest pending input per child, while allowing independent children and interrupt/close to run concurrently. Wait for startup configuration and the initial prompt before input delivery, reuse existing park/restart operations, and use a stable command ID across replay. Reconcile existing durable acceptance before resubmitting; report uncertain delivery rather than changing command identity.

### Milestone 2: Observable progress and interruption


Next, derive input progress from the parent's existing request/result snapshot. Pending deliveries keep wait from returning the previous report. Include child/request identity in failure results. Interrupt binds to the observed active prompt using CancelTurnFor; startup/parked/idle children return immediately. Close must prevent delivery after its admission.

## Concrete Steps


Work in `/home/jonathan/Projects/mjolnir` on the current master branch. Keep unrelated untracked files untouched. Use normal Cargo storage and mbx configuration. Run `cargo test` outside the restricted sandbox and `cargo clippy --all-targets -- -D warnings` in the dev profile. Use isolated automated stores and `--instance queued-subagent-input` for any new CLI instance. Commit only changed files; push the validated commit to origin/master and then run `scripts/install.sh` as requested.

## Validation and Acceptance


Use controlled fakes and real worker sockets to prove that queue acknowledgement returns while delivery is blocked; restarting the endpoint retains the pending request. Verify initial prompt before follow-ups, FIFO delivery per child, one restart for concurrent input, immediate targeted interrupt, close/startup failures, and no stale completion from wait. Replay a stable request after delivery and after durable projection recovery and confirm it does not produce a second prompt. Verify pending readiness does not hold daemon upgrade admission. Run all required workspace checks before committing.

## Idempotence and Recovery


The existing worker queue remains the durable owner. A daemon restart reconstructs pending work from worker snapshots. No new database writes or schema migration are introduced beyond existing prompt-progress recording. Live default-instance session data must not be used by test builds. The installed binary is updated only after successful validation and push.

## Artifacts and Notes


The observed production failure was `session initialization is still running`; a later manual agent retry succeeded. The desired first response is `{request_id, child_session_id, status: "queued"}`, explicitly not a turn acceptance.

## Interfaces and Dependencies


Keep SubagentToolRequest and SubagentToolResult envelopes compatible. Return the queue receipt as an ordinary socket result without inserting it into completed results. Completed SendInput payloads identify the child, original request ordering, and actual delivery outcome. Reuse existing CancelTurnFor, startup state, relay submissions, and database projections. Introduce small local scheduling/projection helpers rather than a new crate or broad lifecycle abstraction.

Revision note (2026-09-26): Recorded the implementation and passing validation. The dispatcher is a reconstructible scheduling cache; the worker queue remains the only owner of pending input. Startup readiness waits hold no upgrade admission. Existing transcript evidence without an acceptance ordinal fails visibly instead of permitting a duplicate prompt.
