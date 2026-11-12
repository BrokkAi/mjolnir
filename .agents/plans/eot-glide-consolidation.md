# Consolidate completed-turn assessment on GLiDE

This living ExecPlan follows `.agents/PLANS.md`. The end-of-turn assessment is the semantic decision taken after one physical harness turn completes. The worker owns its durable result; the proxy owns provider questions and credentials.

## Purpose / Big Picture

Replace Jev's completed-turn classifier with the frozen Fastino GLiDE evaluation policy while retaining existing lifecycle admission, question location and recovery behavior. Continue must abstain until live outcomes justify enabling it. A real completed turn should send evidence only to a dedicated proxy route, produce a durable assessment, and log probabilities and request.

## Progress

- [x] (2026-11-12) Fetch and fast-forward master from 0fcbd172f to 90b7da547; read repository instructions and frozen specification.
- [x] (2026-11-12) Copy exact frozen questions and port the probability rule into `mj-core/src/activity/eot_glide.rs`.
- [x] (2026-11-12) Extract 18 real fresh-test probability vectors with research-derived expectations, excluding labels and transcript text.
- [x] Integrate typed categorical judgment with durable TurnAssessment and define relay compatibility changes.
- [x] (2026-11-12) Implement dedicated proxy route, admission and normalized-distribution validation; proxy check and all tests pass.
- [x] (2026-11-12) Implement separate worker projection and typed hosted transport under an 80 KiB budget; production remains disconnected.
- [x] Connect production completed-turn requests, durable categorical judgments and compatibility.
- [x] (2026-11-12) Audit daemon continuation: unified/current workers consume durable assessment; leave legacy Jev route unchanged.
- [x] (2026-11-12) Capture actual relay/worker projection as proxy fixture and verify worker body → proxy → Fastino shape.
- [x] Complete targeted and final validation (full-suite obsolete fixtures corrected and rerun selectively).

## Surprises & Discoveries

The current daemon legacy classifier uses `/v2/continuation-verdict` rather than `/v7/turn-verdict` (`mj-controller/src/continuation.rs`). It is still relevant to this switch. The worker's Direct source is also used for unrelated question location and AFK classification; changing its constructor globally would change unrelated classifiers.

The initial focused Cargo command used the directory name as package name and was corrected to `brokk-mj-core`. No build-directory or mbx settings were changed.

## Decision Log

2026-11-12: Keep the frozen questions verbatim, including unused permission_veto. Use probabilities rather than confidence. `ENABLE_AUTOMATIC_CONTINUE = false` is the single future enablement switch; when a Continue candidate wins, abstain before considering lower-priority Finished or Wait. This preserves the frozen rule's precedence.

2026-11-12: No persisted format change has yet been made. The integration must add a typed categorical judgment to Verdict with backward-readable defaults, update literal constructors, and assess relay journal/snapshot compatibility before enabling production use. No database migration is currently proposed. The implementation must never encode categorical scores inside unrelated existing fields.

## Outcomes & Retrospective

Preparatory rule milestone exists; the production classifier has not switched. Remaining milestones above are required before accepting this feature.

## Context and Orientation

`mj-core/src/assessment.rs` holds durable Verdict and Action. `mj-core/src/activity/verdict.rs::decide` maps action to public activity decisions. `mj-worker/src/relay/verdict.rs` applies assessments and owns stale-generation and lifecycle gates. `mj-worker/src/acp/verdict_client.rs` owns transport and decision logging; the hosted endpoint is currently v7 Jev. `services/jev-proxy/src/index.ts` admits requests through a per-IP limiter and bounded parser. `services/jev-proxy/src/glide.ts` already sends model fastino/GLiDE and store=false to Fastino with X-API-Key. Preserve the handback route.

## Plan of Work

First finish core rule verification. Then add explicit validated categorical probabilities to the durable judgment and route all new completed-turn action decisions through eot_glide::action. Preserve previously persisted authoritative actions and old-worker compatibility. Add a dedicated hosted completed-turn route; keep provider questions and model entirely proxy-owned. Reuse glide.ts and FASTINO_API_KEY, preserve provider errors and Retry-After. The evaluation measured requests near 79 KB, exceeding the existing 64 KiB bound: increase only this route's parser allowance using that measurement, and verify worker-side budgeting and actual largest captured fixture. Do not raise unrelated route limits.

Project richer evidence from worker facts: latest user message and following assistant replies from authorization.messages; phase, silence, tools, background commands, queued prompts and final-tool outcomes as plain observations. Never infer missing child liveness or future wake sources. Preserve richer recent message text before existing summary trimming. Keep `[jev] enabled=false` disabling classification and exclude child sessions.

## Concrete Steps

Work from the repository root. Run `cargo test -p brokk-mj-core frozen_fresh_test_probabilities_match_research_with_continue_abstention` outside sandbox. Inspect worker request construction, typed assessment parsing and serialization before integrating. Add proxy contract tests in the existing handback-test style, using a fixture emitted through the actual worker request function. Repeat focused tests only while iterating.

## Validation and Acceptance

The frozen regression must pass all real sampled probability vectors, including Continue abstention. Worker/proxy integration must demonstrate evidence-only requests and bundled GLiDE questions, provider error passthrough and admission. At the end run `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, full touched-crate suites outside sandbox on dev profile, and proxy `npm run check` and `npm test`. All daemon and worker end-to-end invocations must use a named isolated instance. Known baseline controller cleanup failure and flaky native mbx probe must be reported separately with logs.

## Idempotence and Recovery

No deployment, installation, commit or push is authorized for this task. Changes remain in the working tree for parent review. No live daemon or store should be used. The source bundle is `/mnt/optane/tmp/eot-glide-port-20261112/FROZEN`; the checked-in sample includes only probabilities and expected decisions.

## Artifacts and Notes

Focused test output is `.mj/agents/79d7b1f391b29f883eefb030b02582e6/core-rule-test.log`. Rule input is the answers object (each axis containing probabilities), plus the already validated authorization-complete boolean. Background judgment independently uses the presence of listed processes and the .90 probability floor.

## Interfaces and Dependencies

`mj_core::activity::eot_glide::action(&serde_json::Value, bool) -> assessment::Action` is the sole new probability-to-action rule. Its caller must validate complete finite normalized distributions. `background_needed(&Value, bool) -> Option<bool>` retains independent background judgment. These functions currently have no production callers.

Revision 2026-11-12: Recorded the completed preparatory milestone and explicitly marked production integration and validation incomplete.


Revision 2026-11-12 (second child checkpoint): Added `/v1/glide-turn-verdict` to `services/jev-proxy/src/index.ts`. Evidence is the existing v6 body plus required `latest_user_message` string, `current_assistant_replies` string array, and `runtime_observations` string array; phase must be replied. Response is `{answers}` with all seven frozen axes, complete finite normalized probabilities validated with the existing 0.03 normalization tolerance. Questions are bundled from the exact frozen core JSON. Fastino errors and Retry-After pass through unchanged, with no retries. This route uses FASTINO_API_KEY, shared TURN_RATE_LIMITER per IP and an 80 KiB request bound derived from the measured ~79 KB; all older routes retain 64 KiB. Response parsing retains 64 KiB. No worker code or persisted contracts changed in this checkpoint.

Legacy-path decision evidence: `mj-controller/src/daemon/continuation.rs::unified` checks relay protocol against `assessment::PROTOCOL`; the job at lines 812 onward immediately returns a derived ContinuationVerdict for unified snapshots. It only calls the legacy classifier below that branch for older workers. Keep `/v2/continuation-verdict` on Jev.

Validation: proxy `npm run check` and `npm test` passed, logs `.mj/agents/4582287d263c2c961839701b98032bf2/proxy-check.log` and `proxy-test.log`. New `tests/eot-glide.test.ts` covers frozen request construction, missing distributions, passthrough, rate limiting, >64 KiB acceptance and >80 KiB rejection. Its input reuses a handback capture with synthetic projection fields: it is explicitly NOT the required actual completed-turn worker capture. Remaining: typed stored categorical judgment and all compatibility decisions; worker transport isolated from Direct locator/AFK; evaluated richer evidence and budget; actual worker capture; touched Rust checks and final validation. No protocol/schema/journal revision has been changed. No commit, push, deploy or install occurred. Proxy must deploy before a worker using the new route is installed, once the remaining implementation is complete.

Revision 2026-11-12 (third child, worker transport slice): Added `VerdictClient::ask_glide_turn(&self, &TurnEvidence, ContextHistory, &ActivityFacts, usize) -> Result<glide_turn::Probabilities>`. It always uses the hosted GLiDE route, including when unrelated classifiers use Direct; a Hosted endpoint retains its origin for isolated transport tests. It propagates HTTP, decoding and distribution errors. Explicit typed distributions cover input, failure, work, background, turn_state and permission_veto (six axes; the earlier note saying seven was a counting error). Complete finite [0,1] distributions and proxy-equivalent .03 normalization tolerance are required; categorical scores are not stored in unrelated fields.

The separate `glide_turn::project` accepts untrimmed worker authorization history, completed-turn evidence, ActivityFacts and relay native-agent count. It adds latest_user_message, current_assistant_replies, and plain runtime observations of phase/silence, tools, command/queued counts, final tools/background inventory and native agents/goal state. All strings pass the shared credential redactor. Only this request has an 80 KiB serialized body allowance; existing classifiers and their budgets are unchanged. It removes historical context before the latest user (marking omitted authorization or assistant history), preserving the current user and following replies; oversized current context returns an error rather than silently truncating it. No future wake or child liveness is inferred.

Next slice must call this method using the FULL snapshot authorization context before `bound_authorization` applies the older Jev budget, and use facts/native count captured with that immutable generation. It must implement durable categorical judgment, preserve stored authoritative actions, decide relay journal/snapshot backward compatibility and required revision bump, then integrate production actions/logging and independent background judgment. No production call, migration, protocol change, install, deploy, commit or push was made here.

Fixture provenance: `relay::verdict::tests::glide_worker_projection_capture_and_large_history_bound` opens a temporary isolated DurableRelay, submits and claims a real user prompt, records an ACP assistant chunk and finishes the prompt through existing relay test support. It projects actual worker history and ActivityFacts. `MJ_UPDATE_GLIDE_FIXTURE=1 cargo test -p brokk-mj-worker --lib -- glide_` wrote `services/jev-proxy/tests/fixtures/eot-glide-worker.json`; ordinary replay compares the complete JSON value. The proxy EOT test now imports that capture, removing the synthetic handback-derived projection. Large-history coverage feeds roughly 900 KB through the same projection, verifies output is above 64 KiB and at most 80 KiB, preserves current user/runtime facts, and checks no questions are sent. A second test parses all 18 real probability vectors and rejects incomplete or out-of-range distributions.

Third-slice validation: `cargo test -p brokk-mj-core frozen_fresh_test_probabilities_match_research_with_continue_abstention` passed (1); `cargo test -p brokk-mj-worker --lib -- glide_` passed (2); proxy `npm run check` and `npm test` passed (47). Required clippy and fmt were also run; final statuses are recorded in the child report. Logs: `.mj/agents/e31b000ffad4ae34272d3a87a7d69dd5/{core-test,worker-test,proxy-check,proxy-test,clippy,fmt}.log`. Mechanical compile fix: import EvidenceMessage from its public continuation module rather than assessment's private import. No unresolved test failure is known.

Revision 2026-11-12 (production integration): dispatch now calls ask_glide_logged/ask_glide_turn. Authorization is retained before the old Jev shrink; dispatch captures ActivityFacts/native count under the same relay lock as pending request admission. apply_glide_assessment uses the frozen action/background rule; existing lifecycle admission, failures/backoff and locator remain. Locator accepts GLiDE required-input probabilities. Continue remains held off solely in eot_glide::ENABLE_AUTOMATIC_CONTINUE=false.

Typed distributions moved from worker-private types into activity::eot_glide::types; TurnAssessment.glide is optional, default absent and omitted when absent. Old snapshots deserialize without it, new snapshots retain stored action, and old JSON readers ignore the additive field. No DB migration or journal/snapshot format epoch is required (same JSON event and snapshot envelope). Relay protocol advances 61→62 to advertise the new semantic producer; minimum supported relay version remains unchanged so upgraded daemons can consume old workers, including their Jev assessments. Older daemons do not understand GLiDE distributions and can lose the additive field if rewriting snapshots; install upgraded daemon before workers. Daemon continuation's completed-assessment detection accepts glide as well as verdict; other view/question paths consume stored action. Existing legacy Jev worker request helpers now compile only for tests of the old transport; unrelated production classifiers remain unchanged. Durable GLiDE background judgment restores after worker restart rather than reclassifying a completed action.

Validation checkpoint: targeted GLiDE core (2) and worker (3) tests PASS, including local fake HTTP /v1/glide-turn-verdict → typed assessment → journal reopen for all 18 vectors, Continue abstention, and absent-field old JSON. Proxy npm check/test PASS. Workspace clippy passed before the final atomic-capture/daemon-reader edits; final clippy and full touched-crate suites are in progress. Evidence directory .mj/agents/52d63f5ea29fec596bf2f5591a65f74b (targeted.log, targeted-final.log, clippy-complete.log, full-tests.log, proxy-check.log, proxy-test.log, fmt.log). Deploy order: proxy first, upgraded daemon second, workers last. No commit/push/deploy/install performed.

Final targeted rerun PASS (2 core, 3 worker); final workspace clippy PASS (clippy-complete.log, 24.27s); fmt check PASS; diff whitespace check PASS. Full crate suite still compiling as of this checkpoint. Mechanical fixes: removed unused Context import after moving distribution parser to core; restricted obsolete worker Jev transport/apply helpers to cfg(test), preserving existing transport regressions without a production Jev completed-turn path.

Full validation results: controller 2,566 passed / 19 ignored, core 636 passed / 3 ignored (and all core binary tests passed). Worker full run: 818 passed / 11 ignored / 6 failures, all obsolete Jev fixture expectations: stored_evidence_is_refit_to_the_request_limit_before_sending, and dispatch verdict_tests::{a_classifier_answer_waits_for_a_ready_checkpoint_barrier, probability_qualified_completion_and_input_reach_activity_at_low_confidence, replied_verdict_publishes_without_delaying_completion_and_clears_on_output, shutdown_cancels_pending_replied_verdict, uncertain_verdicts_are_cached_and_transport_failures_back_off}. Updated the existing budget regression to preserve full history then bound via GLiDE. Updated dispatch tests to hosted evidence shape/root fields, all six complete probability axes and probability-based logging. Their final focused rerun (dispatch-corrected.log) PASSES all seven affected tests. No remaining known test failures. The full suite is not repeated to confirm those fixture fixes, per repository targeted-rerun rule. Final lint/format after these fixes: clippy-last.log / fmt-last.log (in progress at this checkpoint). Added GLiDE requested/received/failed tracing with probabilities, preserving cancellation diagnostics and redaction checks. No deployment occurred.

Final status: clippy-last.log PASS (19.38s), fmt-last.log PASS, doctests.log PASS, git diff --check PASS. All validation complete; no unresolved failures. No commits, pushes, deployment or installation.
