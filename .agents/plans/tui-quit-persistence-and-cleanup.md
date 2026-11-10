# Reduce TUI quit persistence and cleanup waits

This ExecPlan is a living document maintained according to `.agents/PLANS.md`. Its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective sections must stay current. Tracking issue: #1486. Draft lifecycle work overlaps #1386, whose broader cleanup interface is outside this change.

## Purpose / Big Picture

The terminal dashboard should close promptly without losing unsent text or read positions. Six measured quits took 2.9–8.0 seconds normally and 20.1 seconds in one slow case. That outlier spent 16.5 seconds waiting for draft/read-position acknowledgments, including 43 draft-save timeouts. Quit currently re-saves retained conversations regardless of changes. Final settings saves and session-manager cleanup then run sequentially. An additional 1.1–2.3 seconds is attributed to teardown, which includes both destroying UI state and reconnecting to the background service to detach.

After this work, changed drafts are saved while the dashboard is open, repeated unchanged navigation and quitting do not produce redundant draft writes, and quitting drains outstanding mutations before independent final cleanup runs together. The single exit log distinguishes dashboard destruction, attachment cancellation, connection setup, and detachment. Accepted writes remain supervised and errors remain visible. No installation into the live instance is authorized by this task.

## Progress

- [x] (2026-11-10) Analyze six existing quit records and trace mutation-drain and teardown boundaries.
- [x] (2026-11-10) Update the existing master checkout to a83864d97 and claim #1486 plus overlapping #1386 work.
- [x] (2026-11-10) Settle draft ownership and serialization, including recovery-entry lifecycle and submission ordering.
- [x] (2026-11-10) Implement incremental draft persistence, ordered storage, backward-compatible handoff, and disposal of both warm and standby editors.
- [x] (2026-11-10) Overlap independent final cleanup after durable mutation drain and split teardown timings; source review complete, runtime validation pending.
- [x] (2026-11-10) Run focused regressions and an isolated named-instance TUI check.
- [x] (2026-11-10) Run final required Cargo suites, targeted review regressions, clippy, formatting, dev builds, and the final-binary isolated TUI scenario.
- [x] (2026-11-10) Review the integrated change, commit on the current branch, pass push review, merge upstream, and push implementation to origin/master as 99b348fef.

## Surprises & Discoveries

The existing detached-draft endpoint inserts a new recovery archive for each nonempty save. Reusing it on every edit would amplify #1386, rather than merely improve quit latency. Incremental persistence must update a single client-owned draft and retire superseded state on submit or clearing while preserving other clients' text.

Stored-summary reads are untracked background operations and do not block quit. Their generic timeout message incorrectly describes them as saves. The pending-write interval measures elapsed event-loop time while actual mutation guards remain; it is not a sum of individual write durations.

The teardown timer begins before DashboardContext fields drop and ends after main cancels and joins attachment maintenance, makes a new daemon connection, and awaits Detach. No fixed two-second destructor sleep was found. Runtime shutdown consumed 0–5 milliseconds in all measured exits.

The slowest observed quit coincided with a daemon upgrade handoff waiting for session creation. The daemon logs do not establish why acknowledgments stalled, so this change does not claim to fix that independent handoff problem.

The existing detached_drafts table rejects empty text with CHECK(length(text) > 0). Adding a revision column alone therefore cannot hold cleared ordering records. Migration 123 must rebuild that table while copying every existing archive and retaining its IDs, recovery timestamps, foreign keys, and indexes. Its text constraint allows emptiness only for a managed row with a non-NULL positive revision.

Independent ownership review found that lifecycle disposal removed the warm chat and draft cache but left its standby editor alive. Automatic capture would then re-adopt discarded text. Disposal must remove the actual standby composer at the same ownership boundary. A delayed external clipboard request can already outlive shutdown; that pre-existing cancellation boundary is outside this change, while paste results that arrive during normal interaction are captured by the new writer.

## Decision Log

Decision: preserve the existing durable-mutation drain boundary before shutting down worker-facing request transport. Rationale: pending submission and lifecycle actions can still depend on that transport; cancellation of background reads alone does not establish safety. Date: 2026-11-10, parent agent.

Decision: do not blindly reuse main's initial daemon stream for detachment. Rationale: a dashboard can survive daemon replacement; a stale stream does not automatically reconnect after arbitrary transport errors. Attachment maintenance must be cancelled and joined before detachment to prevent attaching after detaching. Date: 2026-11-10, parent agent.

Decision: reuse existing persistence/ordering mechanisms where possible and keep a single owner for each draft's requested, in-flight, and acknowledged state. Rationale: an old acknowledgment must never clear a newer edit, and an older save must never resurrect submitted text. Exact implementation interfaces will be recorded after the focused lifecycle investigation. Date: 2026-11-10, parent agent.

Decision: add nullable revision to detached_drafts and a unique managed owner key. The initial source/workspace/session key was corrected after push review to source/session, as recorded below. Existing archives retain NULL revision. One managed row keeps a stable draft ID; clearing stores empty text plus its revision instead of deleting the ordering evidence. Only strictly higher revisions change that row, and listing/recovery excludes empty rows. The migration is breaking because older readers would expose empty ordering rows and older writers bypass revision checks. Rationale: local acknowledgment ordering alone cannot reject an older server operation that finishes after its caller times out. Date: 2026-11-10, parent agent.

Decision: compare-clear an inherited legacy seed only during the first managed-row insertion, never on an update. Higher-revision saves preserve recovered_at for identical text and reset it only for changed text. Rationale: lost acknowledgments cannot justify clearing an explicitly recovered legacy draft. A recovered copy and the original live composer are independent; genuinely newer text must remain protected instead of being suppressed. Date: 2026-11-10, parent agent.

Decision: replace the combined detached-draft/read-receipt action with a revisioned draft-only action, and send read positions through the existing receipt owner. Rationale: independent owners already provide the proper serialization; duplicate combined writes make every navigation and quit expensive. Date: 2026-11-10, parent agent.

Decision: keep attachment detachment after settings persistence completes. Rationale: DAEMON_EXIT_WHEN_IDLE can terminate the service within its 100 ms polling cycle after the last attachment leaves, which can race final settings requests. No new liveness lease is warranted for this task. Manager shutdown and settings saves can safely overlap after the mutation drain. Date: 2026-11-10, parent agent.

Decision: a final flush retries an already-failed value but does not queue a second attempt for the same value that is currently in flight. Rationale: quitting during an acknowledgment wait must not turn the existing 15-second wait into two serial waits. A genuinely newer desired value still starts after the earlier completion; a failed latest value is reported and the surface can exit. Date: 2026-11-10, parent agent.

Decision: managed draft identity is source/client plus session, and workspace is placement metadata that follows the session. Move managed rows with session_contexts in the same transaction, and resolve the current workspace inside the draft-save transaction so a delayed request with old placement cannot recreate a source-workspace copy. Legacy NULL-revision archives remain historical. Rationale: push review found that workspace-keyed ownership left unchanged drafts behind and allowed source-workspace closure to delete their only durable copy. The new, unshipped migration 123 can still be corrected before publication. Date: 2026-11-10, parent agent.

Decision: remove workspace_id entirely from the new terminal draft request and storage APIs. The transaction resolves placement from session_contexts; inherited-input retirement compares the session and exact adopted text in that same transaction. Rationale: the second push review found that retaining the client's stale workspace solely for seed retirement could leave submitted text in the legacy field after a move. Removing that input makes the stale placement check impossible. Date: 2026-11-10, parent agent.

## Outcomes & Retrospective

Implementation, validation, review, and delivery are complete. Drafts persist while editing, acknowledged unchanged drafts do not create quit writes, and final settings persistence overlaps manager cleanup after mutation drainage. The first focused run exposed a missing async runtime in a new UI fixture; the corrected tests passed. Review exposed workspace-placement and seed-retirement ownership gaps; both were corrected at the storage owner, and the final review was clean at 6b2afb0ba. The implementation was pushed to origin/master as 99b348fef after merging unrelated upstream mbx changes. No installation was performed.

Checkpoint 74da52a35 contains the implementation and initial focused validation. Workspace clippy passed with warnings denied. Push review found one workspace-placement ownership bug, corrected in the follow-up described below before final suites and a second review. At that checkpoint neither installation nor push had occurred.

The workspace-placement correction is complete and seven affected storage integration tests passed in `/mnt/optane/tmp/mj1486-validation/review1-workspace-drafts.log`. All three session-placement paths move managed drafts transactionally, and delayed writes resolve current placement inside their transaction. Managed ownership is now source/session; legacy archives remain unchanged.

The isolated tmux acceptance scenario passed using the initial copied binaries in named instance tui-quit-persistence-1486-1518144. It verified continuous save, stable recovery ID, independent clients, clear/submission retirement, quit without first waiting for the final edit acknowledgment, and explicit recovery into a newly opened TUI. Quit took 122 ms: 110 ms pending writes, 8 ms final saves/combined cleanup, 1 ms dashboard destruction, 2 ms aggregate teardown, and 0 ms runtime shutdown. This two-session fixture does not establish the timing of a large live dashboard or an unresponsive daemon. Database integrity and foreign-key checks passed. Evidence is `/mnt/optane/tmp/mj1486-live-artifacts/tui-quit-persistence-seed-1486-1518144/live-evidence.json` and sibling runtime logs/captures.

The full touched-crate command passed client 54/54 and controller 2517 tests, with 19 ignored, then stopped on one fixture-only failure before the CLI suite ran. The environment migration fixture rewound to revision 121 but removed only migration 122, leaving the new 123 entry. Its rewind now removes all entries >=122; the failed regression passed in the targeted follow-up. The second review's seed-retirement correction and affected draft/workspace cases passed controller 10/10 and CLI 1/1 in `/mnt/optane/tmp/mj1486-validation/workspace-snapshot-removal.log`. The remaining CLI targets, final clippy/build, and final review were run next, without repeating passing controller/client suites merely to confirm the fixture fix.

Final validation then passed every remaining CLI target: binary 251, ACP 3, daemon startup 21, instance 4, logging 2, store divergence 3, and PTY termination 14. Four import cases remain intentionally ignored for external credentials/images; Windows-only targets selected zero tests on Linux. Together with the client/controller suites and the corrected failing fixture, this accounts for 2870 passing tests and 23 ignored tests, without repeating passing suites after a single fixture correction. Doc tests selected zero examples. Final workspace clippy with warnings denied and workspace formatting both passed. Logs are remaining-cli-tests.log, final-doc-tests.log, final-clippy.log, and final-build.log under `/mnt/optane/tmp/mj1486-validation`.

The final dev binaries built from 6b2afb0ba passed the same isolated TUI scenario again, with no fixture changes. The final suffix-carrying quit took 130 ms: 114 ms pending writes, 7 ms combined cleanup, 2 ms dashboard destruction, and 4 ms aggregate teardown. The two other client quits took 15 and 11 ms. This remains a small two-session fixture rather than a prediction for a large live dashboard. Integrity and foreign-key checks passed, and all named-instance processes were cleaned up. Final artifacts are `/mnt/optane/tmp/mj1486-final-live-artifacts/tui-quit-persistence-seed-1487-2819120/`; final binaries are `/mnt/optane/tmp/mj1486-validation/final-bin/` with mj SHA-256 522cac657b2c143a9fc212a301558c7101205de31040488d505f3f00179c31db and worker SHA-256 94faa3d1169152518ba9d4126bffc44896a44c0a31ba531a962bb9eb7c415a59.

## Context and Orientation

The TUI is the terminal UI in `mj-cli`. The daemon is the background service that owns durable session state. `mj-cli/src/dashboard.rs` owns the interactive context, the mutation-drain loop, and final persistence. `begin_shutdown` captures retained chats and starts detached-session saves; `refresh_shutdown_notice` keeps the loop running while critical mutation guards or read-receipt writes remain. `QuitTimings` stores stage timestamps and `log_quit_timings` emits the single exit record after runtime shutdown.

`mj-cli/src/dashboard/composer_drafts.rs`, `drafts.rs`, and `chat_tasks.rs` capture and restore composer state, where the composer is the unsent text input and its pending choices. `io/spawn.rs` performs asynchronous daemon calls and reports results to the dashboard. `read_receipts.rs` already owns per-session desired, in-flight, and saved read positions. `workspace_settings.rs` provides an existing serialized, coalescing background writer for layout preferences: it compares requested values with acknowledged values and supervises completion.

`mj-controller/src/database/client_state.rs` stores client drafts and recovery entries. Different clients may have distinct unsent text for one session; a TUI must never retire another client's draft. Submission is another independently changing boundary: saving an older draft after submission could resurrect text that was already sent.

`mj-cli/src/main.rs` starts and ends the dashboard and owns attachment maintenance. `mj-cli/src/daemon.rs::maintain_attachment` refreshes presence using cancellable asynchronous operations. After the dashboard returns, main currently cancels and joins this task, connects to the current daemon, and sends Detach. `mj-controller/src/session_manager/channels.rs` owns worker-facing shutdown. `mj-core/src/runtime.rs` coordinates final runtime destruction, which must remain last.

## Plan of Work

First settle draft persistence around one owner that receives meaningful composer changes, serializes writes for each client/session, coalesces newer values, and tracks acknowledged state. Loading a baseline must not be mistaken for an edit. Emptying or submitting text must retire only the corresponding client's superseded state. Navigation and shutdown request a flush without re-publishing unchanged snapshots. Preserve the existing read-position owner and mutation-tracker guarantees. Extend existing tricky-condition or integration coverage rather than adding ordinary happy-path unit examples.

Second, keep auto-AFK finalization and terminal restoration in their required order, then run final preference persistence and session-manager shutdown together after mutation drainage. Evaluate moving attachment cleanup into that same owned cleanup phase only if daemon attachment semantics preserve the final saves and failure paths. Record actual operation durations for overlapping stages; do not subtract sequential timestamps as though overlapping work were disjoint. Split the teardown interval into observable milestones without adding a separate log-drain diagnostic or persistent diagnostic state.

Finally review the integrated diff and run focused tests during implementation. After the last changes run workspace clippy and the complete suites of the touched crates once, as required by the current AGENTS instructions. All cargo test invocations require elevated permissions. New-build daemon/TUI invocations must use an isolated named instance and isolated configuration/data roots, never the user's live store. Use the existing tmux reliability harness where practical. Commit only changed files on the current master branch and complete the configured push review before pushing.

## Concrete Steps

All repository commands run from `/home/jonathan/Projects/mjolnir/.mj/clones/aeaccef34ab1194cf335adf65788e84f`. Do not change Cargo target paths or mbx settings. During implementation use `cargo test -p brokk-mjolnir --bin mj -- <affected-filter>` and equivalent package-specific commands for newly touched persistence code; list the actual final filters here once known. Run Cargo tests outside the restricted sandbox. Use `cargo fmt --all -- --check` and final `cargo clippy --all-targets -- -D warnings` in the dev profile. Cargo builds must retain the host's normal mbx configuration.

Focused tests used one consistent package set: cargo test -p brokk-mjolnir -p brokk-mj-controller -p brokk-mj-client -- followed by the changed draft/cache, handoff, async save, cleanup, migration, and storage regression names. Results are in `/mnt/optane/tmp/mj1486-validation/focused-tests.log` and `focused-rerun.log`. Only the two failing/new UI fixtures were rerun after adding their async runtimes; they passed 2/2. Final suites must use the same package set. cargo fmt --all -- --check and git diff --check also passed.

The dev build command cargo build -p brokk-mjolnir -p brokk-mj-worker --bin mj --bin mj-worker succeeded. Its copied binaries are `/mnt/optane/tmp/mj1486-validation/bin/mj` (SHA-256 4a5cd9cc76a5c47dac17658d72b3c0b3b6a7617ff48120349b05d6b53bb7a616) and sibling mj-worker (SHA-256 16f5e490d9301155eeb36e8011ca28359eae6924c139d8a4b0592f6ec5b5f199). The live test uses those copies, which are safe from mbx target cleanup and later source/test rebuilds.

For a live check, create isolated config/data roots outside target/, use a named instance such as `tui-quit-1486`, launch under an isolated tmux socket with a sanitized environment and test-owned profile homes, change a draft, allow normal persistence, then quit and reopen to confirm its recovery and read position. Compare the emitted quit stages and verify that unchanged retained chats do not create a burst of writes. Preserve captures as evidence and stop only the named test daemon during cleanup.

## Validation and Acceptance

Regression coverage must show that acknowledged unchanged drafts do not write again on repeated navigation/quit, a newer edit made while an earlier save is pending remains pending until its own acknowledgment, submission/clearing cannot be undone by a stale draft save, independent clients' drafts remain distinct, and failures remain visible and do not leave quit waiting forever. Cleanup coverage must show independent final saves and manager shutdown start together only after the mutation drain, and attachment refresh cannot occur after Detach. Use deterministic synchronization rather than timing guesses when possible.

The new single exit log must report separate teardown activities and honest durations for concurrent cleanup. A named-instance TUI run must retain the newest draft through quit/reopen and exit without touching the live instance. Test commands, results, and any baseline failures will be recorded here before acceptance.

## Idempotence and Recovery

The work stays on the current branch. Stage only explicitly changed files. A rejected push is resolved with fetch and merge, never rebase. Do not install the build or mutate the live store. If a new database revision is necessary, allocate the next revision at integration time, classify compatibility beside it, preserve older revisions, and test with isolated stores. Prefer avoiding a migration if the existing client-scoped storage can express the owner model correctly.

## Artifacts and Notes

Observed totals in milliseconds: 2855, 8022, 4676, 4298, 4994, 20082. The slow record is `/home/jonathan/.local/share/mjolnir/logs/mj-tui-20261109T213422.374Z-1209423.log:9316`, with pending_writes_ms=16547, final_saves_ms=961, session_manager_ms=577, teardown_ms=1993, runtime_shutdown_ms=0. The log-writer drain after this record is outside the measured interval.

## Interfaces and Dependencies

Use existing Tokio tasks/channels, CriticalOperationTracker, daemon request types, and draft data shapes. Do not add a workspace crate, a speculative concurrency cap, or a new generic dependency-injection framework. Parent owns shared `dashboard.rs` integration decisions and the plan; concurrent implementers receive disjoint symbol ownership.

The daemon action is PersistTerminalDraft with client_id, session_id, owner_pid, revision (positive u64 representable by SQLite), and the existing DetachedSessionDraft value. The client method persist_terminal_draft accepts those arguments and returns Result<()> after Done. Workspace placement comes only from session_contexts in the transaction. Stale/equal revisions are acknowledged without changing stored data. The old combined PersistDetachedSessionState action is removed. The delivered implementation uses schema 123 and daemon protocol 85. The shared DetachedSessionDraft retains text and optional inherited_input.

The terminal cache preserves desired/saved revisions across version-1 upgrade handoff with defaulted fields for older JSON. In-flight handles are process-local; a resumed unacknowledged value is dirty and can safely retry its revision. Each session has at most one in-flight save. Completion acknowledges the sent value only; if a newer value exists its tracked save starts before quit can observe the owner as drained. No retry loop runs merely because a failed value remains dirty: a newer edit or explicit final flush supplies the next attempt.

The final cleanup helper joins settings/workspace persistence and session-manager shutdown and reports each actual duration plus elapsed combined cleanup time. Main retains attachment ownership, cancels and joins maintenance before Detach, and records dashboard destruction, attachment shutdown, connection, and detach-request milestones in the existing single exit record.

Revision note, 2026-11-10: created from the measured quit evidence and current a83864d97 source; draft writer and attachment cleanup design remain explicit implementation milestones.

Revision note, 2026-11-10: settled managed draft revisions, one-time inherited seed handling, recovery-copy semantics, draft-only API, and safe cleanup overlap. Assigned disjoint implementation slices and a real tmux acceptance scenario; no installation requested.

Revision note, 2026-11-10: recorded canonical session-owned placement, removal of stale workspace input, all final validation, the 130 ms final-binary acceptance, clean review, and completed implementation push. Historical recovery cleanup UI in #1386 remains outside this completed quit-latency task.
