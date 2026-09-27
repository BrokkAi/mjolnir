# Use Jev turn decisions for both wait forms


This living plan follows `.agents/PLANS.md`.

## Purpose


`mj wait` and `mj wait --turn ID` must both respect the worker's Jev completion decision. An expected continuation is not a finished turn. A confident finished or needs-input verdict is a recorded outcome, including when residual background tasks remain. A verdict belongs to its completed command, so later unrelated activity cannot delay that completed target.

## Progress


- [x] (2026-09-26) Trace wait policy, worker verdict application, and durable relay snapshot.
- [x] (2026-09-26) Store worker completion decisions with command identity and publish them in operational snapshots.
- [x] (2026-09-26) Make both wait forms consume that record; add stale-verdict, restart, input, and continuation regressions.
- [x] (2026-09-26) Run formatting, full dev tests, and Clippy on the final implementation.

## Surprises & Discoveries


Jev currently stores only a process-local activity inference. `wait --turn` uses the physical harness outcome instead. The relay already persists worker snapshots; no controller database migration is necessary. Snapshot completion records must not restore inferred idle as evidence that it is safe to replace a worker.

## Decision Log


Use an optional completion record for the latest completed command in the worker snapshot. Physical completion initializes the record, and generation-validated Jev decisions update it. Retain the record when later prompts start; replace it when another physical completion is recorded, matching the existing latest-outcome wait interface. Include the transcript frontier in that record, so summaries include autonomous continuation rather than stopping at the earlier physical harness return. Publish it through the existing operational snapshot. Advance worker snapshot revision, with defaults for all older snapshots. Do not alter controller database or public v1 turn JSON.

Preserve needs-input in the shared Jev decision instead of collapsing it to inferred idle; both decisions still produce idle activity. Treat explicit terminal errors and cancellation as terminal independently of activity. Session waits additionally require session readiness; targeted waits can use a recorded verdict despite unrelated current activity.

## Context and Orientation


`mj-core/src/activity/verdict.rs` owns interpretation of Jev replies. `mj-worker/src/relay/verdict.rs` validates generation and applies them. `mj-core/src/relay/snapshot.rs` defines persisted and published worker state; `snapshot/apply.rs` records physical completions. `mj-controller/src/server/api/wait.rs` assembles observations and `wait_policy.rs` decides wait responses. The existing API uses the latest completed outcome whose acceptance ordinal meets the requested lower bound.

## Milestones and Work


First add a serializable command-bound completion decision to core and relay snapshots, initialize it on prompt completion, and persist accepted Jev decisions before publication. Cover restart durability without restoring process-local safety inference and stale generation rejection in worker tests.

Then carry this record into wait observations and use the shared decision for both explicit and implicit targets. Test ExpectContinuation, InferIdle, AwaitingInput, and a later active turn. Retain lifecycle, retry, and subagent report checks. Older workers without the record use shared activity and physical outcomes.

Finally run validation in the repository directory, using elevated Cargo tests with existing temporary isolated fixtures. Commit only this task's files, push the authorized upstream, and run `scripts/install.sh`.

## Validation and Acceptance


Run `cargo fmt --all -- --check`, `NO_COLOR= cargo test`, and `cargo clippy --all-targets -- -D warnings` in `/home/jonathan/Projects/mjolnir`. Focused tests should show both wait forms pending on ExpectContinuation and returning the same final or input outcome after a conclusive decision. Worker tests must show stale generations do not change the recorded outcome and reopening preserves the outcome but not inferred process idleness. Existing isolated upgrade tests must pass.

## Recovery and Interfaces


The new optional worker snapshot record is initialized for old snapshots. No database or event-journal schema changes are needed. Existing snapshot writes are atomic. Failed tests or installation can be retried; never point a test binary at the live default instance. No new dependencies.

## Outcomes and Retrospective


Both wait forms now consume command-bound Jev decisions, including needs-input and expected continuation, and summaries include the assessed continuation. Records survive worker restart without reviving process-local idle inference. Final full dev tests, Clippy with warnings denied, formatting, and diff checks passed. No database migration is needed. The first build exposed explicit operational-state literals in test fixtures; these now initialize the optional record. Delivery proceeds through the authorized commit, push, and installation.

## Artifacts


Preliminary controller API tests passed (106 tests) before the explicit-turn correction. They are not validation of the final behavior.

Revision: initial plan records the user correction that explicit targets must also honor Jev.

Revision: preserve needs-input in the shared decision and persist the verdict before clearing retry assessment. A crash therefore leaves either the verdict or a resumable assessment, rather than a false completed boundary.

Revision: the worker restart regression initially used invalid short command IDs; corrected it to protocol-valid IDs. The focused regression now passes. Extended the completion record with its transcript frontier and queued final full tests and Clippy.

Validation note: the full run before the summary-frontier extension passed. The final-source run passed all new regressions but had three unrelated checkpoint timeouts and a bridge-exit message race during overlapping validation jobs; all four passed individually with the final test binary. A non-overlapping full rerun is now in progress. Final-source Clippy and formatting passed.

Final validation: `NO_COLOR= cargo test` exited 0 with the final source in a non-overlapping run (`/tmp/mj-jev-completion-clean-tests.log`). Final-source Clippy exited 0 (`/tmp/mj-jev-completion-clippy.log`); formatting and diff checks passed. No implementation work remains.
