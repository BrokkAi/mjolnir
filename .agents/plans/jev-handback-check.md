# Check unfinished work before delivering an ordinary child's handback

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

An ordinary child that reports unfinished authorized work should receive one automatic prompt to continue through its handback tool result. A second handback for that assignment must reach its parent without another classification. Reviewers and oracles must never receive this check.

## Progress

- [x] (2026-11-11) Replaced handback v7 inference with hosted GLiDE; preserved assignment journal admission, serialization, identity and disabled behavior. Stable bundled question file retains c1 wording.
- [x] (2026-11-11) Validated GLiDE contract: 15 worker handback tests, 2 core handback tests, workspace clippy, formatting, proxy TypeScript check and all proxy cases pass. Exact request bodies are in this task report directory. Working tree remains uncommitted; no deployment.

- [x] (2026-11-11) Read AGENTS.md and PLANS.md; traced socket admission, daemon delivery, authorization filtering, and launch identity.
- [x] (2026-11-11) Parent approved optional subagent_kind using the existing enum; absent kind delivers unchecked.
- [x] (2026-11-11) Implemented explicit handback authorization context, durable per-assignment check admission, and asynchronous socket classification.
- [x] (2026-11-11) Added journal-replay/reminder/follow-up and socket concurrency/retry regressions; extended authorization and launch serialization coverage.
- [x] (2026-11-11) Ran targeted checks, all touched-crate suites, and workspace clippy. Fixed and reran the single attribution regression plus affected socket test. Changes remain uncommitted for the parent.

## Surprises & Discoveries

`mj-core/src/worker_launch.rs::WorkerLaunchConfig` carries `handback_tool` and a managed-child environment flag, but no child kind. Its `oracle` flag means permission to spawn an oracle, not that this session is an oracle. `mj-controller/src/controller/worker_binary/launch.rs` deliberately writes that flag false for all children. Consequently the worker cannot reliably exclude oracle children using these flags.

`mj-core/src/subagent.rs::SubagentHandback::answers` compares command IDs literally. Reminder IDs encode a completed turn ordinal through `reminded_turn_ordinal`; they do not directly encode the assignment command ID. Any check identity must resolve this ordinal against the relay history rather than treating a reminder as another assignment.

The socket handler `mj-worker/src/worker_runtime/subagents.rs::serve_one` runs separately from the relay coordinator, but currently publishes a request immediately through `enqueue_marked`. A check must prevent daemon visibility until its result is known, without holding either relay or queue mutex across HTTP awaits.

## Decision Log

Relay history owns the latest delivered assignment ID. Spawn/input delivery updates it; reminder and retry delivery preserves it. Existing reminder admission validates the encoded completed ordinal against turn_completion and refuses stale reminders after new input, so a reminder cannot silently become a fresh assignment.

The new journal event requires protocol 61 and snapshot revision 22, with default initialization when reading older snapshots. This is a worker-journal change, not a database migration.

The parent explicitly requires no commits or pushes for this task. Preserve that override of repository defaults.

Child kind must be supplied explicitly rather than inferred from report size, model, prompt prose, or review settings. The parent approved optional subagent_kind from SubagentRecord.kind and unchecked delivery when absent.

## Outcomes & Retrospective

The original v7 checkpoint completed validation before this GLiDE revision. Full checkpoint, controller, core, transcript, and worker suites ran; the one worker attribution failure was fixed and both affected regressions passed on rerun. Final workspace clippy, formatting, Python syntax, and diff checks passed. Exact commands and logs are in the task report directory under .mj/agents/1b3105bf7b56aab2d14b0cf96d340159/validation.md. No commits, pushes, installs, or live-instance checks were made.

## Context and Orientation

Workers run harness turns independently of the daemon, which stores session metadata. The handback tool reaches a worker socket in `mj-worker/src/subagent_mcp.rs`, then the durable queue in `mj-worker/src/worker_runtime/subagents.rs`, then `mj-controller/src/server_runtime/api.rs`. The worker's classifier is `mj-worker/src/acp/verdict_client.rs`; `worker_runtime/unix/dispatch.rs` resolves it and supplies the decision log. `mj-core/src/assessment.rs::ContextHistory` currently excludes all generated prompts from authorization, including parent assignments. `mj-core/src/continuation.rs::GeneratedPrompt` centrally names these prompt producers. Relay events and snapshots in `mj-core/src/relay/` and `mj-worker/src/relay/` preserve worker state across restart.

## Plan of Work

First establish an explicit ordinary-child identity at launch, with absent kinds delivered unchecked. Extend GeneratedPrompt interpretation and ContextHistory with an explicit handback evidence mode that admits only parent spawn and input prompts in addition to real user messages. Keep ordinary authorization unchanged. Construct TurnEvidence with the report as the final assistant message and phase replied.

Next add durable check admission keyed by assignment command ID to the relay. Resolve reminder turns through their encoded completed ordinal. Serialize reservation before HTTP work so concurrent handbacks, retries, and restarts cannot reserve the same assignment twice. Add HandbackCheckStarted to the journal and advance relay protocol to 61 and snapshot revision to 22: new workers read old snapshots using a default empty check state, while old writers refuse the newer schema rather than dropping single-attempt admission. Run the HTTP request in supervised asynchronous work, withholding queue publication until it finishes. Bounce using mj_core::assessment::handback_needs_more_work only when the GLiDE unfinished probability is at least HANDBACK_UNFINISHED_PROBABILITY (0.50). The six probabilities are unfinished, needs_parent_input, blocked, permitted_stop, finished, unclear. The earlier v7 rule and its thresholds have been removed; ordinary Verdict::action remains unchanged. Deliver on all other answers, disabled continuation, disabled Jev, or request failure. Finish the Jev decision log with the actual delivery outcome.

Finally verify the MCP handback budget covers the classifier's existing request timeout and worker reply margin; adjust only as required by those known deadlines. Leave existing handback reminders unchanged.

## Concrete Steps

From the repository root, use targeted dev-profile tests while iterating, with elevated permissions for every cargo test invocation:

    cargo test -p brokk-mj-core <affected-filter>
    cargo test -p brokk-mj-worker <affected-filter>

For this GLiDE revision the parent explicitly requests targeted Cargo tests only, then:

    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

If controller launch code changes, include mj-controller validation. Do not change Cargo output paths or install a build. Any live invocation must use a separate named --instance.

## Validation and Acceptance

Tricky-condition tests must show a bounced assignment remains checked after a relay reopen, a second handback reaches queue publication without another request, and a new parent input permits a new check. An authorization-boundary regression must show parent spawn/input text appears only in handback evidence while reminders, mailbox wakes, wait prompts and peer-message turns remain excluded. Verify reviewer and oracle exclusion through launch serialization compatibility tests, especially absent kind fields.

## Idempotence and Recovery

Persist admission before starting classification. A crash after admission consumes the single attempt; retry delivers without classification. Do not keep relay or queue locks held while awaiting the network. Preserve existing isolated test directories. Leave no background processes when handing back.

## Artifacts and Notes

Investigation evidence is the source locations recorded above. Existing untracked `scripts/__pycache__/` predates this work and must remain untouched.

## Interfaces and Dependencies

Reuse VerdictClient, TurnEvidence, and v7 questions. Keep the question set a single request-construction parameter shared by logging and transport. The separate handback_needs_more_work helper uses HANDBACK_FAILURE_CONFIDENCE and HANDBACK_WORK_PROBABILITY; both initially equal 0.90. It does not consult the input judgment. The worker requires complete authorization before applying it. The worker owns admission and classification; the daemon must not independently reinterpret whether the same assignment was checked.

Revision 2026-11-11: incorporated parent approval, runtime implementation and regressions. Same-request socket retries share a keyed asynchronous lock so a retry cannot publish while the original call awaits classification. Separate handback calls and unrelated operations remain independent. Parent requested an escape clause for instructions that permit stopping at a checkpoint; the bounce message includes it.

Revision 2026-11-11: parent requested independent handback decision thresholds and a single question-set parameter during final validation. The first workspace clippy passed and the corrected restart regression passed; validation must now cover this final adjustment.

Revision 2026-11-11: final user decision replaces the input gate with unfinished-work probability alone, still requiring a confident no-failure judgment. The bounce escape clause now explicitly invites questions for parent instructions or decisions. The real-HTTP regression supplies required input to protect this separation from autonomous continuation.

Revision 2026-11-11: completed the full touched-crate suites. One existing queue-attribution security regression exposed trust of an incoming origin field; the normal queue path again overwrites it and a private argument carries only the worker-captured pre-check origin. Only the failing and affected socket regressions are rerun after this fix, with workspace clippy repeated for the changed source. All other full-suite targets passed.

Revision 2026-11-11: validation complete. The attribution regression and socket check both passed after the fix; workspace clippy passed again with warnings denied. No unresolved failures or design questions remain.

Revision 2026-11-11 (review fixes): replied-phase handback evidence now includes a benign end_turn CompletionEvidence with no diagnostic, as required by the deployed proxy. HandbackChecks.context is optional and begins only at the first delivered parent assignment; it retains user-side instruction entries only and never mirrors assistant chunks. The report is appended only to the evidence clone. Snapshot revision stays 22 because this shape is unreleased. The replay regression now checks top-level exclusion and instruction-only retention. A temporary ignored test generates the exact hosted request and parses its live response; its source and request/response evidence are saved in the task report directory, and the test will be removed before handback. Only affected core/worker tests and clippy are run for this review round.

Revision 2026-11-11 (review validation complete): affected core authorization/snapshot tests passed (10), affected worker handback/snapshot tests passed (36), and clippy on brokk-mj-core and brokk-mj-worker passed with warnings denied. The exact generated hosted request returned HTTP 200 with User-Agent suppressed, and the production TurnVerdict parser accepted the saved response with a unified assessment. Evidence is in the task report directory under review-fixes.md and handback-proxy-*. The temporary ignored probe was removed. No commits or remaining failures.

## GLiDE provider contract and rollout

The new POST /v1/handback-verdict accepts the existing TurnEvidence JSON directly, with no client model, questions, or credentials. The proxy bundles mj-core/src/activity/handback_questions.json unchanged from c1 and calls https://api.fastino.ai/v1/systemone with model fastino/GLiDE, state equal to the evidence, questions equal to the bundled file, store false, and X-API-Key from FASTINO_API_KEY. The shared services/jev-proxy/src/glide.ts provider helper accepts another bundled question object for future GitHub classification reuse. No retries, rate caps, or new timeouts are added. Existing worker HTTP timeout stays unchanged; a timeout delivers unchecked.

Verified against Fastino systemone, Choice, and retry docs and the working corrected/glide/run.py client under /mnt/optane/tmp/jev-handback-labels-20261111. GLiDE accepts object state unchanged and returns answers.handback.probabilities. The proxy returns {probabilities: {unfinished, needs_parent_input, blocked, permitted_stop, finished, unclear}}. Both boundaries require six finite probabilities in [0,1] and sum within 0.03 of one, following the measured client tolerance. Provider errors including 425/429/503 and Retry-After pass through; workers report errors and deliver unchecked. Decision diagnostics retain all probabilities for successful checks, including accepted handbacks. The explicit self-check bounce wording is in subagents.rs.

Deployment is parent-owned and is not authorized here. From services/jev-proxy, provision the secret with npx wrangler secret put FASTINO_API_KEY, then npm run deploy. Deploy before installing the changed worker; old proxies return 404 and the worker delivers unchecked. No worker receives the Fastino key. Do not commit, push, install or deploy in this task, and preserve mj-core/src/redact.rs and scripts/__pycache__/.
