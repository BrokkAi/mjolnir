# Public Jev turn-verdict proxy

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture

Every mj user should receive the existing turn classification without supplying a TypeSafe key. A small Cloudflare Worker accepts only bounded turn evidence, supplies the fixed Jev questions and server-owned key, and returns typed answers. Existing installations with a local TypeSafe key keep using the direct API. Errors preserve the current turn state.

## Progress

- [x] (2026-09-19) Read repository guidance and existing client; initial working tree is clean.
- [x] (2026-09-19) Created the Worker and shared questions; ten proxy behavior tests, TypeScript checking, and deployment dry-run passed.
- [x] (2026-09-19) User completed Wrangler login; deployed, installed the secret, and verified synthetic HTTP 200 answers before embedding the actual endpoint.
- [x] (2026-09-19) Added direct/hosted routing and loopback tests, npm CI, user documentation and operational runbook.
- [x] (2026-09-19) Full Rust tests, Clippy with warnings denied, portable musl worker build, formatting, package-content and diff checks passed.
- [x] (2026-09-19) Completed the implementation for the final commit on branch `hel2`; no push is authorized or performed.

## Surprises & Discoveries

Wrangler is not installed globally; Node 24 and npm are available. Cloudflare authentication and assigned workers.dev hostname will be discovered using the installed CLI. The core crate uses an explicit Cargo package include list, so the shared JSON was added to that list to preserve published builds. An initial Python urllib smoke request returned HTTP 403; curl requests both with and without a User-Agent returned HTTP 200. The rate limiter is approximate and local to each Cloudflare location; it is not a daily spending ceiling.

## Decision Log

The user selected a public service for all mj users, fixed turn-verdict questions, and rate limits without a global daily cap. Keep the independently deployed npm package in `services/jev-proxy/`. Use TypeScript and local Wrangler, an initial rate of 120 requests per minute per Cloudflare-reported client IP, and a versioned `/v1/turn-verdict` endpoint. No account system, durable shared counter, or database migration is needed. Deployment and synthetic verification are part of the user-approved implementation plan; do not push Git changes or configure automatic deployment.

## Outcomes & Retrospective

The public proxy is live at https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/turn-verdict. Its synthetic response contained a finished verdict with confidence 0.95 and asked_question probability 0.02. Local keys retain direct routing; keyless workers use the hosted endpoint. All validation passed. The implementation is complete and recorded in the commit containing this plan; no Git push or mj release was performed. The public service is deployed independently. No user setup remains.

## Context and Orientation

`mj-core/src/activity/verdict.rs` defines TurnEvidence, byte/list caps, questions, and typed response parsing. `mj-worker/src/acp/verdict_client.rs` currently resolves a local key and calls TypeSafe directly; both running and replied classification paths use it. The daemon already forwards local TypeSafe keys to remote workers. The new proxy handles server-side authentication only; the public client sends no credential. A question JSON resource next to the core Rust module is imported by both implementations to prevent question drift.

## Plan of Work

First extract the existing question object into `mj-core/src/activity/verdict_questions.json`. Build an ES-module Worker using the native fetch API with no routing framework. POST `/v1/turn-verdict` receives the existing TurnEvidence object. Reject unsupported methods, routes, content types, extra fields and arbitrary model/question/URL parameters. Validate text in UTF-8 bytes against Rust caps, cap raw request bodies at 64 KiB, and keep durations/counts nonnegative integers. Rate limit using CF-Connecting-IP, never a caller-chosen identity. Local tests supply the header explicitly. Missing production IP or key fails closed.

The upstream URL and model are fixed; inject TYPESAFE_API_KEY from the Cloudflare secret binding. Disable redirects, enforce an eight-second deadline including response reading, and cap the response at 64 KiB. Return only validated typed answers and sanitized error codes. Do not log evidence, keys, response bodies, or IPs. Request/response caching is disabled. Expose no browser CORS allowance because mj's worker is a native client.

Then install and inspect Wrangler authentication, run tests/type checks/dry-run deployment, provision the secret without printing it, deploy mj-jev-proxy, and verify the assigned workers.dev endpoint using synthetic evidence. Discover the actual hostname rather than committing a guessed address. Keep operational steps and rollback/key-rotation instructions in `.agents/docs/jev-proxy.md`; user-facing routing and data handling belong in the sessions documentation.

Finally extend the Rust client with an explicit hosted source alongside the direct source. Retain existing blank direct sources as disabled test overrides so existing tests never call production. Local keys choose the direct route; missing keys choose the verified hosted URL. Hosted requests contain only TurnEvidence and no Authorization header. Both paths retain the same timeout, body bounds, parsing, scheduling, cancellation, and failure behavior. Add npm CI checks without automatic deployment.

## Concrete Steps

From `services/jev-proxy`, run `npm install`, `npm test`, `npm run check`, and `npm run deploy:dry-run`. Wrangler and dependency versions are pinned by package-lock.json. Run the actual deployment only after local validation. Keep `.dev.vars`, `.wrangler`, `node_modules`, and build outputs ignored.

From the repository root, run `env -u NO_COLOR MJ_CONFIG_DIR=/tmp/mj-proxy-validation-config MJ_DATA_DIR=/tmp/mj-proxy-validation-data cargo test` outside the sandbox. Run `cargo clippy --all-targets -- -D warnings`, `cargo build -p brokk-mj-worker --bin mj-worker --target x86_64-unknown-linux-musl`, `cargo fmt --all -- --check`, and `git diff --check`. Build storage remains in the normal target directory; logs may go under `/mnt/optane`. Stage only implementation files and commit to the current branch without pushing.

## Validation and Acceptance

Tests prove successful evidence forwarding with fixed model/questions and secret authentication; valid Unicode limits; rejection of malformed, oversized, or arbitrary payloads before any upstream call; throttling; missing configuration; timeout; redirects; upstream failures; oversized/malformed answers; and sanitized error responses. Hand-written fakes count upstream calls and inspect the actual forwarded request. Rust loopback tests prove local key precedence, hosted routing without authentication, and existing typed parsing and fail-closed behavior. The deployed synthetic request must return a valid typed verdict before the endpoint becomes the client default.

## Idempotence and Recovery

No live mj store is upgraded or real conversation content used in verification. Wrangler deployments can be repeated; preserve existing unrelated Workers and secrets. A deployment failure does not justify embedding an unverified endpoint. If authentication is unavailable, finish all independent code and local checks, then report the exact deployment blocker. Rotate the provider key via Wrangler secret update and roll back Worker code through Wrangler deployment history. Existing mj behavior remains available whenever classification fails.

## Artifacts and Notes

Wrangler 4.135.0 deployed an 8.06 KiB bundle with the 120 requests/60 seconds binding. The TypeSafe key was supplied through stdin to `wrangler secret put`, never a command argument or repository file. All ten Node behavior tests passed, as did TypeScript checking and the deployment dry-run. The full Rust suite passed with 4081 tests and 25 ignored (excluding two nested child-test repetitions). Clippy with warnings denied and the x86_64-unknown-linux-musl worker build passed. Cargo package --list confirms the shared JSON is published. Formatting and diff checks passed. A live synthetic curl request without a User-Agent returned HTTP 200 and Cache-Control: no-store.

## Interfaces and Dependencies

The public request is a TurnEvidence object at POST `/v1/turn-verdict`. The successful response retains the TypeSafe `answers.waiting_on` and `answers.asked_question` objects consumed by TurnVerdict::parse. Error statuses are 400/413/415 for invalid requests, 429 for rate limiting, 503 for unavailable configuration, 502 for upstream failures or invalid responses, and 504 for deadline expiry. All error bodies contain only a stable error code. Use local npm development dependencies for Wrangler, TypeScript, Worker type definitions, and the TypeScript test loader.

Revision note (2026-09-19): Created before implementation from the user-approved public-proxy plan.

Revision note (2026-09-19): Recorded deployment, synthetic verification, shared-resource packaging, and completed implementation; Rust validation is in progress.

Revision note (2026-09-19): Recorded passing full validation and verified that the extracted questions exactly match the previous Rust question object.
