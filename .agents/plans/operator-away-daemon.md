# Durable operator-away control and notices

## Purpose / Big Picture

An operator can run `mj daemon afk on`, leave, and later run `mj daemon afk off`. During the absence, protocol-45 workers refuse an agent's first request and the daemon sends a private mailbox explanation to that same agent. A repeated request waits for the operator. Returning wakes only top-level sessions that received an automatic reply and did not raise a later blocking request.

## Progress

- [x] (2026-10-27) Added action, State, runtime-feed, viewer-wire and CLI interfaces.
- [x] (2026-10-27) Added breaking migration 99, transactional request tracking, and replay frontiers.
- [x] (2026-10-27) Added reconnect synchronization and bounded concurrent fan-out.
- [x] (2026-10-27) Added migration, relay projection and HTTP action regressions; extended viewer delta and transcript goldens.
- [x] (2026-10-27) Interfaces compile; phase1-ready marker created.
- [x] (2026-10-27) Focused controller tests (3) and transcript golden (1) passed.
- [x] (2026-10-27) All five full crate suites completed once; four controller failures passed targeted follow-ups (one fake-relay fix, three unchanged individual retries). Workspace and final controller clippy passed; no commits.

## Surprises & Discoveries

The committed-state publisher observes table changes through connection-local triggers. Adding a table requires adding its preference case to `mj-controller/src/database/committed.rs`; otherwise a successful write would not reach either feed. Runtime-local elicitation IDs can be reused, so request matching must select the latest preceding captured event rather than keying persistent state by that ID.

## Decision Log

Daemon protocol advances from 62 to 63 because new clients send a new mutation. Relay protocol 45 is supplied by the worker implementation and is required for both mailbox variants. Migration 99 raises the minimum compatible revision to 99 because older daemons cannot decode OperatorAway/OperatorBack mailbox bodies. The parent approved this correction after reviewing the persisted enum contract. The parent explicitly authorized adding operator_away to mj_core::state::State and requested no commits. Worker versions are recorded durably so a restart before reattachment does not erase the AFK-off eligibility evidence.

## Outcomes & Retrospective

Implementation and validation are complete. The broad run had four controller failures, all passing targeted follow-ups; one needed a fake-relay control ACK and three were transient in unchanged code. Workspace clippy passed after helper ordering was corrected; the final fixture edit passed controller-only clippy. Desktop was excluded because GTK is unavailable. Final evidence belongs in `.mj/agents/89cc4a5d7fb14d672a22df1ff0f84737/afk-daemon.md`.

## Context and Orientation

The controller daemon owns persisted session records and global policy. Workers run the agents and send ordered relay observations. `mj-controller/src/database/materialized.rs` applies each relay page in a single transaction. `events.rs` captures API events in that transaction. The new `operator_away.rs` turns away resolutions into waking self-mailbox events, and `state_io.rs` changes the durable policy through the existing shared database writer. `daemon/snapshot.rs` serializes toggles and publishes runtime revisions. `session_manager/standalone.rs` reads the current flag before each connection's projection sync. `mj-transcript/src/projection/observation.rs` removes resolved forms and renders automatic replies as system notices.

## Plan of Work

The first milestone is shared interfaces: add SetOperatorAway actions to mj-client and the controller, operator_away metadata to both feeds, and mailbox rendering. Check the CLI and controller compile, then create phase1-ready. The behavior milestone adds migration 99, committed-state observation, a single serialized setter, and synchronization through actor-owned connections. The projection milestone records request captures and resolutions by API sequence, suppresses replay through an independent durable ordinal frontier, and queues return notices in the AFK-off transaction. Tests cover rollback, split pages, reused IDs, daemon/store reopen, later blocking requests, unsupported workers and projection rebuild.

## Concrete Steps

Run in `/home/jonathan/Projects/mjolnir`, retaining mbx's configured Cargo output directories:

    cargo check -p brokk-mj-controller -p brokk-mjolnir
    cargo test -p brokk-mj-controller operator_away --lib
    MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-transcript golden_mailbox_delivery_transcript --lib
    cargo test -p brokk-mj-controller deltas_omit_unchanged_metadata --lib

Cargo tests must run outside the sandbox. Runtime invocations require an isolated `--instance afk-test`; no test may touch the default instance. The child runs the five requested full crate suites and workspace clippy (excluding desktop if GTK is missing), once after focused checks; the parent owns commits.

## Validation and Acceptance

The migration regression must show a revision-98 reader/writer is refused after revision 99 and rollback preserves floor98. The projection test must produce four away notices and one return notice for the autoreply-only session plus one for a resolution arriving after AFK-off; the later-blocking and legacy-worker sessions receive no return notice. Rebuilding and replaying the projection must not change those counts. The HTTP integration must accept the action with no sessions. The existing browser-feed test must carry operator_away in metadata deltas. The transcript golden must contain the specified away/back system lines, with no manufactured user answer.

## Idempotence and Recovery

Repeated SetOperatorAway with the same value succeeds without changing the AFK period or producing another notice. Projection rollback leaves no half-written mailbox event, and replay frontiers survive projection replacement. Fan-out failures are logged and the next worker connection resynchronizes. Do not rewrite shipped migrations or alter live instance data.

## Artifacts and Notes

Validation output is kept as small text logs in the report directory. Build artifacts remain in mbx storage. No commits are authorized for this child.

## Interfaces and Dependencies

Use `DaemonAction::SetOperatorAway { away: bool }`, `ControllerAction::SetOperatorAway { away: bool }`, `RuntimeMetadata.operator_away`, `ViewerSnapshot.operator_away`, and `State.operator_away`. The worker supplies `RELAY_OPERATOR_AWAY_PROTOCOL`, `RelayRequest::SetOperatorAway { away }`, and `OPERATOR_AWAY_ACTION`. Mailbox bodies are `OperatorAway { request: String }` and `OperatorBack { requests: Vec<String> }`, both requiring protocol 45. Use existing rusqlite transactions, database writer, session actors, runtime revisions and mailbox outbox; add no new crate.

Revision note (2026-10-27): included the parent's return-notice extension and restart-safe per-period request tracking during implementation.

Revision note (2026-10-27): parent accepted transient visibility across split relay pages, approved breaking floor99, and made the projection transaction choose away/back for late resolutions.

Revision note (2026-10-27): recorded final full-suite evidence and targeted follow-ups; no broad test suite was repeated.

## Versioned-presence review follow-up

The independent AFK review found that concurrent relay writers, missed off/on transitions and coalesced TUI frames cannot be distinguished with a boolean. The parent approved replacing the shared fact with `OperatorPresence { away, revision }`, revisioned durably in unreleased migration99. Every actual transition increments the revision; no-op decisions belong to the database writer. Away revisions also identify AFK periods, eliminating the separate singleton period counter. Browser ViewerSnapshot retains its boolean. Relay45 now carries away+revision. Workers apply only greater revisions under the same refusal-memory mutex and clear memory on every applied revision. Standalone connections cache the sent revision rather than the bool. The TUI/dashboard owner adapts to the shared presence object separately.

- [x] (2026-10-27) Live-test bin directory contains mj; Cargo sequencing condition satisfied before any Cargo invocation.
- [x] (2026-10-27) Core, client, controller and worker interface check passed; presence-ready marker written.
- [x] (2026-10-27) Extended ACP bridge and reconnect regressions for stale/equal writes, missed off/on and same-bool revision changes; durable no-op regression extended.
- [x] (2026-10-27)60 affected tests and touched-crate clippy passed; real-action red/green proves live AFK-off ownership defect, summary golden retains command/back notice.

Revision note (2026-10-27): applied the parent's versioned-presence design to remove the three reviewed races structurally; no UI/dashboard or web-source edits are owned by this follow-up.

Revision note (2026-10-27): the live AFK-off regression reproduced Done-without-persistence when the old worker-projection no-op guard was restored. Removed that guard and verified the authoritative writer path. Corrected an ACP fixture session scope. Reused bounded whitespace normalization for the transcript command line; OperatorBack remains visible. Final evidence is presence-followup.md in the report directory.
