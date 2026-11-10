# Classify repeated AFK questions with Jev

This ExecPlan follows `.agents/PLANS.md` and must be maintained as work proceeds.

## Purpose / Big Picture

An absent operator should receive a notice for each new decision an agent asks about. A repeated request can still wait for the operator. Currently every question from one agent shares a single identity, so unrelated questions incorrectly wait without an AFK notice. Ask Jev whether the whole new form effectively repeats previously AFK-answered questions. Preserve the existing broad identity behavior whenever classification is unavailable. The user explicitly authorized deploying the updated hosted Jev service.

## Progress

- [x] (2026-11-10) Traced the live failure: session 5992a212 received an AFK answer at 14:57 UTC, then a mixed new/repeated form at 15:09 UTC remained pending. Claimed issue #1489.
- [x] (2026-11-10) Implemented the shared classifier contract, hosted route, worker transport, and presence-owned acceptance.
- [x] (2026-11-10) Three isolated protocol/concurrency regressions pass, including direct/hosted envelopes and unavailable/malformed responses.
- [x] (2026-11-10) Worker suite: 816 passed, 11 ignored. Workspace dev Clippy, rustfmt, service tests, TypeScript check, and deploy dry run pass.
- [x] (2026-11-10) Deployed Jev version af3e57cf-9fc9-4093-8d3f-bd39dc6695ea. Five synthetic live cases pass. With explicit user approval, three live checks using the actual retained forms pass: mixed=new, identical=same, earlier subset=same.
- [ ] Commit, obtain review, and push to origin/master.

## Surprises & Discoveries

The refusal owner lives in `mj-worker/src/acp/operator_away.rs` and survives bridge restarts. Its question identity is only `(agent, Question)`. Permission identities already include tool content and need no change. Requests opened before AFK use the same owner as newly opened requests.

## Decision Log

- Decision: compare the entire form with all questions actually AFK-answered for the same agent during this presence revision. A batch containing any new decision is different. Keep plan/tool identities unchanged.
  Rationale: exact strings miss paraphrases; comparing only the latest form misses older repeats. User requested Jev and existing-behavior fallback.
  Date/Author: 2026-11-10 / Codex.
- Decision: let existing responder tasks own asynchronous classification and cancellation; keep all answer acceptance under the existing presence mutex. Revalidate presence revision and compared history before applying an answer.
  Rationale: no network I/O on ACP ingress or worker control loops, no detached classifier tasks, and stale results cannot answer a request after the operator returns or answers it.
  Date/Author: 2026-11-10 / Codex.

## Outcomes & Retrospective

Implementation and the hosted deployment are complete. The worker uses one accepted-answer map for both the old fallback identity and classifier evidence. The existing responder task owns HTTP inference; presence/history changes cancel stale inference and all acceptance stays under the same mutex. No migration, relay version, dependency, or install is needed for the service deployment. Worker integration takes effect with a Mjolnir build containing this change; this task does not authorize installing Mjolnir.

## Context and Orientation

The worker runs one agent harness and owns pending structured requests. `mj-worker/src/acp/drive.rs` registers them and spawns responder tasks. `permissions.rs::await_elicitation_response` arbitrates human answers, AFK answers, and cancellation. `operator_away.rs` owns presence and refusal memory. `verdict_client.rs` already sends Jev HTTP requests with a ten-second transport deadline covering the hosted service's eight-second upstream deadline. `services/jev-proxy/src/index.ts` routes validated evidence to TypeSafe and validates its answer. The proxy has a documented 64 KiB body ceiling. Existing tests in `acp/plan_tests.rs` drive real ACP envelopes using isolated temporary roots.

## Plan of Work

Add a shared frozen question specification and a hosted `/v1/afk-question-verdict` route. Its evidence contains the current question and previous AFK-answered questions. Return the validated same/different choice. Extend the existing worker Jev client rather than creating another transport. Keep prior question text beside refusal memory and publish state changes to waiting responder tasks. Classify only when the old generic identity would leave a question pending; no call is needed for the first question. Apply a different verdict only while the compared presence and history remain current. Disabled Jev, network errors, invalid responses, and uncertainty preserve the current behavior.

## Concrete Steps

Work from the repository root. Install service dependencies using `npm ci` in `services/jev-proxy`, then run `npm test`, `npm run check`, and `npm run deploy:dry-run`. Run focused Rust tests outside the sandbox using `cargo test -p brokk-mj-worker <filter>`, maintaining package selection. After the final edit run the touched crate's full dev suite and workspace `cargo clippy --all-targets -- -D warnings`. Use existing isolated temporary test data; any executable daemon invocation must use `--instance afk-question-1489`. Do not redirect Cargo build output or change mbx configuration. Commit only changed files on master, request review before pushing, and merge origin/master if necessary. Deploy using `npm run deploy` after validation and smoke-test both repeated and different questions against the hosted route.

## Validation and Acceptance

A real ACP request first gets an AFK answer. A paraphrase remains pending; an unrelated question or a batch adding a decision gets another AFK answer. HTTP failure preserves the repeat behavior. Human response, cancellation, bridge teardown, and AFK-off during classification prevent stale application. Concurrent questions must be compared against the latest accepted refusal history so two equivalent requests cannot both be treated as new. Service tests exercise actual route requests and upstream envelopes. Live smoke tests prove the deployed contract handles the original #10941/#11482 case.

## Idempotence and Recovery

No schema or relay changes are needed. Presence and refusal history stay process-local as before. Old workers ignore the new route and preserve their existing behavior. Deployment is additive and can be repeated; a failed service call leaves the form open. Do not install Mjolnir without user authorization.

## Artifacts and Notes

Issue: https://github.com/BrokkAi/mjolnir/issues/1489. Hosted version: af3e57cf-9fc9-4093-8d3f-bd39dc6695ea at https://mj-jev-proxy.eng-admin-a63.workers.dev. Actual-form live results: mixed form different=1.0; identical form same=0.99; earlier subset same=0.76. The five synthetic cases also classify as expected. Python urllib received Cloudflare 1010; normal curl requests succeeded.

## Interfaces and Dependencies

Use the existing reqwest/rustls client in `acp/verdict_client.rs` and existing Jev enablement/source configuration. Add no dependency or crate. The worker's presence owner remains the only decision point for consuming pending answer senders. Changes to its state wake existing responder tasks through a watch channel; the responder's select loop drops network work when cancellation or a human answer wins.

Initial plan written 2026-11-10 after tracing the live request and existing client contracts.

Updated 2026-11-10 after implementation and validation. Unified refusal evidence into one map so the fallback identity cannot diverge from the history used by Jev. The first smoke test containing internal text was blocked by automatic approval review; synthetic tests succeeded, then the user explicitly approved the actual-form smoke test and all three cases passed.
