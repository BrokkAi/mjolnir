# Fix the latest subagent API reports

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Issue 986 reports phantom deleted tracked ignored files, discovery errors hidden by teardown, deferred bundle exports returning 500, and unclear CLI wait exit codes. Correct these while preserving checkpoint safety and treating cleanup as internal.

## Progress

- [x] Inspect reports and agree scope and discovery semantics with the user.
- [x] Implement capture, discovery shutdown, export diagnostics, and documentation.
- [x] Validate focused behavior, full serial suite, final merged component checks, Clippy, formatting, and documentation.
- [x] Prepare validated commits and ticket update for final publication.

## Surprises & Discoveries

Capture starts with an empty scratch index, discarding Git's tracked-file knowledge. CheckpointDeferred already provides a typed marker. Discovery aborts the runtime, potentially killing its supervisor before process-group cleanup completes.

## Decision Log

The user selected successful model results regardless of cleanup trouble. Preserve original discovery errors and log cleanup separately. Keep background safety gates and distinguish synchronization knowledge from active tasks. Push when complete, on the existing branch.

## Outcomes & Retrospective

The four fixes are implemented. Successful discovery survives a close timeout and original discovery failures remain intact. Supervisor cleanup now runs after forwarding errors and kills remaining descendants after leader exit. Public background facts preserve unknown state and deferred exports map to 409.

## Context and Plan of Work

Seed the scratch index in src/hel_archive/git.rs from Git's resolved index; initialize missing indexes explicitly. Verify tracked ignored additions, modifications and deletions without changing the actual index, including linked worktrees.

Add cancellation-aware ACP runtime shutdown in src/hel_acp.rs so discovery can abandon an unresponsive protocol request while retaining supervisor teardown. In mj-worker/src/hel_worker_runtime/discovery.rs drain events through close and bounded shutdown, preserve discovery results, and log cleanup failures. Test successful and failed discovery across cleanup failure and timeout.

Reuse the typed checkpoint deferral predicate for bundle HTTP 409. Expose worker background knowledge and tasks in session detail and wait session payloads, and distinguish unknown from active background blockers using shared snapshot interpretation. Keep genuine export failures at 500.

Document wait stdout and exit status in docs/src/content/docs/api-reference.md, including parsing JSON despite exit 1.

## Concrete Steps and Validation

From the repository root run focused cargo tests outside the sandbox, then cargo test -q -- --test-threads=1 and cargo clippy --all-targets -- -D warnings. Run cargo fmt --all --check and git diff --check. Build documentation with npm run build in docs. Validate subprocess teardown with a harness that refuses close and emits more than a channel buffer of events. Commit only changed files to the current branch and push origin HEAD:master.

## Idempotence and Recovery

No schema migration or provider configuration changes. Temporary test repositories and harnesses must be owned and cleaned up after process termination. Do not alter live sessions or restart the daemon.

## Interfaces and Artifacts

Add optional background-work facts to API session payloads; keep existing response fields and exit codes. Preserve profile cache and discovery response shape. Validation logs belong under target/. Update issue 986 after push.

Revision 2026-09-12: created from approved implementation plan.

Revision 2026-09-12: implementation and focused validation complete. The broad workspace check includes desktop GTK dependencies unavailable on this host; required default-member tests and Clippy cover these changes. A discovery test initially reused accepted configuration between independent probes; its fresh-probe fixture now resets that state. The supervisor regression verifies both parent-input closure and broken parent-output pipes, with a descendant ignoring TERM and an output larger than 64 KiB. Validation logs are target/ticket-*.log.

Revision 2026-09-12: all validation passed. Implementation commits are c61f0d6f, 630dee31, and 7e1b3678. A concurrent upstream push rejected the initial delivery; merge 7eee1bc2 integrates the Kimi task-identity/activity fixes without conflicts. Final merged validation passed 155 worker-state tests, 25 Kimi-specific tests, the worker/chat/TUI/CLI suites, 39 API tests, and Clippy. Full pre-merge validation is in target/ticket-regressions-tests.log; merged checks are in target/ticket-merged-tests.log and target/ticket-merged-clippy.log. Documentation checked 1,843 links. The completion comment is https://github.com/BrokkAi/mjolnir/issues/986#issuecomment-5648417326; its delivery status is updated after the final push. This plan checkpoint completes repository work and is published with the implementation. No live sessions were restarted or altered.
