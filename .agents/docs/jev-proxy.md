# Jev proxy operations

The public service is `https://mj-jev-proxy.eng-admin-a63.workers.dev`, deployed as `mj-jev-proxy` in the Brokk Cloudflare account. Its POST routes include `/v1/turn-verdict` through `/v6/turn-verdict`, `/v1/continuation-verdict`, `/v2/continuation-verdict`, `/v1/github-item-verdict`, and `/v1/question-locator-glide`. Source, pinned development dependencies, and Wrangler configuration live in `services/jev-proxy/` in the OSS repository. Deployment is independent of mj releases. GitHub CI validates changes but does not deploy them.

The turn-verdict routes accept POST JSON turn evidence. The proxy supplies the fixed model and versioned questions: `verdict_questions.json` for v6, frozen `verdict_questions_v5.json` for v5, `verdict_questions_v4.json` for v4, `verdict_questions_v3.json` for v3, and the frozen v2/v1 files for older routes. A local TypeSafe key makes mj use TypeSafe directly; otherwise mj uses the public endpoint without credentials. Changing the current question resource affects both implementations and requires deploying the Worker as well as releasing mj.

## Help search (removed)

`POST /v1/help-search` and `HELP_RATE_LIMITER` were removed from the source on 2026-11-13 at the user's direction ("if not, just remove it instead of porting to glide"). No Rust code called the route after the command palette replaced Help on 2026-10-27; only releases before that date used it, and they get a 404 once the proxy is deployed from this source. The route stays live until that deployment. Do not deploy the proxy just for this; it goes out with the next proxy deployment.

## Deployment and key rotation

From `services/jev-proxy/`, use Node 24 and run `npm ci`, `npm run check`, `npm test`, and `npm run deploy:dry-run`. Run `npx wrangler login` once if the local account is not authenticated; `npx wrangler whoami` shows the selected account. Deploy with `npm run deploy` after validation. Wrangler prints the deployed URL and version ID.

The provider key is the `TYPESAFE_API_KEY` Cloudflare secret. Install or rotate it with `npx wrangler secret put TYPESAFE_API_KEY`, entering the value at the private prompt. Never put it in wrangler.jsonc, source, a shell argument, or Git. `.dev.vars*` and `.env*` are ignored for local development. Before the secret is installed, valid requests return 503. Updating a secret creates a new deployed version; code deployment alone does not erase existing secrets.

Verify with synthetic evidence only:

    curl --fail-with-body https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/turn-verdict \
      -H 'Content-Type: application/json' \
      --data '{"harness":"claude","phase":"replied","silent_for_s":0,"tools_in_flight":[],"recent_tools":[],"background_commands":0,"queued_commands":0,"user_prompt_tail":"Say hello.","assistant_text_tail":"Hello!"}'

Expect HTTP 200 with `answers.waiting_on` containing `type`, `choice`, and `confidence`, and `answers.asked_question` containing `type` and `noul`. Specific probabilities are not deterministic. Malformed evidence should return 400 without calling TypeSafe.

## Limits and data handling

The endpoints are public. Separate Cloudflare rate limiters (`TURN_RATE_LIMITER`, `CONTINUATION_RATE_LIMITER` and `GITHUB_ITEM_RATE_LIMITER`) each allow approximately 120 requests per 60 seconds per client IP at each Cloudflare location. Users behind the same NAT share these allowances. This is abuse mitigation, not a global spending ceiling; there is no daily budget counter. Adjust the binding limits in wrangler.jsonc and deploy when necessary.

Request and upstream response bodies are bounded to 64 KiB. All turn versions retain prompt 1024-byte, assistant 2048-byte, active-title 128-byte and sixteen-active-tool caps. V1 accepts eight recent 128-byte tool strings. V2–v4 use `transcript_summary`, at most 48 KiB: Jev selects the conversation from the latest delivered user message and removes all transcript tool-call entries before budgeting. V4 adds a stop reason of at most 128 bytes and optional diagnostic fields capped at 4096 bytes for message, 128 for code, and 256 for reset text. Separate live-tool, background-work, and queue facts remain. Other summary consumers retain the eight-call policy. Bounded text excerpts are explicitly marked. The Rust collector measures the serialized direct request, questions included, to keep the entire request within 64 KiB. The hosted request omits the questions, so it is smaller, and the proxy's upstream request has the same shape as the direct one. Upstream requests have an eight-second deadline, including reading the response, and do not follow redirects. Rate limiting returns 429 with Retry-After; upstream errors return sanitized 502/504 responses. mj preserves its current activity state on failures.

Current v2 evidence includes current-request conversation text and separate live-tool names/status/age, without transcript tool-call bodies. The initial v2 implementation included eight recent tool-call arguments/results; the proxy still accepts that earlier v2 payload shape. V1 evidence includes recent conversation text and tool titles; both Cloudflare and TypeSafe process it. The Worker has no application logging, persistence, or caching of evidence, credentials, IPs, or provider response bodies. Platform request metadata and vendor retention remain governed by those services. Do not enable payload logging or use real conversations for smoke tests.

Help-search requests contain only the entered search query and static help descriptions, not session contents, configured bindings or local availability details. The same no-payload-logging and no-persistence behavior applies.

## Monitoring and recovery

Use Cloudflare's Worker metrics to monitor requests, failures, latency, and rate limiting, and TypeSafe's usage dashboard for provider consumption. No log sink is configured by this package. Increases in 429 can reflect shared NATs; 502/504 can reflect provider availability or deadlines. Keep failures conservative in the client instead of retrying immediately.

Inspect versions with `npx wrangler deployments list`. Restore a known-good version with `npx wrangler rollback VERSION_ID --message 'Reason for rollback'`. Check secret changes before rolling back: a historical version may reference older secret configuration. Reapply the intended key with `secret put` and repeat the synthetic check when needed. Preserve the public hostname because released clients embed it. To suspend provider spending without changing the hostname, remove the secret with `npx wrangler secret delete TYPESAFE_API_KEY`; requests then fail closed with 503 until it is restored.

The first deployment on 2026-09-19 was verified with HTTP 200 and valid typed answers, including a request without a User-Agent (matching the Rust client). No real conversation was sent.

Help search was deployed as version `c625e686-1591-420d-b6ae-2dc5902ffd63` on 2026-09-20 UTC (2026-09-19 America/Chicago). Synthetic requests returned HTTP 200: “leave agents running when I exit” scored Detach 0.96, and “show two conversations side by side” scored Open in split right 0.85. Turn-verdict still returned typed answers with `finished` confidence 0.98.

## Shared transcript summary rollout

Deploy the backward-compatible proxy before shipping mj workers using `/v2/turn-verdict`. V1 retains its exact field validator and question resource so old workers continue working. Test v2 with synthetic evidence replacing `recent_tools` in the example with `transcript_summary`; both routes must return typed answers. No deployment was performed by the shared-summary implementation task.

Worker request logs report summary size and active tool count rather than serializing evidence. The separate, user-authorized bifrost2 replay is summarized in `.agents/docs/jev.md` (section "What the evaluations found"); it is not a production smoke-test procedure.

The 0+1 policy selection and the authorized fifty-request strict pilot plus one-hundred-request exploratory comparison are summarized in `.agents/docs/jev.md`; the full report is in Git history at `.agents/docs/jev-turn-evidence-comparison-20260920.md` (commit af11979f). Confirmed steering establishes a new delivered-user boundary; merely queued or unconfirmed steering does not.

## Authorized continuation

Retired in the client (#1532, 2026-11-13): current releases never call `/v1` or `/v2/continuation-verdict`. The routes stay for installed older releases, because the hosted proxy is shared by every release; retiring them is a separate decision. Their question files moved from `mj-core/src/continuation/` to `services/jev-proxy/src/continuation-questions.json` (v2) and `continuation-questions-v1.json`.

`POST /v1/continuation-verdict` accepts `{assistant_history_omitted, messages}` with whole `{id, role, text}` user/assistant messages. It uses frozen questions in `services/jev-proxy/src/continuation-questions-v1.json` and returns typed Noul `unfinished` and `no_input_needed` answers. The client requires both scores ≥0.90. User text is limited to 32 KiB, assistant text to 16 KiB, and the serialized request to 64 KiB. Omitted assistant context is explicit; incomplete user authorization causes local abstention. Tool bodies are excluded. This evidence intentionally spans multiple user exchanges, unlike the activity classifier's current-request scope.

`CONTINUATION_RATE_LIMITER` has its own 120/minute per-IP budget. Missing bindings fail closed. The endpoint shares existing transport deadlines, sanitized errors, and no-payload-logging behavior. Deploy the route before releasing clients that use it; smoke-test only synthetic scenarios. Rollback leaves clients abstaining without automatic continuation.

Continuation and v2 support were deployed on 2026-09-20 UTC as version `abf42692-4729-42f5-98d1-26041fbe95c6`. Synthetic smoke checks returned HTTP 200 for all four routes. The continuation example scored unfinished=0.96 and no_input_needed=0.94. Requests without User-Agent, matching mj's Rust client, succeeded; the edge rejected the generic Python urllib User-Agent with 403. No private session content was used for deployment smoke tests.

## V3 input and work assessments

The additive `/v3/turn-verdict` route accepts the same bounded evidence as v2. It asks `needs_user_input` (Noul probability) independently of `work_state` (choice and confidence). Background work can coexist with a current request for approval. The new worker uses v3; v1/v2 retain their previous question files and response validators. Deploy the additive proxy before distributing the new worker. Endpoint failures preserve activity and never fall back to v2.

Run `npm test`, `npm run check`, and `npm run deploy:dry-run` in `services/jev-proxy` before publication. The route was deployed with quota-aware continuation on 2026-09-21, as recorded below. Synthetic-only observations are recorded in `jev-turn-verdict-v3-synthetic.json`; the initial wording experiment is retained separately. These observations are not a comparative evaluation of history selection or private sessions.

## V4 transient server retries

`POST /v4/turn-verdict` retains v3's independent input and work questions and adds a Noul `retryable_server_error` probability. Completed turns may include `completion` with a stop reason and a bounded provider diagnostic; running turns omit it. The classifier considers current structured errors and final reply text from every harness. It should score transient provider overload, temporary server failure, and short throttling high, while leaving quota, authentication, local tool, transport, historical examples, and uncertain errors low. mj requires at least 0.90 before its worker schedules the existing durable `Continue` backoff. A missing or failed Jev answer leaves the provider error visible.

V3 keeps the frozen `verdict_questions_v3.json` contract. Deploy v4 before distributing workers that call it. Check v4 and v3 after deployment with synthetic POST evidence; do not send private conversation text.

V4 was deployed on 2026-09-23 as version `bd69b6cf-199b-4e41-96bb-cde8e3d05cbd`; the previous v3 deployment is `82f5f640-5ac7-4588-902c-99154fb2fa98`. TypeScript checks, proxy tests, and the Wrangler dry run passed. Four synthetic repeats of a standalone model-capacity reply scored retryability 0.91–0.94. A temporary 503 and a one-minute throttle scored 0.98 each; quota, authentication, local process failure, completed work, and a quoted capacity example scored 0.01–0.20. The legacy turn, continuation, and help routes returned their expected typed answer shapes, and malformed v4 evidence returned HTTP 400. No private conversation was sent.

## Quota-aware continuation v2

`POST /v2/continuation-verdict` preserves the ordinary continuation questions and adds independent `quota_limit` confidence. The optional `quota_message` contains only the current completed reply and provider diagnostic, bounded to 16 KiB. When ordinary authorization evidence is unavailable, `messages` may be empty only with a nonempty `quota_message`; the controller then prohibits ordinary continuation regardless of classifier scores. A quota score of at least 0.90 selects quota recovery before ordinary continuation. Classification applies to every harness, excludes historical examples and request throttling, and never calculates a reset deadline.

The proxy retains `/v1/continuation-verdict` with its original two-question contract. The v2 route uses the existing continuation rate limiter and transport bounds. `npm test` and `npm run check` validate both versions. This route is deployed and ready for updated hosted clients. Direct TypeSafe callers send the new shared questions without requiring the hosted route.

The daemon calculates reset deadlines from current quota reports, durable last-success cache entries, or deterministically parsed reset text. It waits for all known exhausted windows and adds 60 seconds. Pending recovery and abstention notices are durable relay state. The existing continuation setting controls both behaviors, while quota retries preserve the three-attempt ordinary continuation allowance. A missing reset produces one conversation notice, not speculative retries. Protocol 20 and relay snapshot 11 carry recovery; database revision 44 raises the compatibility floor because old readers cannot interpret the new command variants.

Deployed on 2026-09-21 as version `82f5f640-5ac7-4588-902c-99154fb2fa98` from commit `72dbefee`, following explicit user authorization. Existing proxy tests, TypeScript checks, and deployment dry-run validation passed before publication. Synthetic requests without User-Agent returned HTTP 200 for all six routes. The session-limit example scored quota_limit=0.92; a completed implementation quoting a historical quota example scored 0.04. V1 continuation retained exactly its two answers (unfinished=0.96, no_input_needed=0.95), v3 returned typed work_state and needs_user_input answers, and malformed v2 continuation evidence returned HTTP 400. No private session content was sent. Local smoke results are in `/mnt/optane/mj-quota-recovery-validation/deployment-smoke.json`.

## V5 three-axis assessment and V6 background judgment

`POST /v5/turn-verdict` carries the three-axis contract (`failure`, `input`, `work`, each a choice with confidence) with the optional whole-message `authorization` history. It was deployed before 2026-09-27; the worker used it until v6. Its questions are frozen as `verdict_questions_v5.json`.

`POST /v6/turn-verdict` accepts the v5 evidence plus an optional `background` list (at most 16 entries of `{id, command, started_s_ago}`, command at most 128 bytes) naming the commands behind `background_commands`, and asks a fourth question, `background` (`needed`, `unneeded`, `unclear`), from the frozen `verdict_questions_v6.json`. The worker uses a confident answer to decide whether leftover processes keep the session from being quiet (see `.agents/docs/jev.md`, "Quiet"). An answer without `background` is still valid. v6 also accepts an optional `final_tool_calls` list (at most 16 of `{name, status}`, name at most 128 bytes, status at most 32) naming the tool calls made after the last assistant text. Released v6 workers keep using this route; current workers call v7 (`mj-worker/src/acp/verdict_client.rs`).

The current v6 failure question includes network connectivity failures while calling the model/provider API in `transient_provider`: dropped/reset connections, refusal, timeout, socket hang up, unreachable networks, temporary DNS failures and disconnected response streams. A current provider diagnostic is sufficient even without a final reply or HTTP status. These use the same worker-owned durable 1/2/4/8/16-minute `Continue` backoff as temporary 5xx failures. Local tool/command network failures, harness process crashes, authentication and permanent certificate/configuration errors remain `other`; quota keeps its separate recovery path. Historical examples and recovered warnings do not arm retries. V5 and older routes retain their frozen questions.

Validation on 2026-10-08 used reconstructed screenshot evidence (S45) and synthetic controls, three repeats per case with direct Jev `jev-1.13.0`. The old prompt never selected retry for the screenshot (two `await_input`, one `uncertain`); the new prompt selected `retry_provider` at failure confidence 1.0 on all three. All 42 candidate responses classified the intended failure: 24 provider failures selected retry and 18 controls avoided provider retry, including three quota cases selecting quota recovery. The three local `git push` network failures were `other` at confidence 0.85–0.87, preserving `uncertain` rather than forcing input.

A further three repeats per prompt used the exact worker diagnostic code (`-32603` for JSON-RPC Internal error rather than the raw bridge's `errorKind`): baseline always selected `await_input`, candidate always selected retry at confidence 1.0. S45 records that worker-shaped response. This is a small synthetic comparison, not a measure of production accuracy. Raw prompts, cases and results are in `/mnt/optane/mj-jev-network-connectivity-20261008/`; the replay fixture records the screenshot's incomplete provenance. Proxy tests (30), TypeScript checking, the deployment dry run, Jev corpus policy tests (2) and durable retry tests (12) passed. Hosted question updates take effect through proxy deployment; direct-key workers require the updated mj binary.

Deployed on 2026-10-08 as version `0e83bd7c-bfab-46d5-b1ba-b39677149ba5` from `09a23ac9` (including network-retry change `fd8e33a5`), following explicit user authorization. Seven synthetic smoke requests returned 200: three v6 ECONNRESET repetitions and a diagnostic-only timeout selected provider retry at confidence 1.0, quota selected quota recovery, a local `git push` network failure selected `other`, and the frozen v5 route retained its confidence-only answers. No private conversation was sent. Results are in `/mnt/optane/mj-jev-network-connectivity-20261008/deployment-smoke.jsonl`.

Deployed on 2026-09-30 as version `d85bf39a-e108-4a5a-83e6-3025f63a03f3` (the previous version carried v5). TypeScript check, 24 proxy tests, and the dry run passed first. Synthetic smoke checks without a User-Agent: v6 returned four typed answers (`background: unneeded` 0.92 for a dev server left up for later), v5 returned its three and rejected the v6 fields with 400, a malformed v6 `background` list returned 400, and v4 and `/v1/continuation-verdict` returned 200. No private conversation was sent.

## Probability distributions for input and completion gates

On 2026-09-30, version `14ca6393-f397-40c7-a1c2-615c9bb32b6e` deployed the selected input prompt and full probability forwarding on v6, following explicit user authorization. It replaces `d85bf39a-e108-4a5a-83e6-3025f63a03f3`. V6 now validates each complete named distribution, finite values in [0,1], a sum within independent hundredth rounding, and a selected maximum. Provider confidence remains a separate field. V5 and earlier responses retain their frozen shapes. This prepares the hosted service for workers using the required-input probability floor 0.50 and ratio 2.5, and finished-work probability floor 0.80; those action gates execute in the worker, not the proxy.

Proxy tests, TypeScript checking, and the Wrangler dry run passed before deployment. Synthetic smoke checks returned 200 for a decision request and a finished reply, both with full distributions accepted by the updated evaluator; their actions were `await_input` and `finished`. V5 returned 200 with exactly its original three confidence-only answers, and malformed v6 evidence returned 400. No private conversation was sent. Results are saved in `/mnt/optane/mj-jev-gates-20260930/deployment-smoke.json`. Mj worker rollout remains separate.

## V7 closing-reply signal

`POST /v7/turn-verdict` accepts the v6 evidence shape and uses the live five-question resource from `mj-core/src/activity/verdict_questions.json`. It adds `reply` (`closing`, `continuing`, or `unclear`) to the four v6 judgments. Failure, input, work, and reply distributions are required; `background` remains optional. Every supplied distribution must be complete and bounded, with the selected choice a maximum. `reply` measures whether the latest response sounds like a handback or stopping point; it does not decide whether authorized work remains. The worker uses it only for stale running-turn recovery after 40 minutes of silence, alongside the no-open-tools/background/queued checks and a confident no-failure answer.

V6 remains frozen: `/v6/turn-verdict` loads `verdict_questions_v6.json` and keeps its optional `background` answer and four-distribution validation. This lets released v6 workers keep using their original contract. Deploy v7 before distributing workers that require the reply answer.

Deployed on 2026-10-09 as version `8bdbc740-9b8a-402d-a37e-2d059a639263` from `d045ad87e`, following explicit user authorization. It replaces `0e83bd7c-bfab-46d5-b1ba-b39677149ba5` (network retry); the frozen v6 file carries that change, so v6 callers keep the connectivity wording. An intermediate version `d209cac7-95b1-4fcc-89bf-959feca1b8d2`, built before the rebase onto the network-retry commit, served the earlier failure wording for about ten minutes. Proxy check, 31 tests and the dry run passed first. Synthetic smoke checks without a User-Agent: a running-turn handback scored `reply = closing` 1.00 on v7 with `work = finished`; a mid-work note scored `continuing` 1.00 with `work = authorized_unfinished`; v6 returned its four answers with no `reply`. No private conversation was sent.

## GitHub item verdict

`POST /v2/github-item-verdict` accepts `{item: {repo, kind, number, title, body, author, url}, session: {conversation, files, references, mentions?}}`. Item fields retain the v1 limits except `body` is capped at 4,224 UTF-8 bytes. Optional mentions preserve up to 16 distinct newest lines (256 UTF-8 bytes each) mentioning this item or references in its title/body, selected from a per-session index before digest truncation. These request-sizing budgets follow the 83-pair replay; the daemon interest threshold is 0.4 based on that replay. Older callers may omit mentions. Conversation is at most 8 KiB of UTF-8 text; each message is capped at 2 KiB including its truncation marker; files contain at most 32 strings of 256 bytes; references contain at most 32 strings of 128 bytes. Empty conversation/body strings are allowed; file and reference strings must be nonempty. Unknown fields are rejected. Jev receives the frozen one-question resource `mj-core/src/github_item/questions_v2.json` and returns only a typed Noul `interested` answer. V1 remains unchanged for released daemons, and both routes share `GITHUB_ITEM_RATE_LIMITER`.
Deployed v2 on 2026-11-08 as version `71d9aadf-343d-417c-818f-b8285a49651a` from `3bfa7d0b9`, following explicit user authorization. Before publication, TypeScript checking, all 37 proxy tests, the deployment dry run, and 36/36 live synthetic v2 controls passed. Hosted smoke checks without a User-Agent: the related v2 control scored interested 0.88 and the unrelated one 0.05; a v2 request with an extra session field returned 400; v1 still returned both answers (interested 0.97, created 0.01 for its related control). No private conversation was sent. Results are in `/mnt/optane/jev-github-v2-20261108/deployment-smoke.json`.

Current daemons call v2 (`mj-controller/src/github_item_verdict.rs`). The session digest comes from `mj-transcript/src/github_item.rs`: user and assistant message text from the last three materialized turns, and the newest distinct tool-call file locations and GitHub references (`mj-core/src/github_reference.rs`). Conversation fills from newest messages first, then renders oldest to newest with User/Agent labels. It omits tool payloads, automatic continuation prompts and /compact or /clear commands, and keeps peer/mailbox text. Labels, newlines and truncation markers count against the total budget. Each live session's digest is computed once per poll and reused for every item in it. Jev no longer judges creation. The daemon counts an item from its own GitHub login as opened by a session when the item's URL or `owner/repo#N` reference appears in the output of a tool call running at GitHub's creation time (`load_github_session_creation_output`). Later text in the same turn is not evidence: a session that views or mentions another session's new item would otherwise claim it and take over its creator watch. Output redirected or suppressed by the command is a miss; the creator then receives a redundant interest notice. Jev is not asked about an item its session opened.

The conversation budgets were compared on the same 83 stored item/session pairs (44 v1 positives, 39 negatives). At 0.5, 8 KiB total with a 2 KiB message cap retained 32/44 positives and recovered 15/18 provisionally adjudicated real misses, with 0/39 new positives. None of the four tested variants met the requested 16/18 recovery cutoff at 0.5; these constants choose the best recovery among them, not demonstrated accuracy parity. The replay used creation-time-plus-60-second fallback timestamps and cannot rewind later materialized-row edits. Average provider request size fell from 57,158 bytes (reconstructed v1) to 13,525. The earlier structured digest (512-byte requests and reply instead of conversation) kept only 19/44 positives at 6,384 bytes: bare references lose the relationships stated in prose. Results and dossiers are in `/mnt/optane/jev-github-v2-20261108/replay/`. The interest threshold was later lowered to 0.4 (below).

Blind labels then replaced production's verdicts as the reference: one agent labeled all 83 pairs from each session's own transcript up to the classification time, without seeing any classifier's verdict (`/mnt/optane/jev-github-v2-20261108/adjudication/labels.jsonl`; 45 relevant, 58 high-confidence labels). Against them, production v1 scored precision 0.86, recall 0.84, accuracy 0.84 at 57 KB per request. The 8 KiB conversation digest scored 0.97/0.69/0.82 at 0.5 and 0.92/0.76/0.83 at 0.4. Adding `mentions` and the 0.4 threshold scored 0.92/0.80/0.84 to 0.93/0.82/0.87 across two runs at about 14.9 KB; the second run windows each mention around its reference (`mention_window`), after 26 of 499 prefix-cut mentions had lost theirs. Run-to-run score differences averaged 0.02, and flips were all within 0.06 of the threshold. Results are in `/mnt/optane/jev-github-v2-20261108/mentions/` and `mentions-window/`.

Deployed optional `mentions` on 2026-11-08 as version `fbb5a09a-c342-4956-b48b-1ef93142c5b4` from `54322a3d1`, following explicit user authorization, before pushing the daemon that sends it. TypeScript checking, the proxy tests and 39/39 live synthetic v2 controls passed first. Hosted smoke checks without a User-Agent: a v2 request whose only link is a mention of a referenced issue scored 0.51; a v2 request without `mentions`, as older daemons send, scored 0.91 for the related control; a 300-byte mention returned 400; v1 still returned both answers. Results are in `/mnt/optane/jev-github-v2-20261108/deployment-smoke-mentions.json`.

A per-session work summary was prototyped on the same 83 pairs and not adopted. The utility-model resolver (Kimi k3.1-pro) wrote a summary of about 1.6 KB from about 48 KB of each session's recent text, in about 27 s per summary. Against the blind labels at 0.4, the summary alone scored recall 0.67 and agreement 0.80; summary plus mentions scored recall 0.78 and agreement 0.84, the same agreement as the shipped digest (recall 0.80), with 35% fewer Jev input tokens (3,280 versus 5,013). That saving does not justify a 48 KB utility-model call per turn on the user's subscriptions, nor the summary lifecycle state it would add. Summaries also dropped relationships the mentions field restores. Evidence is in `/mnt/optane/jev-github-v2-20261108/summary/` (`report.md`, `metrics.json`, `prompt.txt`).

`python3 scripts/jev-github-item-eval.py --run --output <file>` evaluates the v2 synthetic controls in `services/jev-proxy/tests/fixtures/github-item-controls-v2.json` (`--contract v1` runs the v1 set). On 2026-11-08 the conversation-field question matched all 36 classifications (12 cases, three repeats); the closest scores were 0.76 for a session reporting a different item number and 0.20 for one identified only by unrelated files. Results are in `/mnt/optane/jev-github-v2-20261108/fixtures-v2-conversation.json`.

`POST /v1/github-item-verdict` accepts `{item: {repo, kind, number, title, body, author, url}, session: {recent_turns}}`, with the item first so the provider prompt prefix is shared by every session classified for one item. `kind` is `issue` or `pull_request`; `recent_turns` is the compaction rendering of the session's last three user turns. The proxy supplies `mj-core/src/github_item/questions.json` and returns Noul `interested` and `created` answers. The daemon's GitHub watcher calls it once per new item per live primary session (`mj-controller/src/github_item_verdict.rs`); see `.agents/plans/session-mailbox-and-github-watch.md`. Item text is untrusted evidence. The route has its own `GITHUB_ITEM_RATE_LIMITER` (120 per 60 s per IP), so a burst of new items cannot use up the continuation budget.

Released daemons on v1 build `recent_turns` with `TranscriptSummary::from_snapshot_keeping(snapshot, 0)`:
the same compaction filter and turn grouping, with every tool reduced to its
name, status, and exit outcome. It never includes raw call inputs, results, or
terminal output, including the newest eight calls that ordinary handoff
compaction preserves. Conversation messages and plans remain available. The
creation prompt interprets these summaries and requires successful creation
with identifying evidence; a failed or unfinished command, or a generic
creation command without the created item's identity, does not establish ownership.
If an identity appeared only in discarded tool output, such as the URL from
`gh pr create --fill`, the session can receive its own item notification before
the assistant reports it. This is the accepted summary-only evidence tradeoff:
abstain from ownership instead of suppressing another session's relevant item.

`python3 scripts/jev-github-item-eval.py --run --output <file>` evaluates the
synthetic summary-only controls in
`services/jev-proxy/tests/fixtures/github-item-controls.json`. On 2026-11-08,
the previous prompt incorrectly treated a failed creation as successful
(created probability 0.51). Review also identified that a generic completed
creation command cannot establish which item was opened. The final prompt
passed 27/27 classifications over three repeats, scoring creation at 0.01 for
both failures and commands without identity. This is a small control set, not
a production accuracy estimate. Local evidence is under
`/mnt/optane/jev-summary-20261108/`. Shipping smaller evidence requires the
updated daemon; hosted prompt changes additionally require deploying the proxy.

Deployed the summary-aware GitHub ownership questions on 2026-11-08 as version
`ed0d18cc-6aba-43e1-98e8-8fe50c73f651` from `aed371dfe`, following explicit user
authorization. It replaces `49d6f62b-4489-454c-b132-614f58384a5a`. Before
publication, all 34 proxy tests, TypeScript checking, the deployment dry run,
and 27 live synthetic model controls passed. Hosted smoke checks returned 200
for all four requests: identified PR creation scored created 0.96; failed
creation and creation without item identity each scored 0.01; v7 turn verdict
retained its five answers and classified the synthetic completed reply as
finished/closing. Smoke requests used no User-Agent as documented above; the
default Python User-Agent received 403 at the edge. No private conversation
content was sent. Results are in
`/mnt/optane/jev-summary-20261108/deployment-smoke.json`.

Deployed on 2026-10-07 as version `684c5a50-d4ba-4392-822e-09152a1cb57b`, replacing `14ca6393-f397-40c7-a1c2-615c9bb32b6e`, following explicit user authorization. TypeScript check, 28 proxy tests and the dry run passed first. Synthetic smoke checks without a User-Agent: a pull request the session's turns show it opening scored interested 0.96 and created 0.97; an unrelated README typo issue scored 0.03 and 0.02; a malformed item returned 400; v6 (running turn), v1 turn-verdict and v1 continuation returned 200. A v6 request with `phase: replied` and no `completion` correctly returns 400. No private conversation was sent.

## Question locator

`POST /v1/question-locator` accepts exactly `{latest_user_message, replies}`, where the first field is a string of at most 4096 UTF-8 bytes and `replies` contains 1–32 ordered `{reply, text}` objects. Each `reply` equals its zero-based array index, each text is at most 3008 UTF-8 bytes, unknown fields are rejected, and the whole request is limited to 64 KiB. Invalid evidence returns HTTP 400 with `invalid_evidence`.

The proxy builds the `request_reply` choice question from `mj-core/src/activity/question_locator.json` and the number of replies, then calls `jev-latest`. It uses `TURN_RATE_LIMITER`. A valid response is `{"choice":"r0","probabilities":{"r0":0.8,"none":0.2},"confidence":0.8}` (keys vary with reply count); `none` means no reply was located. Malformed upstream answers return HTTP 502.

Deployed on 2026-11-05 as version `bb1bb11b-4b4f-4f9b-afa3-b3d01567b717`, from commit `f598ac442`; the previous version was `8bdbc740-9b8a-402d-a37e-2d059a639263`. Synthetic requests without a User-Agent returned HTTP 200 for a three-reply locator case (`r0` at 0.96, the reply that assigned a decision), HTTP 400 `invalid_evidence` for a reply index that did not match its position, and HTTP 200 with all five answers from `/v7/turn-verdict`. No private conversation text was sent.

## Question locator on GLiDE

`POST /v1/question-locator-glide` is the route current workers use; `/v1/question-locator` stays for older releases and still calls `jev-latest`. It accepts the same state and limits as the Jev route (`questionLocatorRequest` is shared), but reads up to `GLIDE_BODY_BYTES` and uses `FASTINO_API_KEY` and `TURN_RATE_LIMITER`. The handler is `questionLocatorGlideVerdict` in `services/jev-proxy/src/index.ts`; question building and answer checking are in `question-locator-glide.ts`.

The proxy sends the frozen V0 question through `glide()`: the instructions and `none` text from `mj-core/src/activity/question_locator.json`, with criteria `rK` ("Reply K (replies[K]) contains the open request.") and `none` as plain strings. `mj-core/src/activity/question_locator_glide_example_n3.json` is the exact three-reply question the evaluation sent; a test in `mj-core` and one in the proxy compare against it. The proxy requires a probability for every label, each in [0, 1], summing to 1 within 0.03, and returns `{"probabilities": {...}}`; provider errors pass through unchanged, including `Retry-After`, and malformed answers return HTTP 502 `invalid_upstream_response`. The worker applies the rule: argmax over the replies and `none`, a tie among replies goes to the newest, `none` wins only when strictly higher, and `none` stores nothing. The worker always calls the hosted route, even with a local TypeSafe key, because the Fastino key exists only as the proxy secret. It sends no size-refusal retry: the state builder keeps the request near 60 KiB, far below the bound. Evaluation and rule: `/mnt/optane/tmp/glide-afk-locator-20261113/locator/glide/` (`REPORT.md`, `FROZEN/RULE.md`) and `.agents/plans/afk-and-locator-on-glide.md`. Not deployed yet.

## Still-open input wording and locator (2026-11-06)

Deployed on 2026-11-06 as version `49d6f62b-4489-454c-b132-614f58384a5a` from `ac2f93ea9`, following explicit user authorization; the previous version was `bb1bb11b-4b4f-4f9b-afa3-b3d01567b717`. It carries the new `input` instructions in `verdict_questions.json` (v7) and the matching `question_locator.json`; frozen v1-v6 files are unchanged. Proxy check, 34 tests and the dry run passed first. Synthetic smoke requests without a User-Agent: an offer after a delivered answer scored `input = required` 0.97 with `work = finished` and `reply = closing`; a reply only waiting on a test run scored `none` 1.00; "Want me to proceed and push it?" for assigned work scored `redundant_request` 0.99; a closing "Still open: your sign-off on ..." item scored `required` 0.95; the locator chose the offer reply (`r1`, 1.00); v6 returned its four answers. No private conversation was sent. Evaluation: [still-open input detection](jev-still-open-20261106.md).
