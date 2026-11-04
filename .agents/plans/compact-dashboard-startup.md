# Send dashboard projections instead of durable launch history

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Opening `mj` should load what its dashboard can show, then follow changes from that exact snapshot. A live measurement found a 30,054,558-byte startup reply containing 4,551 sessions and 4,516 subagents. Subagent launch prompts contributed 20,666,439 bytes. Integer-array chunk encoding expanded the reply to 108,675,123 wire bytes, and the runtime feed subsequently sent another snapshot. The user authorized compact client data, historical detail on demand, and removal of the duplicate bootstrap and chunk overhead. Complete launch intent must remain available for recovery, worker creation, reviews, and accounting.

## Progress

- [x] (2026-11-04) Measure the live payload and trace its consumers and history membership.
- [x] (2026-11-04) Separate published subagent metadata from durable launch records, retaining full prompts and reviewer launch configuration in SQLite.
- [x] (2026-11-04) Make historical session detail follow the opened family; preserve lightweight relationships for counts and navigation.
- [x] (2026-11-04) Bootstrap the client replica from dashboard startup and continue from its cursor.
- [x] (2026-11-04) Encode reply chunks as base64 strings and advance the daemon protocol.
- [x] (2026-11-04) Validate persistence, real daemon/client round trips, cursor and scope changes, terminal behavior, and wire size in isolated tests.
- [x] (2026-11-04) Run final affected-crate suites, workspace Clippy and formatting; deliver this validated checkpoint on the current branch to origin/master.

## Surprises & Discoveries

The 1,200-child resource regression measured 32,678,399 bytes of durable launch-record JSON and a 298,580-byte runtime bootstrap. Closed history retained 1,199 unarchived relationships and only its two live parent rows; opening a family brought in its rows, followed a delta, and released those rows on closing, including after the parent stopped. This is a fixture measurement, not a post-install measurement of the live default instance.

Full source roots must remain in cursor history: filtering them recursively by live parents would prevent opening retained history for a stopped parent. The terminal's pane assignment now selects known child metadata immediately, and its existing transcript golden also exercises an intermediate refresh before detail arrives. Parked children have no worker; the existing `has_live_worker` state predicate now owns that UI decision too.

The first poll must not hold the replica mutex across a thirty-second network wait: that would block independent transcript-tail reads. The regression holds a real TCP long poll, fetches a tail concurrently, changes family scope, and verifies the old poll disconnects and a replacement daemon's snapshot becomes the new base.

Metadata saves require a previously registered full launch intent. Two existing database fixtures created children through whole-state saves, so their isolated fixtures now register full records first. The broad database run passed its other 234 cases, and both corrected cases passed on focused rerun.

Terminal production code never reads `SubagentRecord.initial_prompt` or `reviewer_launch`. It nevertheless receives them because `State` and `RuntimeProjection` reuse complete store records. A later sample had 36 top-level sessions, and one parent contributed 1,186 children. Fifty-eight archived records were also retained. Parked sessions count as active for lifecycle ownership, which explains the hundreds of individual startup summary requests and their observed fifteen-second timeouts.

`State` is also used by the daemon. Removing launch fields through a lossy serializer would fabricate incomplete operational records, so introduce a distinct metadata type and make operations that need launch intent read its complete durable record explicitly. SQLite continues to store the existing full `SubagentRecord` JSON; no migration or rewriting of launch data is intended.

## Decision Log

- Decision: Retain complete source roots in bounded cursor history and derive all client membership in `for_subagent_parents`, retiring the separate recursive live-history projection.
  Rationale: A pinned family can belong to a stopped parent. Pre-filtering daemon history would make its detail impossible to request. Immutable source maps share record contents, while the existing cursor history bounds changed data. One scope projection owns the client membership decision.
  Date/Author: 2026-11-04, Codex.
- Decision: Bound summary requests to four, acquire permits before acknowledgement deadlines, and cancel tasks when their view leaves. Reject responses from earlier visits by generation.
  Rationale: Thousands of historical rows must not create thousands of simultaneous socket reads or consume their timeout while queued.
  Date/Author: 2026-11-04, Codex.

- Decision: Introduce typed subagent metadata in shared state and runtime publications, with full launch records remaining in the database.
  Rationale: Clients cannot accidentally use fabricated empty prompts, and whole-state saves must preserve the stored launch intent.
  Date/Author: 2026-11-04, Codex.
- Decision: Retain lightweight family relationships for badges and navigation while scoping historical session detail to families currently needed by the dashboard.
  Rationale: Omitting history must not make existing children disappear from counts or prevent navigating to them. Closing a family should release unnecessary detail; scope changes need an explicit fresh snapshot.
  Date/Author: 2026-11-04, Codex.
- Decision: Use one startup runtime snapshot and cursor to seed the existing client replica, then request deltas. Encode chunk bytes with the workspace's existing base64 support.
  Rationale: This preserves atomic bootstrap/replay and the frame limit while avoiding repeated snapshots and integer-array expansion.
  Date/Author: 2026-11-04, Codex.

## Outcomes & Retrospective

Dashboard startup now carries one compact runtime projection and its cursor. Full launch prompts and reviewer configuration remain in SQLite; metadata saves merge into existing full records. Unopened stopped/parked child rows stay on the daemon, while lightweight relationships retain navigation and counts. Opening and closing families changes cursor scope and gets a new snapshot. Summary reads are bounded, cancellable, and generation-owned; long polls release the replica lock for independent tail reads. Chunk payloads use base64 strings under protocol 74.

Focused regressions passed. The final full run passed 3,698 tests with 30 existing ignores across core, client, controller, TUI, and CLI, including all 14 PTY cases and all three direct-daemon ownership/migration cases. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed. The 1,200-child fixture reduced its bootstrap to 298,580 bytes while retaining 32,678,399 bytes of full launch records outside that reply. The large Unicode transport regression verifies complete reassembly, reuse of the connection, and wire bytes below 1.5 times payload JSON size. No live default instance was modified or used for development testing. Delivery follows the repository's required current-branch commit and origin/master push.


## Context and Orientation

`mj-core/src/subagent.rs` defines complete durable child launch records. `mj-core/src/state.rs::State` is the metadata snapshot shared by the daemon and terminal. Database state assembly and saves live in `mj-controller/src/database/state_io.rs`, with committed updates in `committed.rs`. The database's `sessions.rs` exposes full launch-record lookup. The daemon builds cursor history in `mj-controller/src/daemon/feed.rs`; `RuntimeProjection`, deltas and the client replica are defined in `mj-client/src/runtime_feed.rs`.

`mj-client/src/daemon.rs` owns the wire types and framing reader. `mj-controller/src/daemon/serve.rs` writes replies. The dashboard bootstrap is assembled in `daemon/dashboard_reads.rs` and requested by `mj-cli/src/main.rs`. `mj-cli/src/dashboard.rs` initializes the terminal and `mj-controller/src/pollers/remote.rs` starts its runtime replica. Dashboard family navigation and initial summary hydration are in `mj-cli/src/dashboard/chat_tasks.rs` and `session_state.rs`. TUI relationship indexes and counts use `mj-tui/src/row_index.rs` and `native_agents.rs`.

## Plan of Work

First add `SubagentMetadata`, containing relationship and lifecycle metadata but no initial prompt or reviewer launch configuration. Give full records an explicit metadata conversion and an operation that applies metadata edits while retaining launch fields. Change `State`, runtime publications and display indexes to this metadata type. Keep database registration and full-record APIs unchanged. State saves must merge metadata into an existing durable record, never serialize metadata over its launch intent. Update the few launch and stopped-child task-summary paths to read full launch records from the database. Extend store integration coverage to verify state saves preserve large prompts and reviewer configuration.

Next distinguish lightweight family membership from requested historical session detail in the runtime feed. Keep complete immutable source roots for historical detail at each cursor; add scope identity to cursors so changing the opened families cannot apply a delta against a different view. Exclude archived history from requested rows. Let the terminal update the requested families from its current family view and retained conversation panes. A scope change interrupts a read-only long poll and gets a fresh snapshot. Preserve background errors and responsive cancellation.

Return a runtime snapshot, its cursor, stored layouts and non-runtime metadata from dashboard startup. Build the display state from it, pass the replica into the remote poller and begin with a delta request. Preserve client read frontiers. Hydrate summaries for rows actually needed at startup and when opening a child family rather than eagerly issuing requests for every parked child. Switch chunk bytes to base64 JSON strings, preserving the existing per-frame limit and complete-reply assembly semantics, and advance the protocol for these shape changes.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir`, using the existing mbx cache and target layout. Run tests outside the restricted sandbox. Use focused store, feed, transport and TUI regressions while iterating. The focused invocation preserves the same crate feature selection and uses libtest's multiple substring filters:

    cargo test -p brokk-mj-core -p brokk-mj-client -p brokk-mj-controller -p brokk-mj-tui -p brokk-mjolnir -- historical_family_scope metadata_state_saves bootstrap_poll dashboard_startup_larger a_stopped_subagent leaving_a_family stored_summary_bursts daemon::feed::tests

The completed database integration run also used `database::` as a filter. Rerun only failures after correcting fixtures. Final commands are:

    cargo test -p brokk-mj-core -p brokk-mj-client -p brokk-mj-controller -p brokk-mj-tui -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

 After the final source change run the full suites of all touched crates and `cargo clippy --all-targets -- -D warnings`, plus `cargo fmt --all -- --check`. Preserve Cargo feature selection when rerunning individual tests so the rerun does not unnecessarily rebuild the dependency graph. Commit coherent validated checkpoints on the current branch; push the completed work to `origin/master` without creating a branch or PR.

## Validation and Acceptance

Use isolated stores and named daemon instances only. A large recorded launch prompt must remain byte-for-byte intact after metadata saves and be available to worker launch/recovery, while being absent from startup and runtime JSON. A fixture with thousands of stopped children must keep its child counts, initially omit their session detail, load the requested family's rows, follow changes, and release unneeded history after scope changes. Test a scope change during a pending read and a daemon replacement so stale frames cannot overwrite a newer scope. The client must request a delta from the bootstrap cursor instead of a second snapshot. Drive chunk transport with more than 8 MiB of Unicode content; verify complete reassembly, bounded frames, connection reuse, and substantially reduced wire bytes. Extend existing rendered-output goldens for historical child navigation where useful. The full terminal suite must retain responsive quit and upgrade/re-exec behavior.

## Idempotence and Recovery

Never point a development binary at the live default store. Live measurements are read-only and report counts/sizes, not prompt contents. Tests own temporary directories and stop process groups before removing files. No database migration is planned: stored full launch-record JSON remains unchanged. Partial metadata must never replace a full record. An interrupted implementation can restart from this plan and the compiler's typed boundaries; do not add fallbacks that synthesize missing launch prompts.

## Artifacts and Notes

The measurement and prior fixes are recorded in `.agents/plans/dashboard-summary-pool-starvation.md`. Temporary validation logs belong under `/tmp`; compiled output must retain the configured mbx target layout.

## Interfaces and Dependencies

Use the current crates. Add no workspace crate. `SubagentMetadata` belongs in `mj-core::subagent`; the existing `SubagentRecord` remains the durable registration type. Runtime scope belongs to the existing daemon/client cursor protocol, with one history owner making membership decisions. Use existing `base64`, Tokio supervision, persistent snapshot maps, and client replica machinery. Any additional dependency declaration must use the workspace version. Preserve worker checkpoint and terminal upgrade handoff formats.

Revision note: Created after the user authorized implementing the measured payload reduction.

Revision note: Recorded implemented ownership boundaries, fixture size measurements, and passing focused regressions before final validation.

Revision note: Recorded completed validation and the final implementation and delivery checkpoint.
