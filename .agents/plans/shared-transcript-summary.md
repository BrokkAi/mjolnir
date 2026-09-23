# Share transcript summaries throughout mj

This ExecPlan follows `.agents/PLANS.md` and must be maintained throughout implementation.

## Purpose / Big Picture

Mj currently sends raw historical command text to Jev and compaction, while reviews and SessionWiki invent other projections. One deterministic view will retain eight newest distinct tool calls with arguments/results and reduce older calls to parsed names and outcomes. Every consumer uses the same filtering and explicit size-truncation rules. Source transcripts remain intact.

## Progress

- [x] (2026-09-20) Inspected consumers and Codex reference; agreed eight-call temporary tail and explicit truncation markers.
- [x] (2026-09-20) Implemented shared projection, adapters, and behavior tests.
- [x] (2026-09-20) Migrated compaction, reviews, indexing, and Jev; added compatible proxy route.
- [x] (2026-09-20) Filed BrokkAi/mjolnir#1110 and replayed the authorized bifrost2 evidence (18 requests).
- [x] (2026-09-20) Finished integration validation; commit and authorized upstream push are the final publication steps.

## Surprises & Discoveries

ACP has individual call/message IDs but no model sub-turn grouping. The live bifrost2 metadata has terminal information, not grouping. Current compaction instead follows OpenCode output protection and preserves two complete user turns only on the paged path. Codex rebuilds initial context separately and retains user/instruction messages newest-first under a budget. Canonical mj transcripts do not store harness system/developer instructions; preserve the existing startup machinery rather than inventing roles for operational system messages.

## Decision Log

The user chose names plus outcomes for older calls, full eight newest calls as a temporary substitute for explicit sub-turn groups, all mj summary consumers, Codex-style recent-message retention, and explicit truncation when full bodies exceed budgets. File a follow-up issue for real grouping. Keep classifier decision thresholds/transitions unchanged. Keep native transcripts and raw transcript inspection intact. No live configuration edits or production deployment. The user subsequently authorized pushing when complete.

## Context and Orientation

`mj-transcript` already owns shell-name parsing and relay projection and depends on `mj-core`; core cannot depend back on it. Move the process-local TurnContext accumulator from core into transcript. Controller compaction, review seeds and SessionWiki adapters already can use transcript. Jev proxy is independently deployed TypeScript in `services/jev-proxy`; preserve v1 clients and add v2 evidence.

## Plan of Work / Milestones

First add a typed summary in `mj-transcript/src/summary.rs`, with canonical/materialized/live inputs. Call identity and original position determine the last eight; updates merge without promotion. Older calls contain parsed name, status and known exit code/signal, never raw bodies. A common renderer bounds excerpts on UTF-8 boundaries with omission markers. Tests prove call nine demotes call one and live/snapshot parity.

Second migrate controller consumers. Compaction sanitizes before paging and applies shared retained-history rules on both single and paged paths and fallback. Reviews preserve their selection frontier and user intent extraction while deriving trajectories from the summary. SessionWiki uses summary entries for both snapshot and live inputs and versions its indexing change token. UI tool names continue using existing common parsing and full details remain inspectable.

Third move TurnContext, preserving generation/counters and cancellation semantics. Build Jev evidence with the common summary and a 48 KiB text budget within a 64 KiB serialized request. Add /v2/turn-verdict, preserve /v1 validation/questions, and update direct and hosted clients together. Log dimensions and verdicts instead of whole payloads. Proxy deployment must precede a release using v2; do not deploy in this task.

Finally file a GitHub issue in BrokkAi/mjolnir for explicit adapter sub-turn metadata, replay the authorized bifrost2 evidence, and complete validation. Update this plan with evidence and limitations, stage only task files, and commit on the current branch and push its configured upstream.

## Concrete Steps / Validation and Acceptance

Work from `/home/jonathan/Projects/hel`. Run focused crate tests as each milestone stabilizes, then `cargo test` outside the restricted sandbox and `cargo clippy --all-targets -- -D warnings` on dev. Run `cargo fmt --all -- --check`. In `services/jev-proxy`, run `npm test`, `npm run check`, and `npm run deploy:dry-run`. Executable integration uses `--instance transcript-summary-test` and isolated directories.

Behavior coverage includes partial updates, late completion, parallel calls, terminal results, failures, Unicode, malformed records, context boundaries, and payloads larger than 64 KiB. Cross-consumer tests prove earlier raw argument/output markers never return via fallback or exact-tail handling. Proxy tests cover old clients, new clients, limits, unknown fields, and direct/hosted question parity. Classifier replay records sizes/confidence without deterministic probability assertions.

## Idempotence and Recovery

Only derived data changes; raw transcripts and checkpoints remain authoritative. No live store or config is opened for writing during tests. Version derived indexing tokens, not database schema. Keep unrelated untracked files untouched. Revert task commits to undo the code; no remote deployment is part of this change.

## Interfaces and Dependencies

The common typed TranscriptSummary exposes snapshot/materialized construction, tool outcome interpretation, chronological entries, and budgeted rendering. Adapters may select a context/review range and specify budgets; they must not redefine tool filtering. Use existing mj-transcript crate and promote its worker dev dependency to a runtime dependency. New Jev wire evidence uses a transcript_summary field and versioned route while retaining lifecycle fields.

## Artifacts and Notes

Prior authorized classifier experiments are under `/mnt/optane/mj-jev-experiments-20260920`; findings are in `.agents/docs/jev-bifrost2-evidence-experiment-20260920.md`. Live-session data is read-only. The user authorized sending this captured evidence to TypeSafe.

## Outcomes & Retrospective

All model-facing consumers now use the shared eight-call projection. Raw history remains unchanged. The authorized classifier replay improved confidence but did not reach the existing action threshold. Actual sub-turn grouping remains tracked in https://github.com/BrokkAi/mjolnir/issues/1110. Deploy the compatible v2 proxy before releasing workers that use its new endpoint; no production deployment was performed.

## Implementation notes (2026-09-20)

The shared view now covers compaction (including fallback and retained context), reviewer trajectories, live/checkpoint SessionWiki indexing, and Jev. The v2 proxy route keeps the v1 request contract and bundled v1 questions intact. The shared renderer removes optional older history before shortening the retained eight calls and latest user/assistant messages. Names and outcomes remain outside raw-body excerpts.

The worker's sealed journal segments deliberately remain cold at startup: an existing regression test verifies reopening even when a historical compressed segment is corrupt. Reconstruct the process-local summary from the bounded hot event window already loaded by startup, and mark unavailable earlier history explicitly; do not add filesystem scans or decompression to opening the worker. Fresh live collection and available hot replay use the same durable-observation entry point. Clear discards classifier text as well as tool history.

Initial validation: 92 transcript tests passed; 1518 controller tests passed with 7 ignored. Worker tests found payload-logging assertions needing revision and caught the sealed-history startup regression, now addressed. Final whole-suite and clippy results are pending.

The authorized replay used canonical archive frontier 116381 (6139 transcript entries), with no tool changes or item creations after the original Running evidence capture. With eight full calls, request size is about 53 KB versus 3 KB for the old payload. Replied Finished confidence rose from 0.47–0.51 to 0.65–0.68; Running rose from 0.38–0.44 to 0.53–0.68. User-question and pending-work controls were classified correctly at 1.0. This does not reach the unchanged 0.85 action threshold; retaining raw recent calls preserves substantial noise.

Final validation: `cargo test --no-fail-fast --quiet` passed every target except one transient worker logging assertion (the cancellation event was present but the request log was missing). That test passed alone; rerunning the complete worker library passed all 530 tests with 9 ignored. All workspace targets have therefore passed. `cargo clippy --all-targets -- -D warnings`, rustfmt, and diff checks passed. Proxy tests passed all 15 cases, TypeScript checking passed, and the deployment dry run passed. Logs are in `target/transcript-summary-tests.log`, `target/transcript-summary-worker-tests.log`, and `target/transcript-summary-clippy.log`. No live configuration or production deployment changed.
