# Make subagent creation and orchestration reliable

This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective throughout implementation.

## Purpose / Big Picture

An orchestrator must discover a profile's models without creating a project container, reject invalid selectors before provisioning, repair configuration, and abandon provisioning immediately. Subsequent milestones expose recorded usage, filtered transcripts, file injection, and explicit input requests. The implementation follows issue 986 comment 5643253355 and the user's decision that an empty profile cache populates automatically. No model prompt is needed for discovery.

## Progress

- [x] (2026-09-12) Inspect existing API, startup follow-up, lifecycle admission, worker harness installation, transcript projection, and ACP usage.
- [x] (2026-09-12) Phase 1: automatic persistent profile discovery, configuration repair, provisioning cancellation, workspace filtering, API diagnostics. Full workspace tests passed, followed by the added close and startup repair regressions; Clippy passed.
- [x] (2026-09-12) Phase 2 implementation: durable usage and role-filtered transcript paging. Full tests, the final HTTP compatibility regression, and Clippy passed.
- [ ] Phase 3: bounded atomic file injection and explicit elicitation interfaces.
- [ ] Validate, document, and commit coherent checkpoints on the current branch.

## Surprises & Discoveries

`mj-cli/src/server/api.rs::apply_followup` validates effort using the snapshot from before the model change. Startup failures remain in an in-memory map and override later waits. The start task supervisor owns a separately spawned task, so aborting only its supervisor does not stop the work.

`mj-controller/src/hel_server/api.rs` lists every workspace and has no public configuration route. `mj-cli/src/api_client.rs::connect` reads the token before testing server support. The controller already supports lifecycle cancellation and deferred cleanup, but web action admission refuses close while creation owns the session.

`src/hel_acp.rs` drops `PromptResponse.usage`; `src/hel_projection.rs` ignores usage updates. ACP usage fields have ambiguous per-turn/cumulative documentation, so preserve source semantics and never infer missing counters as zero. Existing structured elicitations already persist in the materialized session.

## Decision Log

2026-09-12: Use a durable profile cache with per-model efforts, populated automatically on lookup or configured create. Run a private prompt-free local harness probe on a miss; no existing worker or project container is required. Coalesce concurrent probes, expire entries after 24 hours, invalidate when profile settings or harness pins change, and refresh once before rejecting a value missing from cache. The actual target remains authoritative.

2026-09-12: Deliver the six defects before enhancements. Preserve API v1 compatibility through optional fields and opt-in input-aware waits. Free-text question classification and invented cost estimates are excluded.

## Outcomes & Retrospective

Phase 1 is implemented. `cargo test -q` passed across the workspace; the subsequently added close ownership regressions (4 tests) and startup/repair/cancellation suite (5 tests) also passed. `cargo clippy --all-targets -- -D warnings` passed after moving shared helpers ahead of test modules. The fake ACP discovery test proves model-specific effort discovery without a prompt. Cache tests prove automatic population, retry after failure, coalescing, persistence across reopening, expiry, and fingerprint invalidation.

## Context and Orientation

The root `hel` library holds ACP (the harness JSON protocol), durable relay events, transcript projection, SQLite, and path/process helpers. `mj-worker` owns target harness installation and processes. `mj-controller/src/hel_server/api.rs` owns public HTTP types and handlers. `mj-cli/src/server/api.rs` implements its `SubagentBackend` against daemon sessions. `mj-cli/src/server.rs` admits viewer/API lifecycle actions; `mj-cli/src/daemon.rs` supervises their execution. `mj-cli/src/api_client.rs`, `api_commands.rs`, and `main.rs` implement the thin CLI. Public documentation belongs in `docs/src/content/docs/api-reference.md`.

## Plan of Work

### Phase 1

Expose profile configuration at GET `/api/v1/profiles/{id}/config?model=...` and `mj models`. Add a persistent SQLite cache and a coalesced background discovery service. Stage a private profile copy using existing staging code; use worker-managed installation and ACP supervision to start a fresh harness against an empty directory, read choices, optionally set model to discover efforts, and terminate before removing files. Bound discovery to five minutes and cleanup separately. Persist only successful observations and retain retryability after failure. Refresh compatible observations from live sessions.

Before creating a bundle or dispatching New, validate requested selectors through discovery. Return 400 with available values for invalid selectors and a service error for failed discovery. Add current advertised config to session detail and PATCH `/sessions/{id}/config` with `{key,value}`, exposed by `mj set-config`. Observe command completion, refresh after model changes, then validate effort. Supersede failed initialization after successful repair or later accepted prompts; do not replay the original prompt after repair.

Close must bypass busy admission, cancel startup/provisioning, prevent the initial prompt, and schedule cleanup after cancellation completes. Join repeated closes. Preserve non-cancellable lifecycle completion before cleanup and surface background errors. Filter GET sessions by workspace_id and pass global CLI workspace selection through the same resolution used by new. Probe API version support before reading token; distinguish legacy daemon, disabled viewer, transport failure, unsupported version, and authentication failure without automatic restart.

### Phase 2

Carry reported usage from ACP completion through durable relay outcomes into per-turn database records. Add optional wait usage and paginated GET `/sessions/{id}/usage` with cumulative totals and coverage; absent data stays absent. Preserve provider-reported cost only when available. Test replay and multiple turns without double-counting. Add transcript role filtering before page limits using canonical roles, plus a continuation cursor that advances through filtered gaps without skipping streaming updates.

### Phase 3

Add PUT `/sessions/{id}/files?path=...` and `mj put-file`, accepting at most the existing 16 MiB limit. Require a live idle session, explicit overwrite permission, contained relative paths without symlink traversal, and atomic publication. Use shared concurrent-stdin/output process helpers and test payloads larger than 64 KiB. Expose pending structured elicitations and their response route plus CLI, and opt-in wait return `input_required`; ordinary waits retain existing behavior.

## Concrete Steps

Work in `/home/jonathan/Projects/hel2`. After each coherent implementation checkpoint run focused package tests, then the required workspace checks before committing:

    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

Every cargo test invocation runs with elevated sandbox permissions. Keep normal target storage; never redirect builds to /tmp. Stage explicit changed paths, commit on the current branch, and push each completed phase to origin/master as the user requested.

## Validation and Acceptance

Use hand-written fake harnesses/backends to prove automatic empty-cache discovery with no sessions, coalescing, persistence, retry cleanup, invalid selectors without provisioning, model-specific efforts, repair and later waits, immediate/repeated close and provisioning completion races, workspace filtering, and old-daemon diagnostics. Prove durable usage across replay/restart/multiple turns and missing fields. Prove complete filtered transcript pagination. Prove binary file injection over 64 KiB, limit/path/overwrite rejection, and interruption cleanup. Prove structured requests can be answered without blocking unrelated operations. Update public route and CLI examples with the tested contract. Do not use real model turns or existing user sessions as test fixtures.

## Idempotence and Recovery

Use additive SQLite schema changes compatible with existing databases. Cache failures are not permanent entries. Discovery processes own private working files and must exit before deletion. Closing uses existing durable cleanup behavior. Optional serialized fields default when reading older events. Tests use isolated storage; preserve unrelated working-tree changes.

## Artifacts and Notes

Record validation commands and outcomes here as milestones complete. The prior core API ExecPlan is `.agents/plans/native-subagent-api.md`; this plan changes its delivered implementation rather than repeating the initial feature.

## Interfaces and Dependencies

Extend `SubagentBackend` and shared HTTP request/response structs rather than introducing another transport or crate. Reuse `SessionHandle`, relay command identity/outcomes, `CancellableProcessExecutor`, profile staging, worker harness resolution, `session_config_choices`, the canonical transcript role helper, existing elicitation types, and workspace/path validators. Keep blocking scans, SQLite, and subprocess work outside event/render loops; supervise spawned tasks and report failures.

Revision 2026-09-12: Created from the approved three-phase plan, including automatic cold-cache discovery requested by the user.

Revision 2026-09-12: Phase 1 adds backward-compatible auxiliary SQLite tables for profile capabilities and exact configuration command results. Live observations update model-specific cache entries only for managed workers matching the local probe binary. Discovery has a shared shutdown cancellation flag and a five-minute subprocess deadline. Close requests publish stopping state immediately and wait for cancellation or an already committed create before cleanup. User additionally authorized pushing each completed phase to the configured upstream (`origin/master` from branch `hel2`).

Revision 2026-09-12: Phase 1 merged upstream without conflicts, passed full tests and Clippy again, and was pushed at b0d84727. Phase 2 preserves Claude 0.73.0 full-turn usage and Codex 1.11.1 last-request usage as distinct scopes, verified from the installed managed adapter sources. Unknown adapters retain unspecified scope. Totals include only known full-turn reports and carry per-counter coverage. Context occupancy is excluded; provider session cost is the latest cumulative observation with its timestamp. Records begin when upgraded events are projected; historical missing reports are not fabricated. Transcript role filters execute in SQL before a soft page limit that retains whole sequence ties; next_after_seq advances through filtered gaps.

Revision 2026-09-12: Phase 2 full default-member tests passed after correcting the new pagination fixture's non-agent content ordinal. The durable usage regression projects four turns in one transaction, replays them, reopens the database, and checks paginated records, covered totals, missing counters, and provider cost. ACP forwarding is tested with a scripted bridge. A final HTTP regression checks partial Codex usage and keeps usage out of the existing last_turn_outcome wire shape because older clients reject unknown fields there. An exploratory check of all workspace members reached optional desktop GTK dependencies absent on this host; the required default-member tests/checks do not need those libraries.
