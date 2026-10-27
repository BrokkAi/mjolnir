# Use AIR as the background-task owner

Maintain this plan under `.agents/PLANS.md`.

## Purpose

Claude and Codex background commands, including child-session commands, must appear in the same inventory and support stopping through the adapter that owns them. AIR is the adapters’ async task notification and request extension. Hosted terminals remain independently owned by the worker.

## Progress

- [x] 2026-10-27: Read adapter contracts and trace notification, relay and replay paths.
- [x] 2026-10-27: Replaced provider-specific inventories with an AIR task map; scoped Claude stop acknowledgements by owning session.
- [x] 2026-10-27: Converted adapter, relay, capacity, verdict and client activity fixtures.
- [x] 2026-10-27: Full core suite passed (550 tests, 3 doctests); full client suite passed (42 unit tests, 1 integration test).
- [x] 2026-10-27: Affected AIR inventory/adapter tests passed (4); stop acknowledgement regressions passed (3).
- [x] 2026-10-27: Full worker library run completed (692 passed, 8 load-sensitive failures, 10 ignored); all eight exact reruns passed. Remaining worker targets passed (4 binary tests and 19 integration tests). Worker doctests had no cases. Workspace clippy passed with warnings denied.
- [x] 2026-10-27: Prepared validated changes for the required current-branch commit.

## Surprises & Discoveries

RuntimeEvent is deserialized by transcript materialization and chat events; the old task shapes must remain readable. BackgroundTaskStopTarget has no serialization and never crosses the relay wire. Session load emits native-agent ReplayBegin/ReplayCommit, and snapshot.native_agent_replay identifies that bracket.

## Decision Log

Use one map keyed by session and task identity. The owning session travels with each task and stop target. Preserve the old RuntimeEvent variants solely for historical decoding; never produce or apply them. Exclude taskType turn. Only Claude’s harness turn policy anticipates a task-notification turn after completion/failure, and never during replay. These implement the parent’s accepted design.

## Implementation milestones

First generalize the ACP decoder and stop dispatcher in mj-worker/src/acp/air_tasks.rs, route child task notifications before native transcript decoding, and publish one neutral RuntimeEvent. Replace relay policy selection and map ownership in mj-worker/src/relay.rs and relay/background.rs. Keep Claude’s raw result subscription and stop acknowledgement pairing.

Then convert existing relay tests to explicit AIR task events, and convert the child-update suppression test into a fake ACP adapter integration in acp/tests.rs that stops root and child tasks for each harness. Update the autonomous-turn agent documentation.

## Validation and Acceptance

From repository root run affected cargo test -p mj-worker filters, then full cargo test -p mj-worker and cargo test -p mj-core, and workspace cargo clippy --all-targets -- -D warnings. Run cargo tests with escalated permissions, on the dev profile and existing mbx build layout. Fake adapters use isolated duplex streams and temporary relay directories. All task stops must address the owning session; replay must neither schedule notification turns nor lose live tasks. Store text logs in the assigned report directory. Rerun only failed tests after fixes.

## Recovery and scope

Do not change session.rs, goal logic, or the other agent’s drive.rs failure handling. Do not redirect Cargo output or run the default daemon. Stage only this task’s files and commit on master, without pushing.

## Outcomes & Retrospective

Implementation and validation are complete. The fake adapter must advertise Codex’s `agent` execution mode as well as Claude’s `auto`; the replay fixture enables turn tracking only after its tool-card negative control. Runtime task events remain process-local; legacy payload decoding remains compatible. No chat dialog rendering changed. The full worker run’s seven progress timeouts and one delayed Node lease release all passed when rerun individually; no production changes were made for those failures. All requested checks have completed.

2026-10-27 update: child task ownership also requires child-scoped Claude stop acknowledgement pairing; root and child pending stops now have separate sets. Package names are brokk-mj-worker, brokk-mj-core and brokk-mj-client, despite the directory names.

2026-10-27 completion: AIR root/child inventory and stopping are implemented, legacy runtime records remain readable, and all failed full-suite cases passed exact reruns. Commit only the task files and shared notification/task hunks; the other agent’s AIR alignment plan remains outside this change.
