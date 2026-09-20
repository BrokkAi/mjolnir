# Jev proxy operations

The public service is `https://mj-jev-proxy.eng-admin-a63.workers.dev`, deployed as `mj-jev-proxy` in the Brokk Cloudflare account. Its POST routes include `/v1/turn-verdict`, `/v2/turn-verdict`, and `/v1/help-search`. The v2 route is prepared in source; deploy the proxy before releasing a worker that uses it. Source, pinned development dependencies, and Wrangler configuration live in `services/jev-proxy/` in the OSS repository. Deployment is independent of mj releases. GitHub CI validates changes but does not deploy them.

The turn-verdict route accepts POST JSON turn evidence. It supplies the fixed model and questions from `mj-core/src/activity/verdict_questions.json` (v2) or the frozen `verdict_questions_v1.json` (v1), calls TypeSafe, and returns the two typed answers consumed by mj. A local TypeSafe key makes mj use TypeSafe directly; otherwise mj uses the public endpoints without credentials. Changing a shared question resource affects both implementations and requires deploying the Worker as well as releasing mj.

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

Request and upstream response bodies are bounded to 64 KiB. Both versions retain prompt 1024-byte, assistant 2048-byte, active-title 128-byte and sixteen-active-tool caps. V1 accepts eight recent 128-byte tool strings. V2 replaces `recent_tools` with `transcript_summary`, at most 48 KiB: Jev selects the conversation from the latest delivered user message and removes all transcript tool-call entries before budgeting. Separate live-tool, background-work, and queue facts remain. Other summary consumers retain the eight-call policy. Bounded text excerpts are explicitly marked. The Rust collector also measures serialized JSON (including escaping) to keep the entire request under 64 KiB. Upstream requests have an eight-second deadline, including reading the response, and do not follow redirects. Rate limiting returns 429 with Retry-After; upstream errors return sanitized 502/504 responses. mj preserves its current activity state on failures.

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
