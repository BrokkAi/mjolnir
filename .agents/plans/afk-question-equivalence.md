# Classify repeated AFK questions with Jev

This ExecPlan follows `.agents/PLANS.md` and must be maintained as work proceeds.

## Purpose / Big Picture

An absent operator should receive a notice for each new decision an agent asks about. A repeated request can still wait for the operator. Currently every question from one agent shares a single identity, so unrelated questions incorrectly wait without an AFK notice. Ask Jev whether the whole new form effectively repeats previously AFK-answered questions. Preserve the existing broad identity behavior whenever classification is unavailable. The user explicitly authorized deploying the updated hosted Jev service.

## Progress

- [x] (2026-11-10) Traced the live failure: session 5992a212 received an AFK answer at 14:57 UTC, then a mixed new/repeated form at 15:09 UTC remained pending. Claimed issue #1489.
- [x] (2026-11-10) Implemented the shared classifier contract, hosted route, worker transport, and presence-owned acceptance.
- [x] (2026-11-10) Four isolated protocol/concurrency regressions pass, including direct/hosted envelopes and unavailable/malformed responses.
- [x] (2026-11-10) Worker suite: 816 passed, 11 ignored. Workspace dev Clippy, rustfmt, service tests, TypeScript check, and deploy dry run pass.
- [x] (2026-11-10) Deployed Jev version 9774197c-a7da-4f0f-9d8f-e45a08dbe744. Five synthetic live cases pass. With explicit user approval, three live checks using the actual retained forms pass: mixed=new, identical=same, earlier subset=same.
- [x] (2026-11-10) Committed the initial validated implementation as 4d8034f7a; first review completed.
- [x] (2026-11-10) Addressed form-constraint evidence finding; four focused AFK regressions, workspace Clippy, service tests (39), type check, dry run, and final live smoke pass.
- [x] (2026-11-10) Final library run: 790 passed, 11 ignored, one unrelated work_facts failure; that test passed on its focused rerun.
- [x] (2026-11-10) Remaining binary/integration suites pass (26 tests). Final implementation is validated and ready for commit and required before-push review; remote delivery follows that review.

- [x] (2026-11-10) Second-review fix validated: five focused AFK regressions, full core/worker suites, workspace Clippy, and formatting pass. The worker suite now has 818 passing tests with 11 ignored. Ready for final commit and push review.

- [x] (2026-11-10) Third-review fix: redact normalized form values before serialization; the five focused AFK regressions, escaped-format live checks, Clippy, and formatting pass.
- [ ] Final combined core/worker suite is rebuilding; commit the focused validated fix and run review in parallel, then push only after both finish.

## Surprises & Discoveries

The refusal owner lives in `mj-worker/src/acp/operator_away.rs` and survives bridge restarts. Its question identity is only `(agent, Question)`. Permission identities already include tool content and need no change. Requests opened before AFK use the same owner as newly opened requests.

## Decision Log

- Decision: compare the entire form with all questions actually AFK-answered for the same agent during this presence revision. A batch containing any new decision is different. Keep plan/tool identities unchanged.
  Rationale: exact strings miss paraphrases; comparing only the latest form misses older repeats. User requested Jev and existing-behavior fallback.
  Date/Author: 2026-11-10 / Codex.
- Decision: let existing responder tasks own asynchronous classification and cancellation; keep all answer acceptance under the existing presence mutex. Revalidate presence revision and compared history before applying an answer.
  Rationale: no network I/O on ACP ingress or worker control loops, no detached classifier tasks, and stale results cannot answer a request after the operator returns or answers it.
  Date/Author: 2026-11-10 / Codex.

- Decision: keep the documented 64 KiB evidence-limit failure on the explicitly requested unavailable-classifier path, with a warning and the existing pending behavior.
  Rationale: truncation or eviction loses evidence for old repeats; independent batches lose whole-form coverage across earlier questions. A new summarization/classification protocol is outside this scoped change. Recorded the review finding as declined.
  Date/Author: 2026-11-10 / Codex.
- Decision: send serialized normalized elicitation forms, excluding transient request IDs, rather than mailbox-rendered text.
  Rationale: review found that requiredness, selection cardinality, and numeric/text constraints disappear from question_text(). Jev needs those constraints to recognize material changes.
  Date/Author: 2026-11-10 / Codex.

- Decision: preserve redacted identifier equality within each AFK comparison with request-local anonymous labels.
  Rationale: the shared egress redactor otherwise replaces distinct commit/session identifiers with the same text. Reuse its token scanner, and keep ordinary diagnostics unchanged; only AFK evidence uses equality-preserving labels.
  Date/Author: 2026-11-10 / Codex.

## Outcomes & Retrospective

Implementation and the hosted deployment are complete. The worker uses one accepted-answer map for both the old fallback identity and classifier evidence. The existing responder task owns HTTP inference; presence/history changes cancel stale inference and all acceptance stays under the same mutex. No migration, relay version, dependency, or install is needed for the service deployment. Worker integration takes effect with a Mjolnir build containing this change; this task does not authorize installing Mjolnir.

## Context and Orientation

The worker runs one agent harness and owns pending structured requests. `mj-worker/src/acp/drive.rs` registers them and spawns responder tasks. `permissions.rs::await_elicitation_response` arbitrates human answers, AFK answers, and cancellation. `operator_away.rs` owns presence and refusal memory. `verdict_client.rs` already sends Jev HTTP requests with a ten-second transport deadline covering the hosted service's eight-second upstream deadline. `services/jev-proxy/src/index.ts` routes validated evidence to TypeSafe and validates its answer. The proxy has a documented 64 KiB body ceiling. Existing tests in `acp/plan_tests.rs` drive real ACP envelopes using isolated temporary roots.

## Plan of Work

Add a shared frozen question specification and a hosted `/v1/afk-question-verdict` route. Its evidence contains the current question and previous AFK-answered questions. Return the validated same/different choice. Extend the existing worker Jev client rather than creating another transport. Keep prior question text beside refusal memory and publish state changes to waiting responder tasks. Classify only when the old generic identity would leave a question pending; no call is needed for the first question. Apply a different verdict only while the compared presence and history remain current. Disabled Jev, network errors, invalid responses, and tied probabilities preserve the current behavior.

## Concrete Steps

Work from the repository root. Install service dependencies using `npm ci` in `services/jev-proxy`, then run `npm test`, `npm run check`, and `npm run deploy:dry-run`. Run focused Rust tests outside the sandbox using `cargo test -p brokk-mj-worker <filter>`, maintaining package selection. After the final edit run the touched crate's full dev suite and workspace `cargo clippy --all-targets -- -D warnings`. Use existing isolated temporary test data; any executable daemon invocation must use `--instance afk-question-1489`. Do not redirect Cargo build output or change mbx configuration. Commit only changed files on master, request review before pushing, and merge origin/master if necessary. Deploy using `npm run deploy` after validation and smoke-test both repeated and different questions against the hosted route.

## Validation and Acceptance

A real ACP request first gets an AFK answer. A paraphrase remains pending; an unrelated question or a batch adding a decision gets another AFK answer. HTTP failure preserves the repeat behavior. Human response, cancellation, bridge teardown, and AFK-off during classification prevent stale application. Concurrent questions must be compared against the latest accepted refusal history so two equivalent requests cannot both be treated as new. Service tests exercise actual route requests and upstream envelopes. Live smoke tests prove the deployed contract handles the original #10941/#11482 case.

## Idempotence and Recovery

No schema or relay changes are needed. Presence and refusal history stay process-local as before. Old workers ignore the new route and preserve their existing behavior. Deployment is additive and can be repeated; a failed service call leaves the form open. Do not install Mjolnir without user authorization.

## Artifacts and Notes

Issue: https://github.com/BrokkAi/mjolnir/issues/1489. Hosted version: 9774197c-a7da-4f0f-9d8f-e45a08dbe744 at https://mj-jev-proxy.eng-admin-a63.workers.dev. Final actual-form live results: mixed form different=0.99; identical form same=0.97. Earlier subset same=0.76 on the first deployment. Final constraint live results: single/multi change different=1.0; requiredness-only change different=0.99; field ID and option order change same=0.99. The five synthetic cases also classify as expected. Python urllib received Cloudflare 1010; normal curl requests succeeded.

## Interfaces and Dependencies

Use the existing reqwest/rustls client in `acp/verdict_client.rs` and existing Jev enablement/source configuration. Add no dependency or crate. The worker's presence owner remains the only decision point for consuming pending answer senders. Changes to its state wake existing responder tasks through a watch channel; the responder's select loop drops network work when cancellation or a human answer wins.

Initial plan written 2026-11-10 after tracing the live request and existing client contracts.

Updated 2026-11-10 after implementation and validation. Unified refusal evidence into one map so the fallback identity cannot diverge from the history used by Jev. The first smoke test containing internal text was blocked by automatic approval review; synthetic tests succeeded, then the user explicitly approved the actual-form smoke test and all three cases passed.

Updated 2026-11-10 after first review: preserve normalized form constraints in Jev evidence; keep unsupported evidence sizes on the user-requested fallback path instead of introducing lossy history handling.

Updated 2026-11-10 after final service deployment 9774197c-a7da-4f0f-9d8f-e45a08dbe744: live constraints and actual normalized forms pass; the final touched-crate suite completed as recorded above; required push review remains.

Final validation note: work_facts::tests::a_subject_read_for_a_different_commit_is_unavailable failed once with "the tree was unavailable" in the full parallel run, then passed immediately by itself. No unrelated source changes were made; binary/integration suites continue separately because Cargo stopped after the library failure.

Implementation checkpoint: all 817 worker tests have passed across the final run and the focused retry, with 11 existing ignores. The 39 service tests, TypeScript check, deploy dry run, rustfmt, workspace Clippy, and final deployed-service smoke checks pass. The final action is review and push; no Mjolnir installation was requested.

Second-review update: normalized forms retain constraints, but shared secret redaction hid distinctions between long identifiers. Added request-local anonymous labeling using the shared scanner, plus a real HTTP regression proving distinct/equal IDs remain distinct/equal and no raw ID leaves the process. Deployed-service live anonymous-label cases pass (different=0.72; same=0.99). No service update is needed for this worker-only fix.

Final validation after the identifier fix: cargo test -p brokk-mj-core -p brokk-mj-worker passed completely, including the earlier transient work_facts case. Worker library: 792 passed, 11 ignored; binary/integration: 26 passed. Workspace Clippy and rustfmt pass. The shared redactor retains its original diagnostic behavior; only AFK comparison evidence gets consistent anonymous labels.

Third-review update: scanning serialized JSON made escape letters part of token identity and could produce invalid escaped text. The AFK client now parses normalized forms, redacts their string values using the shared JSON walker and token scanner, then serializes them. Ordinary diagnostic redaction still preserves its previous identifier-key behavior; AFK evidence anonymizes all values. The existing HTTP regression now covers newline/tab formatting and checks valid decoded evidence.

Checkpoint: the escaped-identifier fix is independently validated by the expanded real HTTP regression, live anonymous-label formatting comparisons, workspace Clippy and rustfmt. The full suite is still running and must complete before delivery. No source edits remain in flight.
