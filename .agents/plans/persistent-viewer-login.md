# Keep phone login usable across restarts

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Issue #1030 asks for a bookmarkable QR login URL that survives daemon restarts, renewal of cookies while a phone is active, and useful rejection diagnostics. A bookmarked login URL must mint a new cookie after browser storage eviction. Changing the persisted signing key must revoke both cookies and login URLs. The six-digit code remains temporary.

## Progress

- [x] (2026-09-21) Read authentication, startup, tests, and security documentation; confirmed the login token is generated separately per process.
- [x] (2026-09-21) Derived the login token from the persisted key; HTTP tests prove restart/rotation behavior.
- [x] (2026-09-21) Shared cookie validation, renewal, and rejection reasons between viewer and versioned API authentication.
- [x] (2026-09-21) Documented credential lifetime and revocation; all 166 focused server tests passed.
- [x] (2026-09-21) Full dev-profile tests, Clippy, formatting, and diff checks passed; reviewed credential derivation, identity preservation, and both authentication paths.
- [x] (2026-09-21) Opened PR #1116 and reproduced two review findings: renewal after logout and promotion of desktop cookies to persistent credentials.
- [x] (2026-09-21) Fixed both findings with durable viewer revocation and explicit phone-cookie renewal eligibility; all 173 focused server tests and Clippy passed.
- [ ] Confirm CI on the review fixes. At the user's request, push without waiting for the full local suite and leave remote validation to CI.

## Surprises & Discoveries

`ServerOptions::set_cookie_key` already runs before startup publishes the QR URL. Both `/api` and `/api/v1` accept viewer cookies, but currently validate independently. A zero configured lifetime means a browser-session cookie with a bounded server expiry; renewal must preserve omission of Max-Age. Viewer identity is part of the signed cookie and must remain unchanged so renewal does not lose drafts.

## Decision Log

On 2026-09-21, choose HMAC-SHA256 with a fixed, separate login-token purpose string and the existing persisted cookie key. HMAC is the existing keyed signature implementation; using a separate purpose prevents treating a cookie signature as a login token. This avoids another credential file and couples revocation to the existing key rotation. Anyone holding the URL retains access until key rotation; document this explicitly and discuss it at the PR checkpoint.

Renew accepted cookies using the server's configured lifetime and the same viewer identity. Keep the existing cookie format and old-cookie acceptance. Authentication middleware (the checks that run before protected HTTP handlers) will attach renewal headers without replacing a handler's own Set-Cookie response. Bearer-token requests do not mint cookies. Diagnostics contain fixed reason labels only, never credential values or URLs.

Review revision on 2026-09-21: renew only newly issued phone cookies carrying a signed viewer prefix (colon is outside the old random viewer-id alphabet). Desktop and legacy cookies remain accepted with their original fixed expiry and are never promoted. Logout must revoke the viewer identity, not only clear browser storage. Keep a memory cache of revoked identities and their maximum possible cookie expiry, persisted atomically beside the signing key as `phone-cookie-revocations.json`. Check the cache at authentication and before renewal. A revocation survives a restart, so even a response delivered out of order cannot restore usable access. Serialize writes in background work without holding the cache lock during disk I/O, report persistence failures, and prune expired entries on load/write. A missing ledger means first use; corrupt or unreadable existing data must fail closed. No SQLite migration is involved.

## Context and Orientation

`mj-controller/src/server.rs` constructs ServerOptions and loads the persisted key. `server/auth.rs` implements signing and cookie parsing. `server/routes.rs` protects viewer APIs; `server/api/routes.rs` protects versioned APIs and also accepts the CLI bearer token. `server/handlers.rs` implements login and logout. `server/tests.rs` has isolated Router tests and temporary key files. `docs/src/content/docs/security.md` describes user-visible authentication.

## Plan of Work

First replace random login-token generation with deterministic derivation from the cookie key, both in construction and set_cookie_key. Add a real Router test that installs a temporary persisted key twice, reuses the original login URL, and rejects it after rotation.

Next centralize cookie validation in auth.rs with explicit absent, malformed, expired, and bad-signature results. Use the same parser for viewer identity. Add a renewal helper used by both authentication layers, with a testable supplied clock for parsing. Verify an old near-expiry cookie returns a later signed expiry with the same viewer and expected HTTP attributes; invalid cookies remain unauthorized with no renewal. Cover browser-session configuration and bearer-only requests.

Finally update the security documentation and review the complete diff for secret exposure, lifetime changes, and response-header handling. No new database schema or dependencies are needed.

For the review fixes, add `server/auth/revocations.rs` for the cached revocation ledger and background persistence. Load the key and ledger together off the async runtime in server startup. Change middleware to authenticate first, then conditionally renew after its handler; logout records revocation before returning success. Prove that a blocked request released after logout cannot renew, that already issued cookies for that viewer fail after logout and after restart, and that another viewer remains signed in. Test a real desktop bootstrap cookie and an old unmarked cookie on both HTTP surfaces, plus persistence failures and expiry pruning.

## Concrete Steps

From `/Users/ryansvihla/code/mjolnir`, run `cargo test -p brokk-mj-controller server::tests` outside the sandbox for focused HTTP behavior, then full `cargo test` and `cargo clippy --all-targets -- -D warnings` in the dev profile. Run `cargo fmt --all -- --check` and `git diff --check`. Store output under `target/issue-1030-*.log`. Commit only changed files, push the issue branch, open a PR referencing #1030, and stop for user review.

## Validation and Acceptance

An original QR URL returns HTTP 303 after restarting with the same key, with Location `/` and a valid cookie; after key rotation it returns 401. Authenticated requests to both API surfaces renew the expiry while preserving viewer identity. Invalid signatures, malformed cookies, absent cookies, and expired cookies cannot gain a new cookie. Ephemeral configuration still omits Max-Age. Existing tests for code lockout, secure attributes, desktop cookie acceptance, and persisted key permissions must pass. Tests use isolated temporary storage and in-memory routers; no live instance is involved.

## Idempotence and Recovery

There is no migration and no live key rotation. Temporary test keys can be discarded. Existing production keys are only read by the existing startup path. Retrying tests is safe. Reverting the change rotates the login URL at restart but leaves existing signed cookies readable.

## Artifacts and Notes

PR #1115 merged as `bf68b34a` on 2026-09-21 after all 18 checks passed. This branch begins from its approved head, so the PR against master contains only the authentication changes.

## Interfaces and Dependencies

Reuse existing Hmac, Sha256, Base64, Axum HeaderMap, and ApiError types. Keep ServerOptions::set_cookie_key and login_token public signatures unchanged. Keep the signed cookie's viewer/expiry/signature representation readable by existing clients.

## Outcomes & Retrospective

Implementation and local validation are complete. The 166 focused server tests passed, including HTTP restart/rotation, renewal with stable identity and browser-session attributes, and rejection without renewal. Full dev-profile tests and Clippy passed; logs are in `target/issue-1030-tests.log` and `target/issue-1030-clippy.log`. Formatting and diff checks passed. The final PR must explain stable URL possession and key rotation before user approval.

Revision 2026-09-21: Created before implementation, including the requested stop at a reviewable PR and explicit credential-lifetime tradeoff.

Revision 2026-09-21: Recorded implementation and passing focused tests; retained full validation and PR discussion as pending.

Revision 2026-09-21: Recorded passing full local validation and pre-PR review. PR #1115 is still awaiting green CI after two distinct failures that passed local focused reproduction.

Revision 2026-09-21: Confirmed #1115 merged with all checks green and verified the authentication-only diff against master before opening this PR at the user's request.

Revision 2026-09-21: Added the user-authorized review fixes. Preserving desktop/legacy expiry requires explicit renewal eligibility; preventing late responses from undoing logout requires durable revocation, including across restarts.

Revision 2026-09-21: Review fixes are implemented and focused validation passed. Regression coverage includes delayed responses, previously renewed cookies, restart persistence, independent viewers, desktop/legacy expiry, persistence failure and retry, concurrent logouts, pruning, and corrupt storage. Clippy and diff checks passed. The full local suite was still running when the user explicitly requested committing and pushing immediately and letting CI finish validation. No merge is authorized for this revision yet.
