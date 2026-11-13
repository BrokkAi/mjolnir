# Publish Mjolnir sessions to Brokk Observe

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan follows [`.agents/PLANS.md`](../PLANS.md). It tracks GitHub issue [#1526](https://github.com/BrokkAi/mjolnir/issues/1526), Observe Phase 1. Observe Phase 2 is [#1527](https://github.com/BrokkAi/mjolnir/issues/1527) and is explicitly out of scope here.

## Purpose / Big Picture

After this work, a person can configure Mjolnir for Brokk Observe and see Mjolnir sessions without installing the separate Claude Code or Codex Observe plugins. The same path works for Muse, Kimi, Grok, OpenCode, DSH, Antigravity, Claude, Codex, and future ACP harnesses. Each session keeps its Mjolnir identity and records its actual harness rather than being presented as Claude Code.

Mjolnir's daemon publishes those sessions automatically through a durable
spool whenever Observe is configured. The local export remains available for
inspection. Observation never waits for Observe and adds no policy decisions.

## Progress

- [x] (2026-11-13) Created Phase 1 issue #1526 and Phase 2 issue #1527. Assigned #1526 and added `agent-in-progress`.
- [x] (2026-11-13) Read Brokk Observe's plugin spool, batch ingest, import, policy, detector, and session model.
- [x] (2026-11-13) Read Mjolnir's SessionWiki adapter, canonical transcript, API transcript response, and ACP permission path.
- [x] (2026-11-13) Wrote this ExecPlan and chose a non-enforcing first checkpoint.
- [x] (2026-11-13 13:06Z) Defined the stable Observe envelope and Mjolnir session metadata in `mj-core/src/observe.rs`.
- [x] (2026-11-13 13:06Z) Converted Mjolnir transcript items into deterministic Observe events in `mj-controller/src/observe.rs`.
- [x] (2026-11-13 13:06Z) Added the durable local spool and idempotent batch-file operations.
- [x] (2026-11-13 13:06Z) Added `mj observe status` and `mj observe export` so a person can inspect and produce the envelope stream without changing a running daemon.
- [x] (2026-11-13 20:30Z) Connected the daemon to a revision-driven background publisher with ordered spool delivery and visible retry state.
- [x] (2026-11-13 20:30Z) Added SessionWiki-driven history discovery while reading full canonical events from Mjolnir's checkpoint projection.
- [x] (2026-11-13 20:30Z) Added generic orchestrator, harness, collector, worker, native-session, and parent-session dimensions to Brokk Observe, including filters and session details.
- [x] (2026-11-13 20:30Z) Correlated vendor plugin events by native session id in both arrival orders.
- [x] (2026-11-13 20:30Z) Reused the plugins' shared configuration file with environment overrides and added automatic delivery state to `mj observe status`.
- [x] (2026-11-13 22:00Z) Validated the spool-to-HTTP path, focused Mjolnir suites, 68 Observe unit tests, 70 Observe PostgreSQL API tests, formatting, Clippy, browser asset syntax, and an isolated `mj observe status` configuration.

## Surprises & Discoveries

- Observation: Observe does not require a continuously connected live stream for ordinary capture.
  Evidence: `plugin/scripts/observe-hook.sh` appends hook events to a local spool and flushes ordered batches through `/api/v1/ingest/batch`; only `PreToolUse` enforcement requests a synchronous answer.

- Observation: SessionWiki is a useful history source but not a complete audit-event source.
  Evidence: `mj-controller/src/sessionwiki.rs` says the index keeps neither full tool arguments nor full output, and deliberately opens the index with `ToolOutputMode::Summary`.

- Observation: Mjolnir's API exposes structured transcript bodies and stable item positions, which are enough to build idempotent event identities without scraping presentation text.
  Evidence: `TranscriptItemView` in `mj-controller/src/server/api/types.rs` carries `stable_id`, `position`, `seq`, timestamps, and the structured `TranscriptBody`.

- Observation: Mjolnir's current ACP permission handler cannot be used as a universal capture hook.
  Evidence: tool-call notifications do not provide a veto, and `ExecutionPolicy::ConfiguredApprovals` preserves harness-specific behavior rather than guaranteeing that every harness emits `session/request_permission`.

## Decision Log

- Decision: Phase 1 exports events without enforcement.
  Rationale: Observation is independently useful, works across all harnesses, and does not depend on the unresolved pre-execution permission capability matrix.
  Date/Author: 2026-11-13 / Codex

- Decision: Use Mjolnir's structured transcript and canonical session metadata as the initial event source.
  Rationale: SessionWiki intentionally summarizes tool data, while the Mjolnir transcript retains tool input, output, stable item identity, and ordering. SessionWiki remains the discovery and historical-navigation layer.
  Date/Author: 2026-11-13 / Codex

- Decision: Make event identities derive from the Mjolnir session id and stable transcript position.
  Rationale: Re-exporting a session must be harmless. Observe de-duplicates by session id plus event id within that session. A position-based identity is stable across daemon restarts and repeated exports.
  Date/Author: 2026-11-13 / Codex

- Decision: Treat a single Mjolnir session as one logical Observe session and preserve the selected harness separately.
  Rationale: The orchestrator and the underlying model product answer different questions. Mjolnir owns the session and worker lifecycle; the harness owns tool semantics and model behavior.
  Date/Author: 2026-11-13 / Codex

- Decision: Reuse `~/.config/brokk-observe/config`, with environment values
  taking precedence one variable at a time.
  Rationale: A machine that already configured a vendor plugin needs no second
  credential, while a Mjolnir-only installation can configure the same file
  without installing either plugin.
  Date/Author: 2026-11-13 / Codex

- Decision: Treat SessionWiki as discovery and Mjolnir's canonical projection
  as the event source.
  Rationale: SessionWiki finds historical session ids and supplies navigation,
  but its summarized tool projection cannot feed detectors or preserve full
  tool input and output.
  Date/Author: 2026-11-13 / Codex

- Decision: Resolve vendor-native session ids into the Mjolnir logical session.
  Rationale: A user may already have Claude Code or Codex Observe installed.
  Native ids let the server attach that history and later vendor events to the
  Mjolnir session instead of double-counting one run.
  Date/Author: 2026-11-13 / Codex

## Outcomes & Retrospective

Phase 1 now records sessions without either vendor plugin. The daemon publishes
new revisions and completed sessions, discovers earlier sessions through
SessionWiki, and flushes ordered batches from a durable spool. Observe keeps
Mjolnir separate from the underlying harness and exposes worker and parent
dimensions. Existing vendor events are aliased by native session id.

The canonical transcript does not retain a separate native compaction boundary
or every native permission prompt, so those are not fabricated. Policy
decisions and their resulting permission outcomes come from the Phase 2 gate
when enforcement is active.

## Context and Orientation

Mjolnir is a Rust workspace. `mj-cli` is the `mj` command. `mj-controller` owns the daemon, session records, transcript storage, and API types. `mj-core` holds shared state and protocol types. `mj-worker` runs one ACP harness for a session.

Brokk Observe is a hosted Rust service. Its plugin sends a JSON envelope containing `meta` and `event`. `event.session_id` is stable within the sender's ingest key and `meta.event_id` makes retries idempotent. A batch is newline-delimited JSON sent to `POST /api/v1/ingest/batch`. The server applies redaction, detectors, counters, and storage in one transaction per event. Existing envelopes use hook event names such as `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, and `SessionEnd`.

SessionWiki is a local SQLite full-text index. Mjolnir writes top-level Mjolnir sessions and native harness sessions into it. It is good for finding a session, opening a history preview, and resuming. Its Mjolnir projection deliberately replaces tool arguments with a description and folds the result into a summary. It therefore cannot feed the full security detector path.

An ACP harness is the coding product behind Mjolnir, such as Claude, Codex, or Muse. The harness sends structured session updates to Mjolnir. A tool call is an action such as executing a command, reading or editing a file, or calling an MCP server. A permission request is a separate harness-to-client request that can be allowed or rejected before execution. Phase 1 records tool calls but never decides them.

## Plan of Work

First, add `mj-core/src/observe.rs` with versioned serializable types for the Observe envelope and Mjolnir metadata. Keep hook-specific event payloads as JSON so one producer can represent multiple harnesses. Add helpers that validate session and event ids and serialize newline-delimited envelopes.

Second, add `mj-controller/src/observe.rs`. Convert one `TranscriptResponse` plus a small metadata value into deterministic envelopes. A user transcript item becomes `UserPromptSubmit`. A completed or failed tool item becomes a synthetic `PreToolUse` followed by `PostToolUse` or `PostToolUseFailure`, preserving the ACP tool kind, title, raw input, raw output, terminal output, and call id. A turn end is emitted after the last agent item before the next user prompt. A stopped session ends with `SessionEnd`. Unknown or presentation-only transcript items are omitted rather than guessed into security events.

Third, add a filesystem spool in `mj-controller/src/observe.rs`. Each envelope is written atomically to a temporary file and linked into the spool directory. Listing is ordered by timestamp, transcript position, ordinal, and the stable id digest. Removing a flushed batch is explicit. A failed upload leaves every file in place. Tests use an isolated directory and prove that rewriting the same envelope does not create two distinct events after Observe's own de-duplication contract is modeled.

Fourth, add `mj observe status` and `mj observe export --session <id>` in `mj-cli/src/observe.rs`. `status` loads the plugins' shared configuration file with environment overrides, reports configuration and delivery state, and lists the spool path and pending count. `export` fetches the session and all transcript pages through the daemon API, converts them, writes newline-delimited JSON to stdout or a named output file, and never performs a network POST.

Fifth, connect the daemon. A short-lived controller task must enqueue session metadata and transcript deltas without blocking the API or render loops. It should spool after turns and session end, flush in the background, and retain files when delivery fails. Configuration and credential ownership need the Mjolnir secret rules before this milestone is enabled by default.

Sixth, add history backfill and cross-repository support. SessionWiki discovers candidate sessions. Mjolnir reads the source checkpoint or canonical transcript for full event data, then marks the import as historical. Brokk Observe adds generic collector, orchestrator, harness, and parent-session columns and UI filters. Existing vendor plugin events are correlated by native session id and must not create a second top-level session.

## Concrete Steps

All commands run from `/home/ryansvihla/code/mjolnir`.

Create or update the protocol and converter, then run focused tests:

    cargo test -p brokk-mj-core observe
    cargo test -p brokk-mj-controller observe

Add the CLI command and run its parser and converter tests:

    cargo test -p brokk-mjolnir observe

Format and lint the touched crates:

    cargo fmt --all -- --check
    cargo clippy -p brokk-mj-core -p brokk-mj-controller -p brokk-mjolnir --all-targets -- -D warnings

Inspect one export against an isolated Mjolnir instance:

    mj --instance observe-qa sessions --json
    mj --instance observe-qa observe export --session <id>

The export is one JSON envelope per line. The first event is `SessionStart`, user prompts are `UserPromptSubmit`, each tool call has a matching result event, and a stopped session ends with `SessionEnd`. Running the same export twice produces byte-identical lines.

## Validation and Acceptance

The focused protocol tests prove id preservation, newline serialization, plugin-config precedence, and rejection of empty identifiers. Converter tests construct a session with two user turns, one successful command, one failed command, and one file edit. They assert that event order and stable ids are deterministic and that the raw tool input and output survive conversion.

The spool tests prove atomic writes, order when timestamps tie, pending count, removal by exact batch name, and idempotence. The controller test spools a canonical session and uploads it as a real ordered HTTP batch, while a second test reconstructs a checkpoint-only session. The Observe API suite runs the receiving side against isolated PostgreSQL, including native-session adoption, historical timestamps, model and effort, worker metadata, and permission outcomes.

Phase 1 acceptance requires a real Mjolnir session to be auto-published and
visible in an isolated Brokk Observe instance without either vendor plugin.
The focused controller test runs the real spool, HTTP ping, batch flush, and
request framing; the Observe API suite runs the receiving contract against
isolated PostgreSQL.

## Idempotence and Recovery

Exporting and spooling are additive. Writing the same event identity again is harmless because Observe de-duplicates within the session. Flush removes a spool file only after a successful batch response. A failed process may leave a temporary file; the spool ignores files not ending in `.json`. Deleting the isolated spool directory discards only pending local delivery and does not alter Mjolnir sessions or SessionWiki.

## Artifacts and Notes

The initial issue bodies and the distinction between Observe's spooled batch
path and its synchronous enforcement path are recorded in #1526 and #1527.
Phase 1's observation path never waits for Observe; Phase 2's explicit
enforcement mode is the only synchronous path.

## Interfaces and Dependencies

In `mj-core/src/observe.rs`, define versioned types equivalent to:

    pub struct ObserveEnvelope {
        pub v: u8,
        pub meta: ObserveMeta,
        pub event: serde_json::Value,
    }

    pub struct ObserveMeta {
        pub event_id: String,
        pub ts: i64,
        pub collector: String,
        pub orchestrator: String,
        pub harness: String,
        pub parent_session_id: Option<String>,
        // Identity, git, and client metadata fields follow.
    }

In `mj-controller/src/observe.rs`, expose conversion and spool interfaces equivalent to:

    pub struct ObserveSession {
        pub id: String,
        pub parent_id: Option<String>,
        pub harness: String,
        pub model: Option<String>,
        pub cwd: Option<String>,
        pub started_at_ms: i64,
        pub stopped: bool,
    }

    pub fn envelopes_for_transcript(
        session: &ObserveSession,
        transcript: &TranscriptResponse,
    ) -> Result<Vec<ObserveEnvelope>>;

    pub struct ObserveSpool { /* isolated directory */ }

    impl ObserveSpool {
        pub fn enqueue(&self, envelope: &ObserveEnvelope) -> Result<PathBuf>;
        pub fn pending(&self) -> Result<Vec<PathBuf>>;
        pub fn remove(&self, path: &Path) -> Result<()>;
    }

The converter uses `serde_json::Value` for ACP tool fields so a new harness tool kind does not require a new Observe schema version. It preserves the raw object under `tool_call` in addition to the normalized fields Observe detectors read.

---

Revision note, 2026-11-13: Created the plan after distinguishing capture from enforcement and choosing the first non-enforcing command-line checkpoint.

Revision note, 2026-11-13: Recorded the completed protocol, converter, spool,
CLI status/export path, focused tests, Clippy, and the documented export-only
limitation. Automatic daemon publication is the next milestone.

Revision note, 2026-11-13: Completed automatic daemon publication, SessionWiki
discovery and checkpoint fallback, generic Observe dimensions, vendor-session
correlation, plugin-config reuse, historical publication semantics, and the
cross-repository validation gate.
