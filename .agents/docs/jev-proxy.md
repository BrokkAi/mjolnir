# Jev proxy operations

The public service is `https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/turn-verdict`, deployed as `mj-jev-proxy` in the Brokk Cloudflare account. Source, pinned development dependencies, and Wrangler configuration live in `services/jev-proxy/` in the OSS repository. Deployment is independent of mj releases. GitHub CI validates changes but does not deploy them.

The Worker accepts only POST JSON turn evidence. It supplies the fixed model and questions from `mj-core/src/activity/verdict_questions.json`, calls TypeSafe, and returns the two typed answers consumed by mj. A local TypeSafe key makes mj use TypeSafe directly; otherwise mj uses this public endpoint without credentials. Changing the question resource affects both implementations and requires deploying the Worker as well as releasing mj.

## Deployment and key rotation

From `services/jev-proxy/`, use Node 24 and run `npm ci`, `npm run check`, `npm test`, and `npm run deploy:dry-run`. Run `npx wrangler login` once if the local account is not authenticated; `npx wrangler whoami` shows the selected account. Deploy with `npm run deploy` after validation. Wrangler prints the deployed URL and version ID.

The provider key is the `TYPESAFE_API_KEY` Cloudflare secret. Install or rotate it with `npx wrangler secret put TYPESAFE_API_KEY`, entering the value at the private prompt. Never put it in wrangler.jsonc, source, a shell argument, or Git. `.dev.vars*` and `.env*` are ignored for local development. Before the secret is installed, valid requests return 503. Updating a secret creates a new deployed version; code deployment alone does not erase existing secrets.

Verify with synthetic evidence only:

    curl --fail-with-body https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/turn-verdict \
      -H 'Content-Type: application/json' \
      --data '{"harness":"claude","phase":"replied","silent_for_s":0,"tools_in_flight":[],"recent_tools":[],"background_commands":0,"queued_commands":0,"user_prompt_tail":"Say hello.","assistant_text_tail":"Hello!"}'

Expect HTTP 200 with `answers.waiting_on` containing `type`, `choice`, and `confidence`, and `answers.asked_question` containing `type` and `noul`. Specific probabilities are not deterministic. Malformed evidence should return 400 without calling TypeSafe.

## Limits and data handling

The endpoint is public. The Cloudflare rate limiter allows approximately 120 requests per 60 seconds per client IP at each Cloudflare location. Users behind the same NAT share this allowance. This is abuse mitigation, not a global spending ceiling; there is no daily budget counter. Adjust the binding limit in wrangler.jsonc and deploy when necessary.

Request and upstream response bodies are bounded to 64 KiB. Evidence fields have the same byte/list caps as mj's Rust collector: prompt 1024 bytes, assistant 2048 bytes, tool titles 128 bytes, eight recent tools and sixteen active tools. Upstream requests have an eight-second deadline, including reading the response, and do not follow redirects. Rate limiting returns 429 with Retry-After; upstream errors return sanitized 502/504 responses. mj preserves its current activity state on failures.

Evidence includes recent conversation text and tool titles; both Cloudflare and TypeSafe process it. The Worker has no application logging, persistence, or caching of evidence, credentials, IPs, or provider response bodies. Platform request metadata and vendor retention remain governed by those services. Do not enable payload logging or use real conversations for smoke tests.

## Monitoring and recovery

Use Cloudflare's Worker metrics to monitor requests, failures, latency, and rate limiting, and TypeSafe's usage dashboard for provider consumption. No log sink is configured by this package. Increases in 429 can reflect shared NATs; 502/504 can reflect provider availability or deadlines. Keep failures conservative in the client instead of retrying immediately.

Inspect versions with `npx wrangler deployments list`. Restore a known-good version with `npx wrangler rollback VERSION_ID --message 'Reason for rollback'`. Check secret changes before rolling back: a historical version may reference older secret configuration. Reapply the intended key with `secret put` and repeat the synthetic check when needed. Preserve the public hostname because released clients embed it. To suspend provider spending without changing the hostname, remove the secret with `npx wrangler secret delete TYPESAFE_API_KEY`; requests then fail closed with 503 until it is restored.

The first deployment on 2026-09-19 was verified with HTTP 200 and valid typed answers, including a request without a User-Agent (matching the Rust client). No real conversation was sent.
