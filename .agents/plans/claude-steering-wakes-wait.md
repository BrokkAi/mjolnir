# Make Claude steering wake a subagent wait

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture

A user correction sent while Claude is waiting for Mjolnir subagents must reach the parent promptly, while the children continue working. The worker owns accepted steering and must return its outstanding read-only wait so Claude can run the PostToolBatch hook that delivers the correction.

## Progress

- [x] (2026-10-11) Diagnosed session 96f321a675e77a6fcd31abbcc11344d7: wait started at 22:01:27 UTC; steering accepted at 22:24:48 UTC with no hook delivery.
- [x] (2026-10-11) Implemented worker-owned wakeup and ordering regressions; all 13 focused steering tests passed.
- [x] (2026-10-11) Extended the isolated live Claude test: installed baseline fails; fixed build acknowledges in 1.99 seconds before child completion and collects the later report.
- [x] (2026-10-11) Completed touched-crate validation, workspace clippy, formatting, Python compilation, and final diff review. Commit and upstream push follow on the current branch.

## Surprises & Discoveries

Claude steering bypasses ACP and its acknowledgment timer. `mj-agents.wait` can run for 3,600 seconds. The daemon observes parent changes but only returns for child reports, exhaustion, or timeout. Existing Claude live tests accept eventual delivery and have no subagent wait scenario.

## Decision Log

- Decision: keep delivery eligibility in DurableRelay and wake the worker socket wait from relay publications.
  Rationale: the worker owns both the active turn and hook delivery. The decision remains available during daemon replacement; no daemon-side copy of the steering predicate is needed.
  Date: 2026-10-11.
- Decision: serialize worker completion and daemon completion under the existing queue lock when yielding for input.
  Rationale: a racing daemon result is returned if already available; otherwise the worker durably completes and removes the wait request. A later daemon result is reported undelivered so child reports remain collectible. The daemon observes that worker-owned completion and retires its own wait. Existing persisted result and delivery fields suffice; no format or protocol change is necessary.
  Date: 2026-10-11.

## Outcomes & Retrospective

The installed 2.41.0+ddcaa16b build fails the new live scenario after 60 seconds, with the child still running and the correction pending. The first 13 focused worker regressions passed; durable retirement was then added after review found that yielding without completing the request would leave old daemon waits occupying capacity. The revised 13 worker regressions and the daemon retirement regression pass. Workspace `cargo clippy --all-targets -- -D warnings` passes. The fixed live run passes with acknowledgment in 1.987 seconds during the original turn while the child remains running; the later child report is collected and the correction appears once. Full touched-crate validation is complete: worker suite 704 passed / 11 ignored; controller 1,984 passed / 10 ignored with one unrelated mbx cleanup deadline failure that passed its isolated rerun. No changes were made to that test.

## Context and Orientation

`mj-worker/src/relay/commands.rs` selects a pending Claude steer for a PostToolBatch hook. `mj-worker/src/relay/journal.rs` publishes durable state changes. `mj-worker/src/worker_runtime/subagents.rs` serves the MCP socket and owns a LiveWaiter whose removal is serialized with daemon result completion. `tests/e2e/steer_live.py` starts a separate daemon with copied credentials and private configuration/data, and records API evidence.

## Plan of Work

First expose relay change notifications and a query that reuses the hook's eligibility decision and matches the wait's originating turn. Subscribe before inspecting readiness, and return only read-only waits when a steer can be delivered. Serialize waiter removal with result completion using the existing queue lock. Leave mutations, attachment/slash-command queues, unsupported delivery modes, and stale turns alone.

Then extend socket-level ordering regressions to prove wakeup before and after wait registration, preservation of early/late child reports, and hook delivery exactly once. Extend the live harness with a parent blocked in wait on a real child whose completion is held until the test releases it. Require prompt acknowledgment while the child is still unfinished, and test isolated daemon replacement while the parent waits if feasible.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir4`, use the configured mbx Cargo wrapper without changing target storage. Run focused worker regressions with elevated `cargo test -p brokk-mj-worker <filter>`. Build matching dev CLI/worker binaries with `cargo build -p brokk-mjolnir -p brokk-mj-worker`. Run `python3 tests/e2e/steer_live.py --help` for isolated live arguments. Final validation is `cargo test -p brokk-mj-worker -p brokk-mj-controller` and `cargo clippy --all-targets -- -D warnings`, plus Python compilation and diff checks.

## Validation and Acceptance

The new socket regression must fail against the original implementation because the wait remains open. The live Claude parent must acknowledge a unique correction within a bounded interval while its child remains running; the original wait must end without cancelling either turn or child. The correction must appear once, and the child's eventual report must remain collectible. All new binaries and daemons use a separate named instance and copied profile homes.

## Idempotence and Recovery

No migrations or changes to live sessions are needed. Test cleanup stops owned processes before removing runtime files. Commit only changed files to the current branch and push the configured upstream, merging upstream if it advanced.

## Artifacts and Notes

Keep live transcripts and traces in task-specific directories under `/tmp`. Baseline evidence is `/tmp/mj-steer-wait-before2-1011/claude-subagent-wait-api-evidence.json`. Focused results: `/tmp/mj-steering-focused-final.log` (13 passed), `/tmp/mj-steering-controller-focused.log` (1 passed). Clippy: `/tmp/mj-steering-clippy.log` (passed).

## Interfaces and Dependencies

Use Tokio notifications and the existing DurableRelay and SubagentEndpoint owners. Do not add a crate, change Cargo cache configuration, or introduce another steering delivery path.

Revision: implemented a relay watch subscription and reused the existing atomic waiter removal, with regressions for early/late input and early/late reports. The first fixture runs exposed invalid short command IDs and an admission-observation race; those were corrected before evaluating production behavior.

Revision: retire the request durably as part of yielding and let the daemon return the worker-owned result. This prevents repeated steering from accumulating hour-long daemon waits. Add a controller persistence regression proving the retired wait leaves a ready child report collectible. Baseline live evidence: `/tmp/mj-steer-wait-before2-1011/claude-subagent-wait-api-evidence.json`.

Revision: live fixture cleanup exposed Podman creating `overlay/tempdirs` during namespace startup. Enumerate children inside the namespace, let ordinary runtime teardown remove host-owned overlay metadata after Podman exits, and retain all existing process and mount checks. Both baseline runtimes were then removed successfully after verifying their sessions and processes were gone.

Live command: `python3 tests/e2e/steer_live.py --run-live --mj target/debug/mj --worker target/debug/mj-worker --claude-home /home/jonathan/.claude4 --codex-home /home/jonathan/.codex2 --harness claude --scenario subagent-wait --instance steer-wait-after-1011 --artifacts /tmp/mj-steer-wait-after-1011`. Credentials were copied, not used in place. Evidence: `/tmp/mj-steer-wait-after-1011/claude-subagent-wait-api-evidence.json` and `trace.jsonl`. The socket regression runs with no daemon, and the controller regression supplies the persisted worker completion to a fresh backend; no additional real daemon-restart run was needed.

Second fixed live run: same arguments with instance `steer-wait-after2-1011` and artifacts `/tmp/mj-steer-wait-after2-1011`; exit 0. Acknowledgment 2.554 seconds, original turn retained, child still running, later report collected, correction once, all isolated runtime files removed. The first fixed run passed behavior but exited nonzero because Podman recreated `tempdirs` at exit; cleanup now lets normal host-owned teardown remove that metadata.

Final validation: `/tmp/mj-steering-full-tests.log` (controller suite, one deadline flake), `/tmp/mj-steering-controller-retry.log` (that test passed alone), `/tmp/mj-steering-worker-full.log` (all worker suites passed). Workspace clippy, `cargo fmt --all -- --check`, `python3 -m py_compile tests/e2e/steer_live.py`, and `git diff --check` passed. No live default-instance worker was replaced; installing the new worker build is outside this push request.
