# Deliver phone notifications from the web viewer

## Purpose / Big Picture

A user installs the web viewer on Android Chrome or an iOS 16.4+ Home Screen, enables notifications in Settings, and receives encrypted system notifications when a session needs them while they are away. Each card opens its conversation without reloading an existing viewer or losing drafts. Refocusing the viewer removes cards for resolved attention, retaining cards whose worker is temporarily unavailable.

Work belongs exclusively in `/home/jonathan/Projects/mjolnir-wt/web-push`, initially detached at `0543d2536`. Commit on detached HEAD, never push or change branches, and leave the main clone's source untouched. The assigned report directory is the exception for text evidence. This living plan follows `.agents/PLANS.md`.

## Progress

- [x] (2026-11-12) Inspected daemon attention, presence, persistence, routing and the service worker; wrote the initial plan before implementation.
- [x] (2026-11-12) Obtained explicit at-least-once delivery approval and implemented subscriptions, encrypted sending, receipts, Settings and click routing.
- [x] (2026-11-12) Committed browser and daemon checkpoints, then fixed withdrawal, rejection, enrollment ownership, draft preservation and native rotation findings.
- [x] (2026-11-12) Parent narrowed the design: remove summaries, their coverage table/API/URLs, and their admission coordination. Turning away sends each open unsent episode individually.
- [x] (2026-11-12) Removed summary machinery and replaced the accumulated summary regression with focused persistence/restart/enrollment coverage.
- [x] (2026-11-12) Push actions now acknowledge completion; server unsubscribe precedes native removal so an HTTP failure retains the retry endpoint; replacements request a new audible alert.
- [x] (2026-11-12) Validated the cut: 2 Rust tests, 3 Playwright cases, 38 browser tests, touched-crate clippy and rustfmt pass. Commit and hand back for the parent's integrated review; no further worker review request.

## Surprises & Discoveries

Remote acceptance cannot commit atomically with a local SQLite receipt. A crash after service acceptance but before recording it leaves an uncertain send. Tags replace the same card, but cannot prevent another alert after dismissal. The parent explicitly accepted this residual duplicate window; the sending code documents it.

Worker recovery may temporarily publish Unknown, Unreachable or Failed even though a question remains pending. Such observations pause sending and preserve episode identity; they do not prove resolution. Browser-native endpoint rotation also changes encryption ownership, so late responses must be fenced by a durable enrollment identifier.

Five review rounds increasingly hardened the summary mechanism rather than the central individual-notification behavior. The parent directed its removal, avoiding coverage lifetime, count-dependent payloads, opaque lookup URLs and summary/individual coordination. The implementation now uses one individual-episode path both on new attention and on turning away.

## Decision Log

The parent approved at-least-once remote delivery: persist intent before sending, retry unknown outcomes after restart, and retain durable acceptance receipts. Acknowledged episodes never reset. The residual duplicate window is remote acceptance followed by a crash before its local receipt. Stable per-session tags make a rare duplicate replace the same visible card.

The latest parent decision replaces the original summary requirement. There is no summary notification or coverage state. When operator presence becomes away, each open unsent NeedsInput or Important episode queues individually. New episodes while away use exactly the same path. Failed, Unreachable and Unread do not trigger notifications. The sole eligibility expression in `mj-controller/src/web_push.rs::eligible` is the one-line policy to change if more attention categories are later requested.

The database writer allocates a monotonic per-session sequence. An episode is session ID plus sequence. Its stored source signature includes attention category, completed outcome ordinals, pending form IDs/messages from the session and managed descendants, and native question owner/generation. Repeated observations and removal of answered sources preserve identity. New sources, category changes, or re-entry after confirmed clearance allocate a new sequence. Unavailable attention preserves identity and pauses sending.

Each device/episode row has one durable state: queued, accepted or in flight. The database writer claims sends atomically and admits one in-flight request per device/tag. Other devices and sessions proceed independently. Startup recovers uncertain flights; accepted receipts remain. Device rows have opaque enrollment IDs captured by admitted sends. Expiry-only metadata refresh preserves ownership; changed keys release old flights and replace the owner in the same transaction. Old completions cannot affect a replacement, including ABA key changes or endpoint deletion/recreation. Endpoint renewal copies receipts and uncertain work atomically.

Existing ring supplies P-256 agreement, HKDF-SHA256, AES-GCM and ES256 VAPID signing; existing reqwest supplies outbound HTTPS. No dependency or workspace crate was added. RFC 8291 Appendix A values validate encryption, and a separate ring verification operation checks JWT signatures. The daemon stores one private PKCS8 key in its data directory using restrictive temporary-file permissions, no-clobber publication and directory fsync; it never replaces a valid existing key.

Provisional migration 127 is compatible: new standalone push tables are neither read nor rewritten by older snapshot writers, and existing JSON, constraints and minimum read/write compatibility remain unchanged. Assign the final revision at merge because another lane has revision 126 in flight. The unshipped migration was simplified in place: device subscriptions/backpressure, presence observation, current episodes, per-session sequence counters and deliveries remain; coverage and summary-only columns are gone.

Transport uncertainty uses exponential backoff with no retry cap. Responses 429 and 5xx honor Retry-After and persist an endpoint deadline that new episodes cannot bypass. Responses 404/410 delete only the matching enrollment. Permanent payload rejection retires only that intent and remains visible in Settings. TTL is one day for an intermittently offline phone; VAPID uses the RFC maximum validity of 24 hours. Payloads contain only session display title, trimmed first-line question/form/reply body, session ID, URL and tag. No silent withdrawal pushes are sent.

The parent instructed this worker to stop requesting reviews and hand back the cut for integrated review. The current review finding about expiry with no native replacement remains deferred: `pushsubscriptionchange` renews a current subscription but does not recreate an absent one while permission remains granted. Distinguishing expiry from concurrent explicit Disable needs a parent design decision. The finding was recorded as declined for this worker's scope, not claimed fixed.

## Context and Orientation

The daemon is the background controller process. Its existing `daemon/attention.rs` projection supplies the sole person-facing attention classification, published with `OperatorPresence` in runtime metadata. `mj-controller/src/web_push.rs` consumes that publication, constructs signatures and notification text, encrypts/signs payloads and supervises independent HTTPS tasks. `daemon/serve.rs` runs it beside the HTTP viewer service. Every accepted send has durable intent, so daemon replacement can cancel and resume it without waiting for worker turns.

`mj-controller/src/database/web_push.rs` sends mutations through the existing database writer. Its schema is `database/web_push.sql`, registered in `database/schema.rs` and `database.rs`. Devices use browser subscription endpoints as their keys. `READY_PUSH` is the shared admission/scheduling predicate, consuming persisted presence and current episode eligibility. No in-memory busy list duplicates its decision.

`server/actions.rs`, `server/validation.rs` and `server_runtime/actions.rs` expose Subscribe, Renew, Unsubscribe and Test through the cookie-authenticated action API. `server_runtime/run.rs` waits for push mutations to finish before replying, so Settings status refresh and service-worker renewal observe the committed result. `server/settings.rs` and `server/routes.rs` serve only the public VAPID key and device status; private key material is never served.

`web/viewer.html` and `web/viewer.js` implement device controls and focus cleanup. `web/service-worker.js` displays each push and messages/focuses an existing viewer, or opens the conversation URL. The viewer validates the message and uses normal hash routing to preserve drafts. Its cache version is v32; reassign upward at merge if another lane also changes it. Conversation URLs are `/#conversation/<session-id>`.

## Plan of Work

The persistence milestone creates compatible standalone tables and demonstrates reservations, receipts, presence, expiry, enrollment replacement and process-equivalent restart against real SQLite files. The sender milestone consumes daemon attention and presence, claims sends in the database and performs network work outside the writer and control loop. The browser milestone requests permission directly from a tap, enrolls through authenticated actions, reports device errors, shows a test notification and routes clicks without losing drafts.

These milestones are implemented. The final milestone removes the summary mechanism at the parent's direction, validates only affected paths, commits on detached HEAD and hands back without another worker review request. The parent owns integrated review and final acceptance. Do not introduce a browser lifecycle actor or another durable mechanism merely to complete more review rounds in this worker.

## Concrete Steps

Run from `/home/jonathan/Projects/mjolnir-wt/web-push` using ordinary mbx/Cargo storage. Every Cargo test must be elevated and use a named instance:

    MJ_INSTANCE=web-push-test cargo test -p brokk-mj-controller web_push --lib
    cargo clippy -p brokk-mj-controller --all-targets -- -D warnings
    cargo fmt --all -- --check
    MJ_INSTANCE=web-push-browser tests/e2e/web/node_modules/.bin/playwright test -c tests/e2e/web/playwright.config.js --project=deterministic settings.spec.js
    node tests/e2e/web/viewer.unit.test.mjs

Expect 2 focused Rust tests, 3 Playwright cases and 38 browser boundary tests passing, and no lint or format errors. Browser dependencies use the existing pinned `npm ci --prefix tests/e2e/web` installation. Never redirect Rust build output, install a test binary, or run against the default instance. Stage explicit changed paths and commit on detached HEAD; do not push or request another review.

## Validation and Acceptance

The encryption/JWT test checks RFC 8291 Appendix A derivation/ciphertext, JWT audience/expiry and signature. The persistence regression exercises present suppression, individual sends on turning away, acknowledged versus uncertain restart, unavailable workers, promoted child forms/source removal/re-entry, Retry-After backpressure, permanent rejection followed by test delivery, enrollment metadata refresh, changed keys/ABA, native endpoint rotation and 410 removal. This is real store/data-access integration and tests tricky partial-failure ordering.

The existing Settings Playwright spec uses mocked routes and native browser APIs to verify permission from a tap, enrollment/test/unsubscribe, current device state, retention of temporarily unavailable question cards and withdrawal of resolved individual cards. The service-worker case verifies displayed notifications, renewal actions and click routing without full document navigation. The Settings golden fits a 390x844 viewport without sideways overflow.

Real phone delivery remains untested. Open a trusted HTTPS viewer; for iOS 16.4+, add it to the Home Screen and open the installed app. Sign in, enable notifications and send a test. While operator presence is away, trigger a question and an important reply; verify title/body, an audible alert for later episodes under the same tag, and conversation routing that preserves drafts. Turn away with multiple existing questions and verify individual session cards. Answer elsewhere, refocus the viewer and check withdrawal. Disconnect/reconnect a worker and verify its still-open card survives. Restart the daemon after acknowledged delivery and check no replay. Keep Tailscale connected for tailnet-only URLs. If native subscription expiration leaves no replacement, reopening Settings and enabling again is currently required; integrated review must resolve that limitation before claiming automatic expiry renewal.

## Idempotence and Recovery

Unique device/episode reservations and acceptance receipts make ordinary replay safe. Only sender startup recovers in-flight rows. Unknown remote acceptance may repeat as explicitly approved; stable tags replace the same card. Resolved queued intents retire before sending. Browser renewal uses existing cookie authentication; expired sign-in must be restored through the viewer. Functional-event errors and background failures are reported with context. Server unsubscribe finishes before native removal, retaining the endpoint for retry on HTTP/authentication failure.

## Outcomes & Retrospective

The pre-cut primary implementation (`web_push.rs`, `database/web_push.rs`, `database/web_push.sql`) was 1739 lines. After the cut it is 1108 lines, a reduction of 631 (36%). All affected checks pass; no tests failed. Rust build/test finished in 8m02s plus 0.24s execution; clippy finished in 4m50s, Playwright in 2.7s, and the 38 browser tests in 75ms. The cut removes summary coverage, lookup and coordination rather than continuing to patch a mechanism the parent rejected. The remaining background-expiry review finding is explicitly deferred to integrated review.

## Artifacts and Notes

Text evidence belongs in `/home/jonathan/Projects/mjolnir/.mj/clones/4f86b847ae014721aac99daaffd3ae50/.mj/agents/0160580e08e248d034b0153fe3f1d13c/`: `tests.log`, `clippy.log`, `format.log`, `browser.log` and `node.log`. The handback records commits, line counts, known limitations and phone checks. No build output belongs there.

## Interfaces and Dependencies

Viewer actions are `PushSubscribe`, `PushRenew`, `PushUnsubscribe` and `PushTest`; GET routes are `/api/push/key` and `/api/push/status?endpoint=...`. Dispatcher database calls are `push_observe`, `push_claim`, `push_result` and startup-only `push_recover`. No summary route or wire contract remains. Existing ring/reqwest provide cryptography/HTTPS with no new dependency.

Revision note (2026-11-12): Rewrote the plan around the parent's explicit summary cut and integrated-review handoff. Previous summary fixes remain in commit history but are superseded by the final individual-episode design. Retained standalone enrollment, receipt, click-routing and renewal work; three small current-review fixes were completed without adding a new lifecycle mechanism.
