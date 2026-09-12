# Complete native subagent events and accounting

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture

Orchestrators must observe every prompt start, completion, error, and explicit question without losing transitions when UI snapshots coalesce. They must also receive honest complete-turn Codex token usage where the provider reports sufficient information. This work spans Mjolnir at `/home/jonathan/Projects/hel2` and its BrokkAi adapter at `/home/jonathan/Projects/codex-acp`.

## Progress

- [x] (2026-09-12) Inspect existing APIs, projection transactions, shared activity classification, adapter, and release rules; confirm plan with user.
- [x] (2026-09-12) Adapter 1.11.2 implemented, 575 tests passed, typecheck/build/pack passed, packaged guardian/yolo probes passed; committed 75a7641, pushed and published.
- [ ] Phase 1 Mjolnir checkpoint: pin and usage scope implemented; full serial tests passed, final Clippy and commit/push pending.
- [ ] Phase 2: persist typed events and expose SSE and CLI replay, validate, commit and push.
- [ ] Phase 3: finish question/activity coverage, documentation, and authorized ticket comments; validate, commit and push.

## Surprises & Discoveries

The live pinned Codex 0.153.4 probe emitted two request counters totaling 36,032 tokens; adapter 1.11.1 returned only the last 18,030. Raw response-completion types exist but require an experimental thread option which the adapter does not enable. Existing cumulative counters are the planned accounting source. The UI classifies idle as no foreground or background work, not elapsed silence. The user explicitly selected existing idle semantics instead of a stall detector.

## Decision Log

On 2026-09-12 the user chose native API nice-to-haves, explicitly excluded evaluation-driver additions, included the Codex adapter release, selected existing ACP elicitation for questions, and selected shared UI idle machinery without a separate silence detector. Use these decisions even though the original ticket requested an asked_question stop reason and a timed stalled event. Preserve existing wait compatibility. Commit to current branches and push each phase; publish the adapter through its normal release workflow.

## Outcomes & Retrospective

Adapter 1.11.2 is published. Workflow 34711472926 passed; npm metadata is public. The GitHub tarball matches the live-tested tarball: SHA-256 47e9974b16129e35ea06013ca01073f11c84ccd9500044bcb7ab5e3f4640574f. The requested evaluation-driver exclusion comment is posted at https://github.com/BrokkAi/mjolnir/issues/986#issuecomment-5647861279. Mjolnir implementation continues.

## Context and Orientation

`src/hel_projection.rs` converts durable relay events into `MaterializedSessionMutation`. `src/hel_database.rs` applies pages of these mutations atomically but coalesces intermediate states. A typed event journal must retain transitions separately within those same transactions. `mj-controller/src/hel_server/api.rs` provides authenticated versioned routes and `SubagentBackend`; `mj-cli/src/server/api.rs` implements it. `mj-chat/src/usage_format.rs` supplies the shared SessionActivity classifier for UI state. Existing ACP elicitations are already projected, listed, and answered through the v1 API.

In the adapter, `src/CodexEventHandler.ts` receives native token notifications and `src/CodexAcpServer.ts` builds ACP prompt replies. Session counters currently retain last request and thread total only. `src/hel_usage.rs` in Mjolnir marks all Codex reports last_request; add explicit metadata recognition while preserving old report interpretation.

## Plan of Work

### Phase 1: accounting and adapter release

Track cumulative counter baselines and prompt-local observed consumption using native turn identity. Repeated totals do not add usage. Preserve baselines between prompts, distinguish absent usage from zero, and mark unknown baselines/resets/incomplete reports as incomplete. Cancellation reports consumption received so far. Add ACP usage metadata declaring scope, recognized by Mjolnir without rewriting historical records. Context occupancy continues using last request data. Costs remain provider-reported amounts, never estimates.

Add behavior tests for multiple requests, successive prompts, resumed sessions, duplicate updates, cancellation, missing reports and resets. Follow adapter `docs/RELEASES.md`: npm ci, typecheck, tests, build, pack, live guardian/yolo verification, update version and changelog, commit and push BrokkAi main, tag exact tested commit, verify publication. Update managed adapter package and lockfile, runtime version/install identity and affected fixtures in Mjolnir, then run Rust checks and commit/push phase 1.

### Phase 2: durable events and CLI

Add an SQLite event journal with global monotonically increasing sequence, session identity, timestamp, type and JSON payload. Persist relay-derived events with their projection page and retain ordered intermediate events. Record turn_started, turn_ended, error, input_required, input_resolved and activity_changed. Controller lifecycle failures and observed activity transitions use the existing background database writer. Deduplicate replay and unchanged activity. Delete history only when its session is forgotten; record from upgrade forward without speculative backfill.

Expose GET /api/v1/events as SSE with the v1 token/version contract. Accept optional session and workspace filters and after_seq or Last-Event-ID cursor; reject conflicting values. Without cursor capture current frontier, then stream future events. Bounded database pages and bounded channels keep slow readers off event loops; readers resume from the last SSE ID. Leave viewer /api/events unchanged. Add mj events with matching filters/cursor and line-delimited JSON output.

### Phase 3: shared activity, questions, docs and ticket

Use existing ACP elicitations, including text schemas, for input_required and input_resolved. Preserve return_on_input wait semantics and response validation. Publish the same activity classification and timestamps as UI SessionActivity, with connectivity and pending input retained; no silence detector or stalled outcome. Document interfaces and examples in docs/src/content/docs/api-reference.md and cover question/answer/resume behavior in tests.

Post the authorized ticket comment: “I’ve decided not to implement the evaluation-driver additions: target fact discovery, per-session environment overrides, run grouping/reporting, and workspace-management APIs. File upload is already implemented.” Record delivered changes and the agreed ACP/idle replacements on issue 986 after implementation.

## Concrete Steps

Run commands from each repository root. In the adapter run npm ci, npm run typecheck, npm test, npm run build, npm pack, and live guardian/yolo probes using its run-codex skill and existing test patterns. Release only the next unused patch version, retaining Codex 0.153.4. In Mjolnir run cargo fmt --all --check, elevated cargo test, elevated cargo clippy --all-targets -- -D warnings and git diff --check. Commit only changed files and push HEAD to configured upstream. Never create branches or rebase.

## Validation and Acceptance

A multi-request Codex turn must expose combined usage rather than last-request usage, while missing counters remain missing. Old reports retain last_request scope. SSE tests must prove rapid start/end survives coalescing, rollback leaves no events, replay is idempotent, filtered reconnects are ordered, daemon restart retains history, slow clients cannot block execution, and errors carry session/turn identity. A free-text elicitation must flow from request event through response to resolution and turn completion. Activity output must match UI idle, running, background, waiting and disconnected behavior.

## Idempotence and Recovery

Projection event writes share the existing replay transaction and unique relay identity. Published npm versions and tags are immutable; retry a failed publication workflow for its existing tested tag. Never update the Mjolnir pin before package verification. Preserve unrelated working-tree edits. No event history backfill or schema-destructive migration is needed.

## Artifacts and Notes

The read-only planning probe log is /tmp/mj-986-codex-protocol.log. Do not publish its full contents because it includes local provider configuration. Record only relevant usage totals and test outcomes here.

## Interfaces and Dependencies

Reuse existing SQLite writer, Axum SSE, Tokio bounded channels, ACP schemas, SessionActivity and CLI API client. Add no workspace crate. Public usage semantics remain scope plus counters. Public events carry seq, session_id, timestamp, event type and typed data, with turn identity when relevant. The SSE id is the durable seq; consumption is at-least-once on reconnect, with IDs available for client deduplication.

Revision 2026-09-12: created from the user-approved plan before implementation.

Revision 2026-09-12: recorded adapter publication and live validation. Mjolnir parallel validation exposed the pre-existing upgrade fixture executable-copy race (ETXTBSY); the full serial suite passes without modifying unrelated implementation.
