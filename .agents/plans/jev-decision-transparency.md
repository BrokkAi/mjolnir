# Make Jev decisions inspectable

This living ExecPlan follows `.agents/PLANS.md` and implements the user-approved transparency plan. The user also explicitly requests pushing the completed commits to origin/master from the current branch.

## Purpose / Big Picture

Operators should see when Jev influences session activity or resumes unfinished work, then inspect what mj checked, what Jev returned, and what mj actually did. Normal session use gets brief attribution; a shared Jev decisions inspector exposes pending, applied, unchanged, uncertain, cancelled, stale and failed checks. Exact bounded request bodies are retained in rotating local diagnostic logs, not a database or permanent archive.

## Progress

- [x] Inspected current classifier, continuation, logging and UI paths; confirmed clean current branch hel2.
- [x] Shared decision records and rotating background log writers/readers.
- [x] Instrument running/replied worker checks and daemon continuation, preserving policies.
- [x] Read-only worker/controller access and live attribution.
- [x] TUI and web inspectors with expandable technical evidence.
- [x] Behavior tests, isolated acceptance, required checks, documentation and final diff review. Commit on hel2 and push to origin/master follow this validation update.

## Surprises & Discoveries

Current worker logging writes stderr to worker.log and daemon logs retain files per process incarnation rather than rotating by size. A dedicated shared structured diagnostic log is needed for bounded per-owner retention without changing all logging. Existing classifier results contain classifications and scores, not prose explanations; explanations must be deterministic descriptions of those results and application guards.

The live `jev_decision_id` stays on worker operational snapshots and viewer session snapshots, outside persisted API activity JSON. This avoids a database compatibility change and prevents diagnostic IDs from producing activity events.

A continuation submission can be accepted before its live view update is processed. Once dispatched, its supervised 15-second task retains responsibility for recording acceptance or rejection, even if a new live view removes the pending UI check. The worker’s existing atomic frontier guard remains authoritative.

## Decision Log

2026-09-20: The user selected concise visible changes plus detail access. Exact inputs may be retained in rotating logs; there is no requirement for permanent evidence retention. Use 8 MiB segments and four retained segments per owner. Preserve all existing inference thresholds, context windows and continuation limits. Logging and inspection must not mutate classifier evidence or activity generations.

## Outcomes & Retrospective

Implementation and validation are complete. Operators can inspect activity and continuation checks from both UI surfaces, with exact inputs retained only in rotating logs. Classifier policies and database compatibility are unchanged. Commit and the explicitly requested push are the final publication steps.

## Context and Orientation

`mj-worker/src/acp/verdict_client.rs` issues running and replied activity requests; `mj-worker/src/relay/verdict.rs` applies replied assessments. `mj-controller/src/daemon/continuation.rs` gates automatic continuation and review. The controller and worker use the relay protocol in mj-core for remote reads. TUI command and modal infrastructure lives in mj-tui; the web UI lives in `mj-controller/src/web/viewer.js`. Shared record/log behavior belongs in mj-core without adding a crate.

## Plan of Work

Add a versioned shared decision record containing identity, session, check kind/phase, timing, exact request, evidence scope, classifier contract, returned scores, decision thresholds and actual application outcome. A background writer appends JSON lines and rotates four 8 MiB segments. Readers operate off UI loops and serve summaries separately from exact input. Record missing/rotated history honestly.

Instrument both activity paths and automatic continuation at request start, response and final disposition. Correlate all phases by ID. Attribution reflects applied results only and clears when runtime facts supersede them. Automatic notices explain that Jev assessed remaining work as already requested. Log failure is reported and never affects whether the classifier is allowed to act.

Expose read-only session decision list/detail access through a capability-gated worker request and authenticated controller APIs. The controller combines its own continuation log with the active worker's activity log; unsupported/unreachable sources are reported explicitly. UI surfaces load details on demand in supervised tasks. Add a TUI palette command and web session-menu entry, with status attribution and continuation-notice entry points.

## Concrete Steps

Work in /home/jonathan/Projects/hel2. Run focused behavior tests during implementation. Run cargo test outside the sandbox, cargo clippy --all-targets -- -D warnings, cargo fmt --all -- --check, relevant web/proxy tests, and git diff --check. Use --instance jev-transparency with isolated configuration/data and an ephemeral API port for any new-build live invocation. Commit to hel2; push HEAD to origin/master after reconciling any upstream changes without rebasing or changing branches.

## Validation and Acceptance

Prove exact logged requests match the submitted body, including Unicode, omitted context and runtime facts. Prove request/application correlation for successful changes, no-op and uncertain decisions, stale/cancelled requests, HTTP failures and worker refusals. Test rotation, restart reads, incomplete writes, absent logs, old workers and remote failure. Demonstrate that no diagnostic update affects turn evidence or triggers classification. TUI and web inspection must remain responsive while reads wait. Logs must contain no HTTP credentials. Ordinary polls and retries must not create transcript chatter.

## Idempotence and Recovery

There is no database migration or backfill. Diagnostic records are append-only and bounded by rotation; partial trailing records are reported or ignored as incomplete without fabricating decisions. Capability gating preserves older workers. Failed inspection never changes a session. Changes are committed on the current branch; pushing is explicitly authorized.

## Artifacts and Notes

Record validation and deployment/push evidence here as the work progresses. No additional classifier calls or changed model questions are needed for this feature.

## Interfaces and Dependencies

Reuse serde, serde_json, Tokio, existing relay/session-manager APIs, command palette and authenticated viewer API. Keep compact decision summaries in live state and fetch full log bodies only for inspection. New wire requests require the next relay protocol revision; do not change database compatibility or durable worker state solely for diagnostic history.


Validation so far: all 46 web unit tests pass (`node --test *.unit.test.mjs` in tests/e2e/web, outside the sandbox). New tests cover lazy exact-input retrieval, safe text rendering, missing/rotated history, and dismissal while fetching. Cargo compilation of all tests succeeds. Full dev-profile tests and strict Clippy are running; logs are `/mnt/optane/mj-jev-transparency-test.log` and `/mnt/optane/mj-jev-transparency-clippy.log`. `git fetch origin master` confirmed no upstream commits beyond this branch’s ancestry.


Isolated acceptance: built the dev CLI and ran `mj --instance jev-transparency daemon-run` with MJ_CONFIG_DIR and MJ_DATA_DIR under `/mnt/optane/mj-jev-transparency-instance-494l2uz7` and `phone.bind = "127.0.0.1:0"`. The new viewer asset was served; authenticated session listing returned 200; unauthenticated Jev inspection returned 401; authenticated list/detail requests for an unknown session returned 404. The isolated daemon was stopped afterward.

The first complete Cargo suite passed, but its mbx wrapper then waited on a shared cache lock also held by unrelated build work. Stopped only this task’s queued wrappers and used the installed Cargo 1.96.0 binary for final checks, retaining the ordinary target directory and elevated test execution. Strict `cargo clippy --all-targets -- -D warnings` passed; the final complete test suite is running. Final web validation again passed all 46 tests. No live store was upgraded or default daemon restarted.


Final validation passed: complete dev-profile `cargo test` (including the worker-rejection, evidence-generation, retention/restart, authentication, and responsive-inspector tests), `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `git diff --check`, and 46 web unit tests. Final logs: `/mnt/optane/mj-jev-transparency-test-final.log`, `/mnt/optane/mj-jev-transparency-clippy-final.log`, `/mnt/optane/mj-jev-transparency-web-final.log`. The final direct toolchain invocations used `/home/jonathan/.rustup/toolchains/1.96.0-x86_64-unknown-linux-gnu/bin/cargo`; tests ran outside the restricted sandbox. Details for existing workers become available when they run updated code; old logs are not backfilled.
