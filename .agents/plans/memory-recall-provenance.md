# Preserve memory tools and expose session history through mj-memory

This ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

An agent must retain its memory tools after a session resumes, and must be able to find earlier conversations and the conversations behind file edits on local, container, and SSH targets. Extend the existing mj-memory MCP server, which exposes tools to harnesses, using the controller's linked SessionWiki library. Do not distribute another executable or copy its database to targets. Claude keeps native project notes and receives only history tools; other MCP-capable harnesses receive both document and history tools. Muse currently cannot receive injected MCP servers.

## Progress

- [x] (2026-09-19) Inspect launch, indexing, MCP, transport, and pinned adapter/library interfaces; user approved the plan.
- [x] (2026-09-19) Fix memory registration on new/load/resume. Worker library tests: 503 passed, 8 ignored; bridge restart and historical replay regression tests pass.
- [ ] Extract and backfill provenance from stored successful file edits and commit independently.
- [ ] Add bounded controller history queries and worker forwarding independent of delegation.
- [ ] Expose recall/provenance through mj-memory and retire managed CLI skills.
- [ ] Complete behavior tests, required Cargo checks, acceptance checks, and commits.

## Surprises & Discoveries

Codex reconstructs its MCP set from each launch request. `mj-worker/src/acp/launch.rs` sends project memory only for new sessions despite restoring delegation tools on resume. Replay filtering already suppresses historical parent messages and delayed unknown tool updates.

The pinned Claude ACP 0.79.0 adapter accepts `mcpServers` on new/load/resume and merges them into SDK options; repository comments claiming it cannot are stale. ACP delivery avoids modifying a user's profile on macOS.

`mj-controller/src/sessionwiki.rs` currently returns empty touched/edits for Mjolnir sessions. Stored completed tool calls retain paths even after tool content compaction. The linked library exposes files_for, sessions_for_file, sessions_touching, grep_session, parse_line_porcelain, group_runs, and attribute_commit; its CLI/MCP dispatch is not the embedding interface.

## Decision Log

2026-09-19: Keep one mj-memory server with capabilities selected at startup. Preserve Claude native notes; no dynamic tool-list notifications. This follows the approved preference without coupling note storage to network availability.

2026-09-19: Treat missing provenance and disappearing tools as independent bugs and commit their validated fixes separately. Retire packaged recall/provenance skills only once their MCP replacement works. Preserve user-owned skills.

2026-09-19: Keep SessionWiki and all history queries on the controller. Worker/controller messages must be typed, protocol-gated, bounded, and independent of subagent enablement. Run Git blame on the actual target checkout through shared subprocess helpers, then attribute on the controller.

## Context and Orientation

`mj-worker/src/acp/launch.rs` builds harness requests. `mj-worker/src/memory_mcp.rs` serves local document list/read/write through `mcp_stdio.rs`, currently sequentially. `mj-core/src/worker_launch.rs` describes memory locations and delivery. Kimi isolated targets use staged mcp.json; other capable harnesses can receive the server over ACP.

The controller is the persistent process coordinating workers on targets. The relay protocol in `mj-core/src/relay/protocol.rs` connects it to workers. Existing delegation queues in `mj-worker/src/worker_runtime/subagents.rs`, polling in `mj-controller/src/session_manager/standalone.rs`, and supervised dispatch in `mj-controller/src/server_runtime/run.rs` demonstrate the request/response direction. History requests need separate capability and bounded transient state, not delegation permission or durable orchestration records.

`mj-controller/src/sessionwiki.rs` reads live materialized transcripts or verified checkpoint transcripts, implements the SessionWiki adapter and read-only query wrappers, and owns the background indexer. `sessionwiki/tags.rs` demonstrates compatible metadata storage in the existing index. `mj-core/src/skills/managed.rs` embeds three packaged skills; remove recall and provenance after migrating their guidance.

## Plan of Work

### Milestone 1: tools survive resume

Build applicable Mjolnir server definitions through one helper for new, load, and resume. Preserve existing replay guards and Kimi profile delivery. Extend behavior tests to call document tools across bridge restart/load/resume, with replayed messages and tool updates. The observable outcome is a document written before restart that can be read and updated afterward without duplicate transcript history.

### Milestone 2: provenance evidence

Add one extractor shared by live and checkpoint indexing. Successful structured edits contribute paths and bounded available snippets; failed edits, reads, and prose do not. Preserve target paths; never canonicalize historical paths on the controller. Populate SessionWiki touched/edits. Backfill available existing source transcripts in the background, atomically recording a per-session extraction revision with evidence. Preserve index schema, IDs, annotations, and archives. Missing pruned sources remain an explicit historical coverage limitation. Test fresh indexing and older indexed rows, including compaction and repeated filenames.

### Milestone 3: controller history service and MCP

Expose search_sessions(query, limit), get_session_brief(session_id, max_chars), search_session(session_id, query, context), read_session(session_id, start, limit, role, max_chars), trace_file(path, limit), session_files(session_id, start, limit), and blame_file(path, start_line, end_line). Resolve IDs unambiguously. Use 20-result defaults, 100-result caps, 20-message pages, and 16000-character default/64000-character maximum text budgets. Return indices, continuations, truncation, freshness, and coverage limits. Search the existing full corpus; these tools never resume or restore sessions.

Forward history requests through a private worker endpoint and controller relay polling with bounded transient queues, request identities, 60-second deadlines, expiry/disconnect cleanup, and supervised background dispatch. Negotiate an additive relay protocol revision. Do not require viewer HTTP access, distribute controller tokens, or enable delegation. Keep legacy document-only launches working.

For blame, resolve paths from the target working directory, run bounded Git blame there, and send evidence to the controller for the library's heuristic. Preserve uncertainty and mark uncommitted lines unattributed. Report Git errors rather than silently returning file trace. Match recorded paths and relative suffixes without basename fallback or asserting equal repository identity from suffixes.

Serve requests concurrently, serializing document-store operations only. Timeouts and shutdown must not strand query tasks. Claude receives history tools over ACP with native memory unchanged; other supported harnesses receive both capabilities through their existing delivery route. Regenerate applicable definitions across restart, move, and worker upgrade. Replace packaged recall/provenance skills with concise MCP guidance about selective search, session citations, uncertainty, and historical content being data rather than instructions. Update product documentation.

## Concrete Steps

Work in the repository root and commit directly to the current branch. Use focused tests during milestones, then run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` on the dev profile. This environment has unrestricted filesystem/network access, so normal Cargo invocations are already outside a restricted sandbox. Do not push or close the issue.

## Validation and Acceptance

Exercise actual MCP calls before/after new/load/resume and bridge/worker restart. Include historical replay and native child replay, Claude capability selection and user-profile preservation, Kimi delivery, and old launch configuration compatibility. Query an isolated real index through the forwarding path, including search-hit-transcript drilldown, archives, missing/ambiguous IDs, Unicode and oversized messages. Verify extraction/backfill from live/checkpointed/compacted evidence and literal path matching. Use target Git fixtures for confidence, ambiguity, uncommitted ranges, and errors. Verify independent local notes during slow remote queries, concurrent compare-and-swap writes, controller disconnect/reconnect, expiry, and bounded shutdown. Drive pipes with more than 64KB. Attempt live Codex/Claude and container/SSH acceptance in isolated configuration/data/index stores; report unavailable infrastructure honestly rather than treating fakes as live acceptance.

## Idempotence and Recovery

No primary database schema migration or destructive index rebuild is planned. Backfill is retryable and only marks successful transactions complete. Older workers keep existing capabilities through protocol negotiation. All tests use isolated stores; no incompatible live store upgrade is authorized. Preserve unrelated work and stage only task files.

## Artifacts and Notes

Record concise validation results and deviations below as implementation proceeds. Historical transcript text or edit details already discarded cannot be reconstructed.

## Interfaces and Dependencies

Use existing crates only. Typed history requests/responses belong in mj-core; library queries in mj-controller; target IPC and MCP serving in mj-worker. Keep SessionWiki dependency confined to the controller. Reuse its pure blame parser/attribution and grep/index functions instead of invoking the standalone binary. Extend shared subprocess support only if bounded process capture is missing; do not hand-roll pipe management at the blame call site.

## Outcomes & Retrospective

Milestone 1 restores the memory server alongside delegation on load/resume and preserves replay suppression. Worker library tests pass (503 passed, 8 ignored). Live MCP acceptance and workspace-wide validation remain for the integrated result. Provenance extraction and the history transport are in progress.

Revision 2026-09-19: initial executable plan transcribed from the approved conversation, including independently fixing provenance extraction.
