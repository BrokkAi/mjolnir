# Preserve memory tools and expose session history through mj-memory

This ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

An agent must retain its memory tools after a session resumes, and must be able to find earlier conversations and the conversations behind file edits on local, container, and SSH targets. Extend the existing mj-memory MCP server, which exposes tools to harnesses, using the controller's linked SessionWiki library. Do not distribute another executable or copy its database to targets. Claude keeps native project notes and receives only history tools; other MCP-capable harnesses receive both document and history tools. Muse currently cannot receive injected MCP servers.

## Progress

- [x] (2026-09-19) Inspect launch, indexing, MCP, transport, and pinned adapter/library interfaces; user approved the plan.
- [x] (2026-09-19) Fix memory registration on new/load/resume. Worker library tests: 503 passed, 8 ignored; bridge restart and historical replay regression tests pass.
- [x] (2026-09-19) Extract/backfill successful file evidence from live and checkpointed transcripts; focused controller tests pass (31 tests), followed by the full controller suite. Committed separately as 5b4d657c.
- [x] (2026-09-19) Add typed, protocol-gated controller history queries and a bounded worker queue independent of delegation.
- [x] (2026-09-19) Wire history-only Claude and combined MCP capabilities, Kimi target socket delivery, and retire packaged recall/provenance skills while preserving user copies.
- [x] (2026-09-19) Complete behavior tests, required Cargo checks, available acceptance checks, and separate implementation commits; publish to origin/master under the user's explicit instruction.

## Surprises & Discoveries

Codex reconstructs its MCP set from each launch request. At inspection, `mj-worker/src/acp/launch.rs` sent project memory only for new sessions despite restoring delegation tools on resume. Replay filtering already suppressed historical parent messages and delayed unknown tool updates.

The pinned Claude ACP 0.79.0 adapter accepts `mcpServers` on new/load/resume and merges them into SDK options; repository comments claiming it cannot are stale. ACP delivery avoids modifying a user's profile on macOS.

At inspection, `mj-controller/src/sessionwiki.rs` returned empty touched/edits for Mjolnir sessions. Stored completed tool calls retain paths even after tool content compaction. The linked library exposes files_for, sessions_for_file, sessions_touching, grep_session, parse_line_porcelain, group_runs, and attribute_commit; its CLI/MCP dispatch is not the embedding interface.

## Decision Log

2026-09-19: Keep one mj-memory server with capabilities selected at startup. Preserve Claude native notes; no dynamic tool-list notifications. This follows the approved preference without coupling note storage to network availability.

2026-09-19: Treat missing provenance and disappearing tools as independent bugs and commit their validated fixes separately. Retire packaged recall/provenance skills only once their MCP replacement works. Preserve user-owned skills.

2026-09-19: Keep SessionWiki and all history queries on the controller. Worker/controller messages must be typed, protocol-gated, bounded, and independent of subagent enablement. Run Git blame on the actual target checkout through shared subprocess helpers, then attribute on the controller.

2026-09-19: Poll history work from each existing standalone worker connection and supervise query tasks there. Limit the worker to eight outstanding requests and the controller to four blocking queries across workers. A disconnected MCP caller drops its request; an unavailable controller yields a deadline error. Local document calls use a separate mutex and stay responsive. Reap finished MCP call threads during normal operation and close history sockets on stdin EOF before joining remaining calls.

2026-09-19: The user explicitly requested publication to origin/master. The resume fix was pushed in merge commit 7968c56b; finish, validate, commit, and push the remaining implementation to the same destination.

2026-09-19: A concurrent upstream context-clear change also allocated relay protocol 15. After merging it, history uses protocol 16 while clear remains at 15. Negotiating with an upstream-only version-15 worker must not send history requests. The merge introduces upstream's separate database/archive revisions; validation continues to use isolated stores.

## Context and Orientation

`mj-worker/src/acp/launch.rs` builds harness requests. `mj-worker/src/memory_mcp.rs` serves local document list/read/write and forwarded history calls concurrently through `mcp_stdio.rs`, serializing document operations with their own mutex. `mj-core/src/worker_launch.rs` describes memory locations and delivery. Kimi isolated targets use staged mcp.json; other capable harnesses receive the server over ACP.

The controller is the persistent process coordinating workers on targets. The relay protocol in `mj-core/src/relay/protocol.rs` connects it to workers. Existing delegation queues in `mj-worker/src/worker_runtime/subagents.rs`, polling in `mj-controller/src/session_manager/standalone.rs`, and supervised dispatch in `mj-controller/src/server_runtime/run.rs` demonstrate the request/response direction. History requests need separate capability and bounded transient state, not delegation permission or durable orchestration records.

`mj-controller/src/sessionwiki.rs` reads live materialized transcripts or verified checkpoint transcripts, implements the SessionWiki adapter and read-only query wrappers, and owns the background indexer. `sessionwiki/provenance.rs` extracts and backfills file evidence; `sessionwiki/history.rs` serves the controller queries. `sessionwiki/tags.rs` demonstrates compatible metadata storage in the existing index. `mj-core/src/skills/managed.rs` now embeds only the mj skill; recall/provenance guidance lives in the MCP instructions and tool descriptions.

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

Work in the repository root and commit directly to the current branch. Use focused tests during milestones, then run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` on the dev profile. This environment has unrestricted filesystem/network access, so normal Cargo invocations are already outside a restricted sandbox. Push the validated commits to origin/master as explicitly requested; do not close the issue.

## Validation and Acceptance

Exercise actual MCP calls before/after new/load/resume and bridge/worker restart. Include historical replay and native child replay, Claude capability selection and user-profile preservation, Kimi delivery, and old launch configuration compatibility. Query an isolated real index through the forwarding path, including search-hit-transcript drilldown, archives, missing/ambiguous IDs, Unicode and oversized messages. Verify extraction/backfill from live/checkpointed/compacted evidence and literal path matching. Use target Git fixtures for confidence, ambiguity, uncommitted ranges, and errors. Verify independent local notes during slow remote queries, concurrent compare-and-swap writes, controller disconnect/reconnect, expiry, and bounded shutdown. Drive pipes with more than 64KB. Attempt live Codex/Claude and container/SSH acceptance in isolated configuration/data/index stores; report unavailable infrastructure honestly rather than treating fakes as live acceptance.

## Idempotence and Recovery

No primary database schema migration or destructive index rebuild is planned. Backfill is retryable and only marks successful transactions complete. Older workers keep existing capabilities through protocol negotiation. All tests use isolated stores; no incompatible live store upgrade is authorized. Preserve unrelated work and stage only task files.

## Artifacts and Notes

Record concise validation results and deviations below as implementation proceeds. Historical transcript text or edit details already discarded cannot be reconstructed.

2026-09-19: Focused worker history tests pass (15 tests), covering actual MCP framing and worker socket framing with 180 KB Unicode responses, local compare-and-swap note writes during delayed history calls, cancellation on MCP EOF and socket disconnect, queue limits/deadlines, target Git errors/uncommitted lines, and existing native-child replay behavior. An isolated live Codex ACP probe called history search and note read successfully on session/new and again on session/load after terminating and restarting the bridge. This probe used the built MCP executable with a controlled history reply; the real SessionWiki query and controller polling are covered separately by isolated-index tests.

Live Claude is unavailable because this environment has neither Claude credentials nor a Claude API/setup-token environment variable. Docker and Podman are absent and localhost SSH refuses connections, so remote-target acceptance is limited to automated launch/transport fixtures. No live stores or source profiles were modified.

The full-build environment exposed an mbx 1.12.0 shared-shim race: `/mnt/nvme/mbx/shims/mbx-cc` was repeatedly replaced with links into other disposable workers that are absent in this container. Required checks use `MBX_DISABLE=1` with the existing target directory to avoid that race. They also unset inherited `NO_COLOR=1`, which invalidates existing terminal-color assertions. Neither workaround changes the repository's build configuration.

The container exposes 96 CPUs to libtest but has a 24-CPU cgroup quota. A default-concurrency run passed the full controller suite (1,494 tests) but hit one-second deadlines in seven existing worker relay tests. Final validation uses `RUST_TEST_THREADS=4`; no production behavior or test deadline was changed to accommodate contention. The controller integration fixture now models transient history polling explicitly; durable-only relays report an empty history queue, matching their existing delegation-queue behavior.

Checks on the implementation before the publication merge all exited 0: `env -u NO_COLOR MBX_DISABLE=1 RUST_TEST_THREADS=4 cargo test`, `env MBX_DISABLE=1 cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. The full run passed all seven worker tests that timed out at default concurrency.

After merging upstream 24db400e and separating the history protocol at 16, the shared managed target directory disappeared during validation, terminating both checks. Rebuild and validate with `MBX_DISABLE=1 CARGO_TARGET_DIR=target-memory-validation CARGO_BUILD_JOBS=16`, additionally unsetting NO_COLOR and setting RUST_TEST_THREADS=4 for tests. This workspace-local target is outside the shared managed cache. The final logs are `target-memory-validation/tests.log` and `target-memory-validation/clippy.log`; the recorded results here are the durable validation record.

Final validation of the merged implementation passed from the workspace-local target: the full dev-profile test suite with four test threads, Clippy on all targets with warnings denied, rustfmt check, and diff whitespace check. This includes the regression test that permits protocol-15 context clearing while withholding protocol-16 history requests from older workers.

## Interfaces and Dependencies

Use existing crates only. Typed history requests/responses belong in mj-core; library queries in mj-controller; target IPC and MCP serving in mj-worker. Keep SessionWiki dependency confined to the controller. Reuse its pure blame parser/attribution and grep/index functions instead of invoking the standalone binary. Extend shared subprocess support only if bounded process capture is missing; do not hand-roll pipe management at the blame call site.

## Outcomes & Retrospective

All implementation milestones are complete. Applicable memory tools survive new/load/resume; recall and provenance are seven bounded tools in the existing mj-memory server, backed by the controller's linked SessionWiki library. Claude retains native notes; other capable harnesses receive both notes and history. Provenance extraction and a compatible, retryable repair populate retained file evidence without a schema upgrade or index reset. Packaged CLI skills are retired and user-owned skills remain intact.

Full Rust tests, Clippy, formatting, and whitespace checks pass. Automated checks cover real-index controller polling, worker and MCP wire transport, large replies, Unicode continuation, literal paths, target Git, concurrent notes, cancellation, expiry, capability selection, and replay. A live isolated Codex session read both history and notes before and after bridge restart. Live Claude and remote-target acceptance remain environment limitations described above, rather than unimplemented features. The resume fix was already published; the provenance fix and MCP implementation are committed separately for the requested origin/master publication.

Revision 2026-09-19: initial executable plan transcribed from the approved conversation, including independently fixing provenance extraction.

Revision 2026-09-19: completed the migration, documented supervision and shutdown decisions, recorded live/automated validation and environment limitations, and updated publication instructions to match the user's explicit push request.
