# Compact move history since the latest usable summary

This ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as work proceeds.

## Purpose / Big Picture

Moving a session to another harness should reuse the latest complete summary and summarize only the conversation after it. A harness is the coding program running the conversation. Today each move sends the entire canonical transcript (Mjolnir's portable conversation) to the utility model again. This work reuses Mjolnir handoffs and Claude Code's readable native compaction summaries. Codex native summaries and other native formats remain out of scope. Tests will inspect the actual utility-model prompts and prove old raw history is omitted while its summary and subsequent work survive.

## Progress

- [x] (2026-11-01) Confirmed user authorization for generic handoff reuse and Claude native support, with no Codex native work.
- [x] (2026-11-01) Confirmed clean current master and read `.agents/PLANS.md`.
- [x] (2026-11-01) Traced verified checkpoint/native access; selected causal tool identities with conservative overlap rather than timestamps.
- [x] (2026-11-01) Implemented generic handoff persistence, prefix validation, incremental prompts, and same-turn continuation.
- [x] (2026-11-01) Added bounded digest-verified selected checkpoint-payload reader; 169 tests passed and checkpoint slice committed as `48b402e8f`.
- [x] (2026-11-01) Implemented Claude summary parsing, retained exception rows, and source-checkpoint integration on both move routes.
- [x] (2026-11-01) Validated shared transcript persistence: 93 tests passed.
- [x] (2026-11-01) Unified controller context installs on existing durable command stream; added a real DurableRelay client integration regression.
- [x] (2026-11-01) Completed independent integration review and dev-profile validation: checkpoint 169 passed, transcript 93 passed, controller 2,226 passed with 18 ignored; workspace Clippy and formatting passed.
- [ ] Commit changed files on current branch and push to origin/master.

## Surprises & Discoveries

The shared transcript summary currently replaces previous handoff bodies with a placeholder because the old pipeline always included their raw lineage. It must continue doing so for unrelated consumers; compaction needs its own explicit earlier-summary input. A fallback handoff can contain only bounded recent history, so recognizing its opening sentence alone is insufficient proof that older history can be omitted.

The verified checkpoint includes the primary Claude JSONL even when its source worker was remote or containerized. Existing streaming verification discards native payload bodies. A new bounded selected-payload reader in `mj-checkpoint/src/archive.rs` reuses its ZIP entry validation and checks the full SHA-256 on reread. Its 169 passing tests supported checkpoint commit `48b402e8f`.

Timestamp-only native mapping cannot prove a safe cut. Worker journal receipt and Claude JSONL normally share a host clock, but it is adjustable wall time. Materialized tools can also receive terminal output without a timestamp change. The native mapping must use causal content or tool identity anchors, keeping ambiguous canonical items rather than excluding them based on timestamps.

Production tracing corrected the initial generic design: normal handoffs are hidden prompt resources installed with `InstallPromptContext`, never standalone canonical User turns. The initial generic handback's manufactured-user tests therefore did not prove production reuse. Parent took over controller compaction after reviewing two incomplete handbacks and replaced raw-user detection with durable state projected from the actual install command. New `mj-transcript/src/handoff.rs` captures a versioned source cutoff, SHA-256 of the raw semantic canonical prefix, and earlier summary. The install projects this into reserved `configuration.values["mj_handoff_state"]`, preserving the existing wire/schema formats and user-visible transcript. Current-prefix validation detects reused tool IDs or other historical mutations before allowing reuse; `/clear` invalidates the seed. Older unmarked handoffs safely receive one full compaction.

Ordinary context installation had a second, relay-private request route that emitted no event, unlike the existing startup command route. `RelayClient::install_prompt_context` now uses the existing journaled `InstallPromptContext` command for all current controller callers; standalone and actor owners sync its projection, and the actor publishes the refreshed view. This makes the durable state reachable in real moves, not just a manufactured projection test. No worker protocol extension is required. The legacy wire request remains readable for older clients.

## Decision Log

Decision: Support Mjolnir's previous complete handoffs and Claude Code native summaries; leave Codex native parsing unchanged. Rationale: User approved these two sources and explicitly rejected provider-specific Codex support. Date/Author: 2026-11-01, user and Codex.

Decision: Keep retained recent context sourced from the original full transcript and preserve existing full-history behavior when a summary cannot be trusted or mapped. Rationale: Incremental selection must not lose subsequent work or make unavailable native state fatal to a move. Date/Author: 2026-11-01, Codex.

Decision: Preserve the existing persisted native-recovery startup-step format. Rationale: Cross-harness move and resume have the verified checkpoint available; generic previous-handoff reuse also works on native-continuity recovery, which does not persist a native archive reference. Extending that separate durable format is unnecessary for the approved scope. Date/Author: 2026-11-01, Codex.

Decision: Cut before a matched native tool's canonical creation row and retain that tool. Rationale: The pinned Claude bridge preserves tool-use IDs in ACP toolCallId. Tool creation closes prior agent streams. Eager permission-time creation only moves the cut earlier, preserving extra context. This deliberately retains whole rows instead of claiming an exact raw-event boundary. Date/Author: 2026-11-01, Codex.

Decision: Persist generated generic summaries through the real hidden-context install path, bind them to a semantic prefix digest, and decline unmarked legacy summaries as slicing boundaries. Rationale: Summary presence in raw User content was not a production invariant, and positions alone cannot prove old mutable rows still match the earlier summary. Store the state in the existing extensible configuration JSON, reserve its key against harness updates, and preserve it during configuration replacement. Date/Author: 2026-11-01, Codex.

Decision: Keep native unproven rows individually in original canonical order instead of stopping all omission at the first such row. Rationale: Early controller messages or mutable tools must remain available without forcing the entire otherwise-proven pre-summary conversation back through the model. Date/Author: 2026-11-01, Codex.

## Outcomes & Retrospective

The feature is implemented and accepted. The bounded selected-checkpoint reader is committed as `48b402e8f` and passed 169 tests, with 2 ignored. Shared transcript persistence passed 93 tests. The full controller run passed 2,224 tests with 18 ignored and exposed two new-test fixture/assertion defects; both failed tests passed targeted reruns after correction, yielding 2,226 passing controller tests. Workspace `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all -- --check` passed. Successful tests were reused rather than rerunning the full suite for fixture fixes.

The corrected assertion distinguishes old raw data intentionally kept inside the previous handoff's bounded retained context from old history rendered again as new transcript delta. The Claude archive fixture now supplies a valid non-genesis event digest/activity watermark and complete ACP plan fields. No test failure remains. Independent review checked the actual durable relay install route, native archive-to-prompt flow, prefix freshness, configuration isolation, and cancellation. Native parsing checks cancellation between bounded reads and records; explicit cancellation joins the blocking task, while a single capped payload read/hash must finish first.

New handoffs preserve reusable state for every harness; Claude native summaries additionally provide a causal boundary within a turn. Old unmarked handoffs, absent native anchors, ambiguous histories, and changed historical prefixes retain full-history behavior. Codex native parsing was deliberately not added. Delivery is being completed on current master, including merge of unrelated upstream API-routing commit `b7aedfb43`.

## Context and Orientation

`mj-controller/src/compaction.rs::compact_snapshot` groups canonical transcript items into turns, renders bounded pages, summarizes them concurrently, and reduces page summaries into one handoff. A handoff is hidden context attached to the first eligible prompt sent to the target harness. `mj-controller/src/handoff.rs::build_handoff_context` resolves the utility model and falls back to bounded recent context when unavailable. `mj-transcript/src/summary.rs` owns shared handoff preambles and transcript projections; `mj-transcript/src/handoff.rs` owns reusable summary provenance. `mj-controller/src/controller/resume.rs` and its `in_place.rs` module orchestrate source checkpoint creation and target restoration. A checkpoint is a portable archive of the worker's conversation and native state. Claude Code records compaction boundaries and readable summary messages in its native JSONL files; `mj-controller/src/import/transcripts.rs` already recognizes these artifacts for import.

## Plan of Work

Read the current source checkpoint for both ordinary and in-place moves on supervised blocking tasks. Select the primary Claude JSONL by the verified source manifest and native session identity, with a 256 MiB cap. Pair native `compact_boundary` and `isCompactSummary` records. Use a unique native tool-use identity with its result before the boundary to find a corresponding canonical tool; cut immediately before that tool's canonical creation position, deliberately replaying the tool and every later item. Tool creation closes preceding canonical agent streams, so earlier assistant rows cannot accumulate later content across that barrier. Require an unambiguous native/canonical prompt sequence, and individually retain earlier unresolved or terminal-backed tools, unrepresented users, and controller-only messages/plans in canonical order. Never infer exclusion from wall-clock timestamps. Ambiguous identity or history retains generic/full compaction. Read generic handoff state from the reserved canonical configuration key and validate its source-prefix digest. Every complete generated model handoff carries a versioned provenance marker; project it on the journaled hidden-context install command. Pass the previous summary and only subsequent conversation plus native exceptions through the existing single-request or paged reduction pipeline. If a summary is too large, use existing bounded paging instead of bypassing budgets. Poll the resume executor for cancellation, signal native parsing cooperatively, and join its blocking task before explicit cancellation returns. Validate repeated moves, compaction inside a turn, multiple boundaries, clear boundaries, missing or ambiguous native state, and recent-only handoffs.

## Milestones

The first milestone is a precise interface between boundary selection and model compaction, supported by source references and a generic repeated-move regression. Its visible result is that model prompts contain the earlier summary and new work but no old raw lineage. The second milestone connects Claude native summaries from checkpoint archives, proving the same result after a native compaction and preserving all later events. The final milestone reviews the integrated code, runs required tests once after the last changes, and commits and pushes the result.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir/.mj/clones/7e24159814b377fda76aa0d9c6d44ca1`. Preserve existing mbx build configuration and target paths. Run targeted tests with `cargo test -p brokk-mj-controller compaction` outside the restricted sandbox; new binary invocations, if needed, must use a named `--instance incremental-compaction-test`. Run the complete suites for every touched crate (`cargo test -p brokk-mj-checkpoint`, `cargo test -p brokk-mj-transcript`, and `cargo test -p brokk-mj-controller`) and `cargo clippy --all-targets -- -D warnings` in the dev profile after final changes. The finalized checkpoint and transcript crates can be validated while controller integration proceeds. If a check fails, fix it and rerun only the failing test or affected module. Run `cargo fmt --all -- --check`, review `git diff --check`, stage only changed files, commit on current master, and push `HEAD:master`. Merge origin/master if upstream advanced; do not rebase.

## Validation and Acceptance

Recording backends in compaction tests capture prompts without external model calls. Assert that the latest valid earlier summary reaches the backend, old raw history is absent from the new transcript delta, and every post-boundary canonical item remains available. The previous summary can include bounded retained raw context from the earlier handoff. Exercise single-request and multi-page inputs, including inputs above 64 KiB, and preserve model byte budgets. Claude tests cross the verified checkpoint/JSONL/canonical boundary; relay integration verifies actual hidden-context installation persists the state without a synthetic user turn. Existing clear-boundary and tool-retention regressions must continue passing. Full touched-crate tests and workspace Clippy must pass; log any unrelated environmental limitation precisely.

## Idempotence and Recovery

No database migrations or live-session changes are required. Tests use their existing isolated data roots. Missing or unusable native summaries retain the existing full-history path, and summarizer failure retains the existing recent-context handoff. Repeating tests or native parsing must not modify checkpoints. Revert only task-owned changes if needed; preserve unrelated concurrent work.

## Artifacts and Notes

Delegated investigation reports live in `.mj/agents/` and are transient evidence. This plan is the durable record. Validation logs will be kept in agent report directories rather than product documentation.

## Interfaces and Dependencies

Keep existing `CompactionBackend`, `CompactionBudget`, and canonical snapshot ownership. In controller compaction define `PriorCompaction { through_position: u64, summary: String, retained_positions: Vec<u64> }` and `compact_snapshot_since(snapshot, budget, backend, prior: Option<&PriorCompaction>)`. Keep public `compact_snapshot` as the generic wrapper. Choose the newest valid native or generic candidate by canonical position, retaining native exception rows. In `mj-transcript/src/handoff.rs`, `encode_handoff(snapshot, text)` appends a versioned prefix-provenance marker to a complete model handoff; `parse_handoff(text)` recognizes that format for projection; `reusable_handoff(snapshot)` reads and validates the reserved configuration value. The checkpoint API is `read_verified_checkpoint_payload(path: &Path, descriptor: &PayloadDescriptor, max_bytes: u64) -> Result<Vec<u8>>`; callers already hold a verified manifest, and the selected body is size-capped and its full digest rechecked. Reuse JSON parsing libraries. Do not add a workspace crate, daemon schema, worker protocol, or live native-log scan.

Plan created 2026-11-01 to capture approved scope, ownership constraints, and validation before implementation. Integration details will be updated from the delegated source trace.

Plan updated 2026-11-01 after tracing verified archive availability and rejecting timestamp-only cutoff as unsafe; recorded settled shared seed and bounded-reader interfaces.

Plan updated 2026-11-01 to specify native tool-identity barriers, conservative retained overlap, and exact Cargo package names. Checkpoint validation starts independently once that slice is final.

Plan updated 2026-11-01 after production integration review disproved the raw-user handoff assumption. Recorded durable hidden-context state, prefix freshness, controller takeover, retained native exceptions, and checkpoint validation/commit evidence.

Plan updated 2026-11-01 after finding and removing the controller's unjournaled install route. Recorded actual relay integration coverage, transcript validation, source-native parsing on supervised blocking work, and final controller checks.

Plan updated 2026-11-01 after final validation passed. Recorded passing counts, the two repaired test defects, cancellation bounds, independent review results, and remaining delivery step.
