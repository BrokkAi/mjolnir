# Jev proxy operations

The public service is `https://mj-jev-proxy.eng-admin-a63.workers.dev`, deployed as `mj-jev-proxy` in the Brokk Cloudflare account. Its POST routes include `/v1/turn-verdict` through `/v4/turn-verdict`, `/v1/help-search`, `/v1/continuation-verdict`, and `/v2/continuation-verdict`. Source, pinned development dependencies, and Wrangler configuration live in `services/jev-proxy/` in the OSS repository. Deployment is independent of mj releases. GitHub CI validates changes but does not deploy them.

The turn-verdict routes accept POST JSON turn evidence. The proxy supplies the fixed model and versioned questions: `verdict_questions.json` for v4, frozen `verdict_questions_v3.json` for v3, and the frozen v2/v1 files for older routes. A local TypeSafe key makes mj use TypeSafe directly; otherwise mj uses the public endpoint without credentials. Changing the current question resource affects both implementations and requires deploying the Worker as well as releasing mj.

## Help search

`POST /v1/help-search` accepts `{query, entries}`. Each entry has an integer `id` (0–127, unique within the request), `category`, `label`, and `description`. The client supplies its own static help catalog so old and new client versions work with the same deployment. Maximums are 128 entries, 1024 UTF-8 bytes for the query, 128 bytes for a category, 256 for a label, and 1024 for a description. Query, category and label must be nonempty (query must not be whitespace). The whole request remains limited to 64 KiB. Unknown request fields are rejected.

The Worker builds one relevance question per entry using `mj-core/src/help_search/question.json`; callers cannot supply questions, models or endpoints. Jev answers all questions in one request without an index. The response is `{scores: [{id, probability}]}` with exactly one finite probability in [0,1] for each submitted ID. Incomplete or malformed upstream answers return 502, not partial results.

The TUI updates substring matches immediately, then waits 400 ms without query edits before asking for semantic matches. It shows at most eight additional results at probability ≥0.70. Requests are cancelled on edits, closing help and shutdown; generation checks reject late replies. Failures retain local filtering and display a fallback status, with no automatic retry. Local queries beyond the remote byte limit remain searchable locally. The client deadline is ten seconds; the Worker retains its eight-second deadline.

Synthetic verification:

    curl --fail-with-body https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/help-search \
      -H 'Content-Type: application/json' \
      --data '{"query":"leave agents running when I exit","entries":[{"id":0,"category":"Essentials","label":"Detach from this terminal","description":"Leave this terminal client; the daemon and its sessions keep running."},{"id":1,"category":"Sessions","label":"Stop session","description":"Stop the selected session."}]}'

Expect HTTP 200 and a score for each ID. Detach should be relevant and Stop session should not; exact probabilities can vary. Also verify the turn-verdict route after deployment. Rolling back to a version without help search makes help fall back to substring search.

## Deployment and key rotation

From `services/jev-proxy/`, use Node 24 and run `npm ci`, `npm run check`, `npm test`, and `npm run deploy:dry-run`. Run `npx wrangler login` once if the local account is not authenticated; `npx wrangler whoami` shows the selected account. Deploy with `npm run deploy` after validation. Wrangler prints the deployed URL and version ID.

The provider key is the `TYPESAFE_API_KEY` Cloudflare secret. Install or rotate it with `npx wrangler secret put TYPESAFE_API_KEY`, entering the value at the private prompt. Never put it in wrangler.jsonc, source, a shell argument, or Git. `.dev.vars*` and `.env*` are ignored for local development. Before the secret is installed, valid requests return 503. Updating a secret creates a new deployed version; code deployment alone does not erase existing secrets.

Verify with synthetic evidence only:

    curl --fail-with-body https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/turn-verdict \
      -H 'Content-Type: application/json' \
      --data '{"harness":"claude","phase":"replied","silent_for_s":0,"tools_in_flight":[],"recent_tools":[],"background_commands":0,"queued_commands":0,"user_prompt_tail":"Say hello.","assistant_text_tail":"Hello!"}'

Expect HTTP 200 with `answers.waiting_on` containing `type`, `choice`, and `confidence`, and `answers.asked_question` containing `type` and `noul`. Specific probabilities are not deterministic. Malformed evidence should return 400 without calling TypeSafe.

## Limits and data handling

The endpoints are public. Separate Cloudflare rate limiters (`TURN_RATE_LIMITER` and `HELP_RATE_LIMITER`) each allow approximately 120 requests per 60 seconds per client IP at each Cloudflare location. Help traffic does not consume turn-verdict capacity. Users behind the same NAT share these allowances. This is abuse mitigation, not a global spending ceiling; there is no daily budget counter. Adjust the binding limits in wrangler.jsonc and deploy when necessary.

Request and upstream response bodies are bounded to 64 KiB. All turn versions retain prompt 1024-byte, assistant 2048-byte, active-title 128-byte and sixteen-active-tool caps. V1 accepts eight recent 128-byte tool strings. V2–v4 use `transcript_summary`, at most 48 KiB: Jev selects the conversation from the latest delivered user message and removes all transcript tool-call entries before budgeting. V4 adds a stop reason of at most 128 bytes and optional diagnostic fields capped at 4096 bytes for message, 128 for code, and 256 for reset text. Separate live-tool, background-work, and queue facts remain. Other summary consumers retain the eight-call policy. Bounded text excerpts are explicitly marked. The Rust collector also measures serialized JSON (including escaping) to keep the entire request under 64 KiB. Upstream requests have an eight-second deadline, including reading the response, and do not follow redirects. Rate limiting returns 429 with Retry-After; upstream errors return sanitized 502/504 responses. mj preserves its current activity state on failures.

Current v2 evidence includes current-request conversation text and separate live-tool names/status/age, without transcript tool-call bodies. The initial v2 implementation included eight recent tool-call arguments/results; the proxy still accepts that earlier v2 payload shape. V1 evidence includes recent conversation text and tool titles; both Cloudflare and TypeSafe process it. The Worker has no application logging, persistence, or caching of evidence, credentials, IPs, or provider response bodies. Platform request metadata and vendor retention remain governed by those services. Do not enable payload logging or use real conversations for smoke tests.

Help-search requests contain only the entered search query and static help descriptions, not session contents, configured bindings or local availability details. The same no-payload-logging and no-persistence behavior applies.

## Monitoring and recovery

Use Cloudflare's Worker metrics to monitor requests, failures, latency, and rate limiting, and TypeSafe's usage dashboard for provider consumption. No log sink is configured by this package. Increases in 429 can reflect shared NATs; 502/504 can reflect provider availability or deadlines. Keep failures conservative in the client instead of retrying immediately.

Inspect versions with `npx wrangler deployments list`. Restore a known-good version with `npx wrangler rollback VERSION_ID --message 'Reason for rollback'`. Check secret changes before rolling back: a historical version may reference older secret configuration. Reapply the intended key with `secret put` and repeat the synthetic check when needed. Preserve the public hostname because released clients embed it. To suspend provider spending without changing the hostname, remove the secret with `npx wrangler secret delete TYPESAFE_API_KEY`; requests then fail closed with 503 until it is restored.

The first deployment on 2026-09-19 was verified with HTTP 200 and valid typed answers, including a request without a User-Agent (matching the Rust client). No real conversation was sent.

Help search was deployed as version `c625e686-1591-420d-b6ae-2dc5902ffd63` on 2026-09-20 UTC (2026-09-19 America/Chicago). Synthetic requests returned HTTP 200: “leave agents running when I exit” scored Detach 0.96, and “show two conversations side by side” scored Open in split right 0.85. Turn-verdict still returned typed answers with `finished` confidence 0.98.

## Shared transcript summary rollout

Deploy the backward-compatible proxy before shipping mj workers using `/v2/turn-verdict`. V1 retains its exact field validator and question resource so old workers continue working. Test v2 with synthetic evidence replacing `recent_tools` in the example with `transcript_summary`; both routes must return typed answers. No deployment was performed by the shared-summary implementation task.

Worker request logs report summary size and active tool count rather than serializing evidence. The separate, user-authorized bifrost2 replay is documented in `.agents/docs/jev-bifrost2-evidence-experiment-20260920.md`; it is not a production smoke-test procedure.

The 0+1 policy selection and authorized fifty-request strict pilot plus one-hundred-request exploratory comparison are documented in `.agents/docs/jev-turn-evidence-comparison-20260920.md`. Confirmed steering establishes a new delivered-user boundary; merely queued or unconfirmed steering does not.

## Authorized continuation

`POST /v1/continuation-verdict` accepts `{assistant_history_omitted, messages}` with whole `{id, role, text}` user/assistant messages. It uses frozen questions in `mj-core/src/continuation/questions_v1.json` and returns typed Noul `unfinished` and `no_input_needed` answers. The client requires both scores ≥0.90. User text is limited to 32 KiB, assistant text to 16 KiB, and the serialized request to 64 KiB. Omitted assistant context is explicit; incomplete user authorization causes local abstention. Tool bodies are excluded. This evidence intentionally spans multiple user exchanges, unlike the activity classifier's current-request scope.

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
