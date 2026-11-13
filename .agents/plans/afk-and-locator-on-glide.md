# Move the AFK repeated-question check and the question locator from Jev to GLiDE

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` (repository root) and must be maintained in accordance with that file. Tracking issue: #1533.

## Purpose / Big Picture

Mjolnir ("mj") uses hosted text classifiers to make small judgments about agent sessions. Two providers exist. Jev is the TypeSafe classifier (model `jev-latest`). GLiDE is the Fastino classifier (model `fastino/GLiDE`). The completed-turn verdict, the sub-agent handback check and the GitHub item classifier already moved to GLiDE, where they measured as well as or better than Jev. Two live classifiers still call Jev:

The AFK repeated-question check. "AFK" (away from keyboard) is a mode in which the person tells mj they are away. While it is on, the first question an agent asks is declined at once with an "operator away" answer, so the agent proceeds on its own judgment. When the same agent asks again, mj must decide whether the new question repeats one already declined (then it is held for the person, because the agent evidently needs them) or is a new decision (then it is declined too). Jev makes that same-or-different call.

The question locator. When the completed-turn verdict decides a turn needs the person's input, and the agent wrote several replies since the person's last message, the locator picks the reply that holds the open question, so the TUI and web viewer open the session scrolled to it instead of at the bottom.

After this change both run on GLiDE through the hosted proxy, each is measured on labeled cases and tuned (question wording, input sizes, decision thresholds) to be at least as accurate as Jev, the AFK check writes a decision log so future production evidence exists, and the dead Jev client code in the worker is removed. The person sees the same behavior: repeated AFK questions wait for them, new ones are declined, and needs-input sessions open at the question.

## Progress

- [ ] Milestone 1: locator evaluation corpus (existing fixtures plus labeled production cases) and a Jev baseline.
- [ ] Milestone 2: locator GLiDE variants on the tuning split; freeze one design; confirm once on the held-out split.
- [ ] Milestone 3: AFK evaluation corpus (real form pairs plus constructed variants) and a Jev baseline.
- [ ] Milestone 4: AFK GLiDE wording and threshold on tuning; freeze; confirm on held-out.
- [ ] Milestone 5: proxy routes, worker clients, AFK decision log; tests.
- [ ] Milestone 6: remove the dead Jev client code from the worker (and controller, if any remains).
- [ ] Milestone 7: full validation, proxy deployment, commit, review, push.

## Surprises & Discoveries

- Observation: GLiDE calls are always hosted, even on hosts where the worker talks to Jev directly with a local TypeSafe key, because the Fastino key exists only as the proxy's secret `FASTINO_API_KEY`.
  Evidence: `mj-worker/src/acp/verdict_client.rs` ~752-760 (`ask_glide_turn` builds the hosted URL even for `VerdictSource::Direct`).
- Observation: the AFK check has no production evidence. It writes no decision log and no matching tracing lines were found in any retained worker or daemon log; the number of real calls is unknown.
- Observation: the locator has 396 logged decisions in 116 sessions (2026-11-06 to 2026-11-13: 343 located, 28 none, 25 single-reply, 0 failed), each with the exact request sent and Jev's answer; Jev latency p50 0.18 s, p90 0.28 s; request size p50 9.9 KB, max 22 KB; 22 requests hit the 32-reply cap.

## Decision Log

- Decision: evaluate before building. Milestones 1 to 4 run against the Fastino API directly with the local key `~/.secrets/fastino_api_key`, using the same request body the proxy will send; proxy and client code follow the frozen designs.
  Rationale: the earlier GLiDE ports showed that question wording, not the model, decides quality (handback: Jev 25% precision against GLiDE categories 91%). Building routes first would freeze an untested wording.
  Date/Author: 2026-11-13, parent session.
- Decision: the locator stays a separate request on its own route, sent only for turns already classified as needing input, rather than an extra question in the completed-turn request.
  Rationale: the locator is needed on a small share of turns; an extra question on every completed turn would add cost to every turn, change the frozen completed-turn route, and require reply indexes to survive that route's shrinking. The existing staleness check at store time (`mj-worker/src/relay/verdict.rs` `store_question_target`) already prevents a late answer from applying to a newer turn.
  Date/Author: 2026-11-13, parent session.
- Decision: acceptance is non-inferiority to Jev on held-out cases. The locator ships the best tuned GLiDE design if its held-out accuracy is not more than 5 percentage points below Jev's on the same cases (report the paired difference with a 95% confidence interval). The AFK check ships the best tuned design whose held-out false-different rate (repeats wrongly auto-declined) is no higher than Jev's on the same pairs, and among those, the one with the highest different-recall. If no design meets these, stop and report to the user with the numbers before building anything.
  Rationale: the user decided to move both to GLiDE; the bar is not to make either worse. For the AFK check, the existing design treats a false "different" as the harmful error (a repeat the agent needs the person for is declined again), so the threshold is fitted to that error.
  Date/Author: 2026-11-13, parent session.
- Decision: both new routes are additive (`/v1/question-locator-glide`, `/v1/afk-question-glide`); the Jev routes `/v1/question-locator` and `/v1/afk-question-verdict` stay deployed for older installed releases.
  Rationale: the proxy is shared by every installed release; the repository keeps old routes (turn-verdict `/v1` to `/v7` are all still served).
  Date/Author: 2026-11-13, parent session.
- Decision: no client-side byte caps on the GLiDE requests and the provider's read timeout. The proxy accepts up to its existing GLiDE body bound (`GLIDE_BODY_BYTES`, 256 KiB, from Fastino's 65,536-token limit at 4 bytes per token). The locator shrinks its per-reply budget and retries only when the proxy or provider refuses the size (HTTP 413 `body_too_large`, or Fastino HTTP 400 `input_too_long`), as the completed-turn client already does. The AFK check keeps its existing rule of no truncation: an oversized request fails and the question is held for the person. Client timeout 180 seconds, from Fastino's guidance to set read timeouts above 170 seconds (same as the completed-turn client). The locator's per-reply text budget and reply count are evaluation parameters, chosen in Milestone 2, not caps.
  Rationale: repository rule against speculative caps; same choices as the completed-turn port.
  Date/Author: 2026-11-13, parent session.
- Decision: the AFK check gains a decision-log record per classification (same `mj_core::jev::DecisionLog` used by the locator).
  Rationale: there is no production evidence today; future calibration needs it. The record holds the same redacted request that is sent.
  Date/Author: 2026-11-13, parent session.
- Decision: Milestone 6 removes the worker's dead Jev client code: the v7 turn-verdict request path (`HOSTED_VERDICT_ENDPOINT`, `ask_logged`, `ask_logged_for`, `apply_turn_assessment`) and the TypeSafe direct transport (`VerdictSource::Direct`, TypeSafe key discovery), once nothing in production calls them. Stored types that old journals or assessments contain (for example legacy verdict fields read by `requires_input()`) stay decodable. The v7 scenario suite `mj-core/tests/jev_scenarios.rs` is deleted with the code it tests; the fixture files in `mj-core/tests/jev-scenarios/` stay because the locator evaluation reads them.
  Rationale: after this plan no live feature calls Jev; keeping an unreachable second classifier path invites confusion and bitrot. The automatic-continuation removal plan (`.agents/plans/remove-automatic-continuation.md`) deferred this cleanup here.
  Date/Author: 2026-11-13, parent session.

## Outcomes & Retrospective

(Fill in at completion.)

## Context and Orientation

Terms. The daemon (crate `mj-controller`) is the control plane. A worker (crate `mj-worker`) runs one session's agent. The proxy (`services/jev-proxy`, a TypeScript Cloudflare Worker deployed as `mj-jev-proxy`, public URL `https://mj-jev-proxy.eng-admin-a63.workers.dev`) holds the provider keys and forwards classifier requests. An elicitation is a structured question form an agent sends to the person (type `mj_core::elicitation::ElicitationRequest`, `mj-core/src/elicitation.rs`). The decision log (`mj_core::jev::DecisionLog`, `mj-core/src/jev.rs`) is a per-worker JSON-lines file under `<data dir>/workers/<id>/jev-decisions/decisions.N.jsonl`; each start, update and finish of a decision appends a line with the same `id`, so readers deduplicate by `id` and keep the last line. `[jev] enabled` in the config file is the Privacy switch; when it is off the worker builds no classifier client and neither feature runs (env `MJ_JEV_DISABLED`).

GLiDE request shape. POST `https://api.fastino.ai/v1/systemone` with header `X-API-Key` and body `{"model":"fastino/GLiDE","state":<object>,"questions":<object>,"store":false}`. Each question is `{"type":"choice","instructions":"...","criteria":{"<label>":"<plain description>"}}` (criteria values are plain strings; Jev used `{"what": ...}` objects) or `{"type":"noul",...}` for yes/no. The answer for a choice question has `probabilities` over every label summing to 1 within 0.03; callers use only `probabilities`. The proxy's helper is `glide()` in `services/jev-proxy/src/glide.ts`. A new GLiDE route must be added in `services/jev-proxy/src/index.ts` to the key selection (~456, so it uses `FASTINO_API_KEY`), the body bound (~468, `GLIDE_BODY_BYTES`) and the route table, and must use its own handler because `classify()` chooses Jev questions by state shape.

The AFK check today. `PendingElicitations::refine_question` (`mj-worker/src/acp/operator_away.rs` ~187-286) calls `VerdictClient::same_afk_question(current, previous)` (`mj-worker/src/acp/verdict_client.rs` ~188-300). Evidence per form comes from `question_evidence` (`operator_away.rs` ~46-67): the serialized form without `id`; secret fields lose `default` and gain `"secret": true`. Before sending, secret option strings are replaced by request-local labels (`anonymize_json_strings`) and all strings pass the token redactor with distinct labels for distinct identifiers (`redact_json_strings_distinct`), so equality of values survives redaction. State is `{"current_question": form, "previous_questions": [form, ...]}`. The question is `mj-worker/src/acp/afk_question.json` (one choice `same_question` with labels `same` and `different`). The rule is "same unless different has strictly higher probability". Outcomes: different means decline now ("operator away"); same, any error, or Jev off means hold the question for the person. A state change (person returns, answers, history changes) cancels the in-flight request and reclassifies. Tests in `operator_away.rs` (~432-881) use a fake HTTP classifier and assert the privacy transforms and cancellation.

The locator today. After a completed turn is assessed as needing input (`mj-worker/src/worker_runtime/unix/dispatch.rs` ~358-385), `relay.question_locator_request` (`mj-worker/src/relay/verdict.rs` ~347-367) builds a request with `mj_core::activity::question_locator::build` (`mj-core/src/activity/question_locator.rs` ~62-98): the last 4,096 bytes of the person's latest message and up to 32 newest assistant replies since it, each clipped to a 3,000-byte budget (head two thirds, tail one third), shrunk by 0.8 until the state is at most 60 KiB. With one reply it stores that reply without asking. With two or more it calls `locate_question` (`verdict_client.rs` ~325-400), whose question is one choice `request_reply` with labels `r0..r(n-1)` and `none` (`question_locator.rs` ~104-114, instructions in `mj-core/src/activity/question_locator.json`). The rule is argmax; `none` stores nothing. The answer is stored with `store_question_target` only if the assessment is still current, as `TurnAssessment.question_message` (a stable transcript id such as `agent:<id>`); the TUI and web viewer scroll to it. Every call writes a decision record with `checked = "Which reply contains the open question?"`, `technical.request` (the exact body) and the answer.

Evaluation assets. `mj-core/tests/jev-scenarios/*.json` has 122 fixtures with `outcome.request_message_ids` (acceptable reply ids; 115 have at least two replies after the last user message), with full messages at `evidence.authorization.messages`. `scripts/jev-question-locator-eval.py` rebuilds the locator state from fixtures and asks TypeSafe directly; it is the template for the GLiDE script. Recorded Jev results: 81 of 90 on the 30 W-series fixtures times three repeats; 17 of 18 held-out positives. There is no AFK corpus.

## Plan of Work

All evaluation output goes to `/mnt/optane/tmp/glide-afk-locator-20261113/` (local, private: it contains transcript text). Production text stays on this host except for the classifier calls, which send the same class of data production already sends to Fastino. Labelers are agents that read cases blind (they never see any classifier's answer or another labeler's label) and write one label per case to a file as they go, so work survives interruptions.

Milestone 1 (locator corpus). Write `scripts/glide-question-locator-corpus.py` that reads every worker decision log in the default instance's data directory read-only, keeps records whose `checked` is the locator question, deduplicates by `id`, drops single-reply records, and writes one case per decision with the logged state, Jev's logged answer, session id and time. Combine with the 115 usable fixtures (state rebuilt with the current Rust rules, labels from `outcome.request_message_ids`). Label the production cases: two independent blind labelers each choose the reply that holds the open request the person must answer (or `none`), following the instructions in `question_locator.json`; a third adjudicates disagreements. Split production cases by session into tuning (half) and held-out (half) with a fixed seed, and write the split file before any GLiDE call. Jev baseline: production cases use Jev's logged answer; fixtures are re-run through Jev with the existing script (three repeats, majority).

Milestone 2 (locator design). Write `scripts/glide-question-locator-eval.py` (stdlib only, modelled on the Jev script) that sends variants to Fastino directly and scores accuracy (correct when the chosen reply is in the acceptable set, or `none` when the label is `none`), positives-only accuracy, and wrong-reply rate. Variants on the tuning split: V0, the direct port (same instructions, criteria `rK` as plain strings); V1, criteria rewritten for GLiDE in the categorical style that worked for the handback check; V2, one yes/no (`noul`) question per reply ("does this reply contain a request the person has not yet answered"), choosing the newest reply above a threshold; V3, the best of V0 to V2 with a larger per-reply budget (12,000 bytes) and up to 64 replies, measured on fixtures (they have full text; production logs hold only clipped text). For choice designs also fit a `none` margin (choose `none` unless the best reply's probability exceeds it), which may beat plain argmax. Run each variant twice to measure self-agreement. Freeze one design (question file, budgets, rule) in a `FROZEN/` directory, then run it once on the held-out split and compare with Jev on the same cases.

Milestone 3 (AFK corpus). Find real elicitation forms in the default instance read-only (start from the daemon store's transcript items for elicitation and question requests; if direct reading of the 16 GB store stalls, copy only the needed tables as the repository runbook describes), and form pairs from the same session and agent in time order. Label them same or different with two blind labelers and adjudication, using the rule in `afk_question.json` (decisions, scope and constraints matter, wording does not; any new decision makes it different). Because real pairs are likely few and mostly different, also construct pairs from real forms: paraphrase, reordered or subset of the same decisions (same); added decision, changed option set or constraint, unrelated question (different). Constructed pairs carry their construction label, checked by one labeler. Include the privacy shapes the tests cover (secret fields, anonymized options, redacted identifiers). Target about 150 pairs with at least 50 of each label; split tuning and held-out by source form. Jev baseline: run the current Jev question over all pairs.

Milestone 4 (AFK design). Variants on tuning: the direct port (same instructions, plain-string criteria), and a rewritten wording; for each, the threshold on p(different) at which the tuning false-different rate is no higher than Jev's, maximizing different-recall. Freeze, then held-out once.

Milestone 5 (build). Proxy: add `services/jev-proxy/src/question-locator-glide.ts` and `afk-question-glide.ts`, each validating the same state shape as its Jev route (with the frozen size and count rules), building the frozen questions from a shared JSON file (`mj-core/src/activity/question_locator_glide.json`, `mj-worker/src/acp/afk_question_glide.json`), calling `glide()`, validating the answer, and returning `{"probabilities": {...}}`; register both routes for the Fastino key and the GLiDE body bound; add tests beside `handback.test.ts` (validation errors, upstream error passthrough with Retry-After, a large body near the bound). Worker: `locate_question` and `same_afk_question` call the new routes on the hosted URL (always hosted, like `ask_glide_turn`), with the 180-second timeout, parse probabilities, and apply the frozen rules (`mj_core::activity::question_locator::parse` changes accordingly). The locator rebuilds its state with a smaller budget and retries on an explicit size refusal, reusing the refusal check `is_glide_size_refusal` in `verdict_client.rs`. The AFK client writes a decision record (kind `activity`, checked "Does this question repeat one already declined while away?", the exact redacted request, the probabilities, and the outcome). Keep every privacy transform and the cancellation behavior. Update the existing tests that pin the request path and body (`operator_away.rs` fake classifier, `locator_direct_transport_sends_redacted_state_and_shared_question`), and add one end-to-end check per route against the deployed proxy only after deployment (Milestone 7). Update `.agents/docs/jev-proxy.md` with the two routes and a GLiDE section.

Milestone 6 (retire the Jev client). Confirm with a search that no production path calls the TypeSafe endpoint or the v7 turn route, then remove that code as listed in the Decision Log, keeping every type that old stored data decodes into. Delete `mj-core/tests/jev_scenarios.rs` and its gate file if nothing else uses it; keep the fixture JSON. Update `.agents/docs/jev.md` to say that every live classifier runs on GLiDE.

Milestone 7 (ship). Run the full validation below, deploy the proxy (`npm run deploy` in `services/jev-proxy`, after `npm run deploy:dry-run`), run one live request per new route with a synthetic case, then commit and push. Do not install a new mj build without the user's instruction.

## Concrete Steps

Evaluation scripts run from the repository root with `python3 -I scripts/<name>.py --out /mnt/optane/tmp/glide-afk-locator-20261113/<step>`. Rust tests: `cargo test -p mj-core question_locator`, `cargo test -p mj-worker operator_away`, `cargo test -p mj-worker verdict_client`, outside the sandbox. Proxy: `cd services/jev-proxy && npm ci && npm run check && npm test && npm run deploy:dry-run`.

## Validation and Acceptance

Evaluation acceptance is the non-inferiority rule in the Decision Log, reported in `/mnt/optane/tmp/glide-afk-locator-20261113/REPORT.md` with the held-out numbers for Jev and the frozen GLiDE design side by side.

Behavioral acceptance after deployment, in an isolated instance (`--instance glide-port`, never the default instance): a session whose agent writes three replies with a question in the second and then ends its turn opens at the second reply in the TUI; with AFK on, an agent that asks the same question twice gets the second held for the person, and a different second question is declined. The worker decision log shows a `question-locator-glide` record and an AFK record for these.

Final validation, once after the last milestone: workspace `cargo clippy --all-targets -- -D warnings`, the full `cargo test` suites of `mj-core` and `mj-worker` (and `mj-controller` if Milestone 6 touches it), and the proxy `npm test`.

## Idempotence and Recovery

Evaluation scripts write into fresh step directories and never modify the store or logs they read. Labeling writes one file per case, so a rerun resumes where it stopped. The new proxy routes are additive; if one misbehaves after deployment, the previous proxy version can be redeployed with `npx wrangler rollback` while workers fall back to their failure behavior (AFK: hold for the person; locator: open at the bottom).

## Artifacts and Notes

Reconnaissance used to write this plan: `.mj/agents/87cd7c6f29ab6a7ffb0d97dbe303a713/MAP.md` (local). Precedent studies: `/mnt/optane/tmp/eot-glide-port-20261112` and `/mnt/optane/tmp/handback-recall-r2-20261111` (local).

## Interfaces and Dependencies

In `mj-core/src/activity/question_locator.rs`, `parse` takes the GLiDE probabilities object and the reply count and returns `Result<Option<usize>>` under the frozen rule. In `mj-worker/src/acp/verdict_client.rs`, `same_afk_question` keeps its signature `(&self, current: &Value, previous: &[Value]) -> Result<bool>` (true means same) and `locate_question` keeps its signature. No new crates or dependencies.
