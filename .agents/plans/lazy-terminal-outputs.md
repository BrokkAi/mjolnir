# Attach terminal output only to live owners and join parked output when reading

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

The daemon spends about 9% of its sampled CPU (2026-11-12 frame-pointer profile, `/tmp/mj-daemon-regression-20261111/opus-fp2-fold.txt`) scanning every stored transcript body of a session for `terminal_refs` whenever an event mentions a terminal. The scan exists only to find the tool that owns a terminal so the projector can move the terminal's output into that tool. After this change, terminal output is stored once as its own transcript item and never moved; every reader joins a tool to its output by terminal ID through one shared accessor. The reverse scan disappears, output-before-tool and tool-before-output orderings end in the same stored state without a claim step, and transcript windows no longer decide whether output attaches. Users see no rendering change: output still appears under the command that ran it.

Separately, the daemon gains SQLite planner statistics: the store has never been analyzed (no `sqlite_stat1`), so the planner guesses row counts.

## Progress

- [x] (2026-11-12) M1: `PRAGMA optimize=0x10012` on the writer connection after migrations (`02c10fb8d`). Workspace Clippy and 248 database tests passed. First run on a 5.4 GB copy of the five largest tables took 26.8 s; a repeat took 0.00 s.
- [x] (2026-11-12) M2: one terminal-output accessor (`TerminalOutputs` in `mj-core/src/transcript.rs`), all readers converted, presentation-only hydration of tools in page reads (`e13ed5c7c`). Existing goldens unchanged.
- [x] (2026-11-12) M3 (revised): store reverse scan and `HistoricalReferences.terminals` deleted; live-owner attachment, Kimi matching, fallback finalization and ACP metadata unchanged (`ea047e98d`). Final run: 98 transcript, 54 client unit + integration, 774 selected controller and 21 selected core tests; workspace Clippy, fmt and diff checks passed.
- [x] (2026-11-12) First-run `optimize` cost accepted as a one-time startup delay (see Surprises).
- [ ] Review, push, install, profile.

## Surprises & Discoveries

- Observation: the worker records exactly one `TerminalOutput` per terminal, when it closes, carrying the tail-capped final output.
  Evidence: `mj-worker/src/worker_runtime/unix/dispatch.rs`, `RuntimeEvent::TerminalClosed`.
- Observation: a second producer writes terminal output: ACP display-terminal metadata on tool calls and updates, streamed incrementally into the tool's own `terminal_outputs`.
  Evidence: `mj-transcript/src/projection/terminals.rs:6–108` (`apply_terminal_metadata`), called from `session_update.rs:53,72,127`; protected by `terminal_metadata_retains_bounded_unicode_output_across_restore_and_late_exit`.
- Observation: more readers than first listed: `mj-chat/src/chat/transcript.rs:315`, `mj-controller/src/server/api/turns.rs:57,436`, `mj-controller/src/database/events.rs:671`, and canonical tool-body consumers (`summary.rs`, archive validation, `handoff/claude.rs`).
- Observation: in the three largest live sessions every terminal has exactly one referring item (39,443 terminals, 0 with more than one referrer), although `ProjectionIndex::terminal_referrers` allows a set.

- Observation: the first `optimize` is dominated by an exact row count of each index, which `analysis_limit` does not bound; it is I/O, not CPU.
  Evidence: on a 5.1 GB copy of `materialized_transcript_items`, `optimize=0x10012` took 28.5 s wall, 0.28 s user, 10.4 s system, and wrote exact `sqlite_stat1` counts (4,762,090); `analysis_limit=100` plus `ANALYZE` took 19.5 s; a warm `count(*)` took 4.6 s; a repeat `optimize` took 0.00 s. `ANALYZE` holds the write lock, so moving it after startup would stall writes instead.

## Decision Log

- Decision: run `PRAGMA optimize=0x10012` once, on the writer connection, right after migrations. No periodic run.
  Rationale: user decision (statistics change slowly; daemon restarts on every install). Reader connections are `query_only` and cannot write `sqlite_stat1`. SQLite's suggested `0x10002` omits bit `0x10` (temporary `analysis_limit`), so the first run on the 16 GB store would analyze every index without a bound; `0x10012` keeps the bound.
  Date/Author: 2026-11-12, user + parent agent.
- Decision: terminal output is always stored as the standalone `terminal:<id>` item (`TranscriptBody::TerminalOutput`). The projector never copies it into a tool and never removes it because a tool claimed it.
  Rationale: removes owner lookup at projection time, the reverse scan, and the ordering-dependent claim logic that produced findings in three review rounds.
- Decision (amends the previous one): ACP display-terminal metadata (`apply_terminal_metadata` in `mj-transcript/src/projection/terminals.rs`: `terminal_output`, `terminal_output_delta`, `terminal_exit`) keeps writing `Tool.terminal_outputs` with its existing snapshot, delta, restore and UTF-8 rules. "Always standalone" applies to `RelayObservation::TerminalOutput` only. The accessor resolves each terminal ID by preferring a standalone `terminal:<id>` record over an embedded one; otherwise it yields the embedded record. Order: embedded order, then standalone records for referenced IDs not embedded, in `terminal_refs` order.
  Rationale: metadata output arrives on the owning tool's own update, so there is no owner lookup to remove; moving it out would make every streamed delta write a second item. The worker's close-time record is final, so it wins when both exist. Embedded records are therefore a live form, not only legacy.
  Date/Author: 2026-11-12, parent agent, after the implementer found the second producer.
- Decision (supersedes "always standalone" and the window-closure decision): reconcile only for watched tools. When `RelayObservation::TerminalOutput` arrives, the projector attaches it to referring tools that are in the live projection (`ProjectionIndex::terminal_referrers`, in memory) and to a window-local Kimi raw-result owner, exactly as today; otherwise it parks the output as `terminal:<id>`, and a later tool naming the terminal still claims it by key. The store reverse scan in `load_projection_references_from` and the `terminals` set from `historical_references` are deleted. Readers join parked output to its tool through `terminal_refs` with the accessor; page and API builders that return tools fetch the referenced `terminal:<id>` items by key as presentation context. No window closure is needed: a retained tool can only reference a parked item when its owner was evicted, and then the owner is not retained either.
  Rationale: the implementer found that transcript consumers are incremental and render each item from its own data (cursor-paged and role-filtered transcript API in `database/materialized.rs:607–750` and `server/api/turns.rs:424–444`, the browser projector cache, TUI chat entries). Under full lazy storage, output arrival never changes the tool item, so consumers that already rendered the tool would never show its output without tracking dependencies between items. Owners of running terminals are pending tools, which the window keeps live, so the in-memory index covers nearly every case. Accepted trade-off: whether output is attached or parked still depends on whether the owner was live; late output for an evicted owner shows as a standalone block in the live view and under the tool in history views.
  Date/Author: 2026-11-12, user decision ("switch") on the parent agent's recommendation.
- Decision: one accessor type in `mj-core/src/transcript.rs`, built from the items a reader holds, answers "which output records belong to this tool" and "is this standalone terminal item shown under a tool". Every reader uses it; nobody reads `terminal_outputs` directly except the accessor.
  Rationale: AGENTS.md shared-interpretation rule; makes M3 a change to one function plus the projector. A SQL view would not reach the in-memory readers (actor window → runtime feed → mj-client), which are the hot path.
- Decision: keep reading the legacy embedded form (`Tool.terminal_outputs`) through the accessor; no data migration.
  Rationale: rewriting every historical tool body in a 16 GB store costs a long blocking migration and buys nothing the accessor cannot do. Older daemons and clients already render unclaimed standalone terminal items, so a downgrade shows output as a separate block, never loses it; an older projector re-embedding output on a later tool update is harmless. No schema change, no relay or API protocol change: compatible.
- Decision: the fallback compatibility tool (`hel-terminal:` items from `mj_core::acp::fallback_terminal_tool_call`) is still finalized eagerly from its terminal's exit status, by key lookup of `fallback_terminal_tool_item_id(terminal_id)`. Kimi's raw-output matching becomes association only: when a completed tool's raw result uniquely matches a standalone terminal item in the live projection, or a `TerminalOutput` uniquely matches a completed raw tool, the projector appends the terminal ID to that tool's `terminal_refs` (and removes the duplicate fallback tool as today). It never moves output.
  Rationale: both are key lookups or window-local, no history scan; matching semantics stay as they are (`matches_tool_raw_result` accepts completed/failed only).
- Decision: any item set the daemon keeps live is closed under terminal references. `ProjectionWindow::trim` and the actor loader keep every `terminal:<id>` item referenced by a retained tool. Readers that page history build the accessor over everything they hold.
  Rationale: a kept old unsettled turn can reference a terminal item positioned in a dropped later turn. Terminal items are normally newer than their tool, so newest-first tails and pages already contain them; output-before-tool orderings appear once the older page loads.

- Decision: page and history reads hydrate the tools they return with joined terminal records (primary-key reads of `terminal:<id>`), presentation only, never written back; `TranscriptHistoryPage` is a public wire type, so hydration keeps remote clients correct without a wire field. Clients suppress a standalone item whose terminal a held tool shows. Archive validation reads the accessor's raw embedded view so shadowed embedded records are still validated.
  Rationale: role-filtered and cursor-paged readers cannot see parked items; hydration preserves page shape, cursors, filters and the response format.
  Date/Author: 2026-11-12, implementer proposal approved by the parent agent.

- Decision (amends page hydration): partial views render items as stored and never join across chunks: the live client tail, the asynchronously converted prefix, history pages and unfiltered API pages. Output appears exactly once, under its tool when the owner was live and as a standalone block otherwise. Only readers that hold a complete transcript (archive export and validation, handoff, summaries and SessionWiki) or that filter out standalone terminal items (`role=tool` API pages, the GitHub tool-only reader) join and hydrate through the accessor.
  Rationale: review found that joining in partial views either loses output (tool in the async prefix, parked record in the tail) or duplicates it (owner and parked record on different history pages), and that per-session join state disabled TUI entry reuse whenever any parked record existed. Owners of parked output are evicted from the live window, so the live tail gains nothing from a join.
  Date/Author: 2026-11-12, parent agent after review of e13ed5c7c.

## Outcomes & Retrospective

(none yet)

## Context and Orientation

Terms. An ACP terminal is a shell command the worker runs on an agent's behalf (`terminal/create`, `mj-worker/src/terminal.rs`). A tool call names terminals in its content; the projector records them in `TranscriptBody::Tool.terminal_refs` (`mj-core/src/transcript.rs`). `TerminalOutputRecord` (`mj-core/src/transcript.rs:224`) holds terminal ID, output, truncation flag, exit code and signal. Today `RelayObservation::TerminalOutput` is projected in `mj-transcript/src/projection/observation.rs` (the `TerminalOutput` arm): it writes the record into every referring tool's `terminal_outputs` (found through `ProjectionIndex::terminal_referrers`), or into a Kimi raw-result owner (`uniquely_matching_raw_tool`, `mj-transcript/src/projection/terminals.rs`), or parks it as `terminal:<id>`. Tool calls that name terminals consume parked items (`attach_terminal_outputs`, `uniquely_matching_raw_terminal`, terminals.rs).

The live projection is a window of the stored transcript (`ProjectionWindow::trim`, `mj-core/src/state.rs`; loader `load_materialized_actor_projection_from`, `mj-controller/src/database/materialized.rs`). Before each event batch, `load_projection_references_from` (materialized.rs) rehydrates referenced items; its terminal branch runs the reverse scan `EXISTS(SELECT 1 FROM json_each(body_json,'$.terminal_refs') ...)` over every non-retained body. `historical_references` (`mj-transcript/src/projection.rs`) produces the `terminals` set that triggers it.

Readers of `terminal_outputs` today: `mj-client/src/transcript.rs` (browser/TUI entries, ~line 460–510), `mj-transcript/src/transcript.rs` (~1300–1350 and ~1717–1730), `mj-transcript/src/summary.rs` (two sites, ~273 and ~367; also feeds SessionWiki through `summary::indexed_tool_call`), `mj-controller/src/database/github_watch.rs` (~572), `mj-controller/src/handoff/claude.rs` (~238), `mj-core/src/archive.rs` (canonical export, ~628). Construction sites that set `terminal_outputs: Vec::new()` are not readers.

The writer connection is opened in `start_database_writer_at` (`mj-controller/src/database/writer.rs:234`) via `schema::open_writer`; readers are opened `SQLITE_OPEN_READ_ONLY` with `PRAGMA query_only = ON` (`mj-controller/src/database/schema.rs` ~304–312).

## Plan of Work

M1 (independent files). After migrations complete on the writer connection, execute `PRAGMA optimize=0x10012`, log its duration at INFO, and report failure as a warning without blocking startup (statistics are an optimization; a failure must still be visible). Before landing, measure the first run against copies of the largest live tables (read-only source; copy tables plus their indexes into a scratch database outside `target/`; never `.backup` or `VACUUM` the live store) and record the time here.

M2. Add the accessor next to `TranscriptBody` in `mj-core/src/transcript.rs`. Suggested shape (the implementer may refine it, keeping one owner): `TerminalOutputs::of(items)` indexes standalone `TerminalOutput` records by terminal ID and the set of terminal IDs referenced by tools; `for_tool(&item)` yields the tool's records (embedded legacy records first, then records of its `terminal_refs` not already embedded, deduplicated by terminal ID); `shown_under_tool(&item)` says whether a standalone terminal item is referenced by a tool in the set, so renderers skip it as a separate block. Convert every reader listed above. With today's stored data the output is identical, because the projector still consumes parked items; existing golden files must not change.

M3. Projector: in the `TerminalOutput` arm, upsert `terminal:<id>` (preserving position and creation time of an existing item), finalize the fallback tool by key, and record Kimi raw associations in `terminal_refs` only. Tool calls and updates naming terminals keep recording `terminal_refs` and stop consuming parked items. Delete the terminal reverse scan from `load_projection_references_from` and the `terminals` output of `historical_references`, keeping key fetches of `terminal:<id>` and the fallback item for `TerminalOutput` events. Add terminal-reference closure to `ProjectionWindow::trim` and the actor loader with one shared rule. Update the `TerminalOutput` doc comment. Remove `ProjectionIndex::terminal_referrers` if nothing needs it.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir/.mj/clones/da692b0e2ccfbac1c96600a01ee1b177` on `master`. Run cargo outside the sandbox. Use one package selection for all test runs: `cargo test -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-client -p brokk-mj-controller -- <filters>`, plus `-p brokk-mj-tui` if a TUI golden is touched. Focused filters while iterating; after the last code edit, workspace `cargo clippy --all-targets -- -D warnings` and the touched modules' tests once.

## Validation and Acceptance

- Golden: an existing transcript golden (TUI render or web viewer) covers a command whose output arrives after the tool and one whose output arrives before; rendered output is unchanged across M2 and M3 (output under the tool, no separate block).
- Integration: a store holding legacy embedded outputs and new standalone outputs renders both through the accessor identically.
- Tricky conditions: both orderings produce identical stored items; an old pending tool kept in a sparse window keeps its referenced terminal item; Kimi raw association in both orderings (adapt `history_window_keeps_pending_turn_for_late_completion`); fallback tool finalizes on exit.
- Remove tests that only exercised the reverse scan or parked-item consumption.
- After install, a `cpu-clock` frame-pointer profile shows no `load_projection_references_from` terminal scan.

## Idempotence and Recovery

No schema or data migration; reverting the commits restores the old projector, and the old projector reads the new standalone items as parked output. `PRAGMA optimize` is safe to repeat.
