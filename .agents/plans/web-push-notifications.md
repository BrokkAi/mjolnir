# Deliver phone notifications from the web viewer

This living ExecPlan follows `.agents/PLANS.md`. Work belongs exclusively in `/home/jonathan/Projects/mjolnir-wt/web-push`, initially detached at `0543d2536`. Commit on detached HEAD, never push, and leave the main clone's source untouched. Evidence goes in the assigned agent report directory.

## Purpose / Big Picture

A user installs the viewer on Android Chrome or an iOS 16.4+ Home Screen and enables notifications in Settings. Away sessions asking for input or offering an important reply produce encrypted system notifications without a native app. Individual cards open their conversation. Returning to the viewer withdraws resolved cards and summaries while retaining cards whose attention is temporarily unavailable.

## Progress

- [x] (2026-11-12) Inspected attention publication, presence, persistence, routing and the service worker; wrote this plan before implementation.
- [x] (2026-11-12) Obtained the parent's explicit at-least-once delivery decision.
- [x] (2026-11-12) Implemented durable subscriptions, episodes, summary coverage, admission, receipts and private VAPID keys.
- [x] (2026-11-12) Implemented authenticated actions, device status and coverage APIs, Settings controls and service-worker handlers.
- [x] (2026-11-12) Committed browser checkpoint 980376940 and daemon checkpoint c146a8a89.
- [x] (2026-11-12) Addressed first-review coverage and withdrawal defects in 4c4eefc4c.
- [x] (2026-11-12) Addressed second-review device rejection and in-flight ownership defects, including the underlying summary payload growth.
- [x] (2026-11-12) Final affected checks pass: 2 Rust, 3 Playwright, 38 browser boundary tests, touched-crate clippy and rustfmt.
- [x] (2026-11-12) Send validated implementation through required review; the final verdict is recorded in the session handback.

## Surprises & Discoveries

Remote push acceptance cannot commit atomically with a local SQLite receipt. If the daemon dies after acceptance but before the receipt, restart cannot distinguish that from a request that never reached the service. Tags replace visible cards, but cannot prevent another alert after dismissal. The parent explicitly accepted this residual duplicate window before implementation; the sender comments state it.

The runtime watch can coalesce present-to-away transitions, so the sender must use the owner's presence revision, not only its boolean. Worker recovery can temporarily expose unavailable attention; that must pause an existing episode rather than manufacture a new question.

Review found that summary membership must be reconciled independently of unrelated attention, and that per-tag task bookkeeping alone cannot coordinate summaries with individual requests already in flight. Durable admission now belongs to the same database writer as summary coverage. Review also exposed count-dependent payload growth from putting coverage lists on the wire. Coverage now stays in an independent database table, with a fixed-size opaque URL reference on the wire.

## Decision Log

The parent approved durable intent before send, retries for unknown outcomes, durable acceptance receipts and stable tags. This is at-least-once remote delivery. The residual duplicate window described above is intentional; acknowledged sends never reset on restart.

The database writer allocates a monotonic per-session sequence. Episode identity is session ID plus sequence. Stored source signatures include attention category, completed outcome ordinals, pending form IDs/messages from the session and managed descendants, and native question owner/generation identities. Repeated observations and removal of answered forms retain the episode. New sources, category changes and re-entry after confirmed clearance allocate another sequence. Unknown, Unreachable and Failed observations retain identity and pause sends. Eligibility is one policy expression containing NeedsInput and Important; attention classification itself remains daemon-owned.

One outbox state owns each delivery: queued, accepted, summary-covered or in flight. The database writer atomically claims eligible sends and enforces one in-flight request per device/tag. Summary regrouping excludes in-flight individuals and episodes covered by an in-flight summary. Completion updates only the admitted row, so a late failure cannot uncover a summary-covered episode. Startup resets uncertain in-flight rows to queued and preserves acceptance receipts. The sender has no separate busy flag or active set.

Reuse existing ring for P-256 agreement, HKDF-SHA256, AES-GCM and ES256 VAPID signing, and existing reqwest for HTTPS. No production or test dependency was added. Published RFC 8291 Appendix A values validate encryption, and ring's separate signature verifier validates the JWT. VAPID keys are private PKCS8 files, published with restrictive temporary-file permissions, atomic no-clobber publication and directory fsync. Never regenerate a valid key on restart.

Provisional migration 127 is compatible: standalone push tables are neither read nor rewritten by older snapshot writers, and no existing JSON or constraints change. The compatibility floor remains unchanged. Assign its final revision at merge. Tables own device subscriptions/backpressure, presence observation, current episodes, sequence counters, outbox deliveries and immutable summary cleanup coverage.

Unknown transport outcomes use exponential backoff without a retry cap. Responses 429 and 5xx honor Retry-After and persist an endpoint-wide deadline that new episodes and re-enrollment cannot bypass. Responses 404/410 delete only the matching enrollment. Permanent payload rejection stops only that intent and stays visible in Settings; it cannot disable the device. TTL is one day for an intermittently offline phone. VAPID validity uses the RFC maximum of 24 hours, with the project repository as contact URL.

Push payloads contain only title, first-line body, session ID, URL and notification tag. Summary payload size is independent of coverage count: its URL contains an opaque hash reference. Authenticated coverage lookup returns only the original covered session IDs. Coverage survives outbox retirement and subscription expiry, so even a phone that displayed an unacknowledged send can later withdraw it. The browser fetches fresh attention before evaluating withdrawal, retaining Unknown/Unreachable/Failed cards. No silent withdrawal pushes are sent.

## Outcomes & Retrospective

Implementation and affected validation are complete; no real phone delivery has been attempted. Both independent review rounds' findings were accepted and fixed. The latest change makes races unrepresentable by giving the database writer sole ownership of send admission and coverage, rather than narrowing a task-bookkeeping window. It also removes session-list-sized push bodies instead of adding a guessed limit or a fallback.

## Context and Orientation

`mj-controller/src/daemon/attention.rs::update` supplies the sole person-facing classification. `daemon/feed.rs` publishes it with `OperatorPresence` in runtime metadata. `mj-controller/src/web_push.rs` consumes that projection, builds episode source signatures and notification content, encrypts/signs payloads and supervises independent HTTPS tasks. It runs alongside the phone server in `daemon/serve.rs`; the HTTP server retains its own accepted-work shutdown drain.

`mj-controller/src/database/web_push.rs` uses the existing database writer for every mutation. Its SQL schema lives in `database/web_push.sql`; migration registration is in `database/schema.rs` and `database.rs`. Devices are identified by their browser-provided subscription endpoint. Outbox state and receipts are durable, so replacing the daemon does not depend on a worker turn or an in-memory task.

`server/actions.rs`, `server/validation.rs` and `server_runtime/actions.rs` expose Subscribe, Unsubscribe and Test through the normal cookie-authenticated viewer action path. `server/settings.rs` and `server/routes.rs` serve the public VAPID key, device failure status and summary coverage; private key material never reaches the browser.

`web/viewer.html` and `web/viewer.js` implement device controls and focus cleanup. `web/service-worker.js` displays every push and focuses or opens a same-origin target on click. Its cache version is v29 and must be reassigned upward if another lane also changes it. Conversation URLs are `/#conversation/<session-id>`; summary URLs open the viewer and carry the cleanup reference.

## Plan of Work

The persistence milestone creates compatible standalone tables and demonstrates reservations, receipts, presence changes, expiry and recovery against real SQLite files. The sender milestone consumes the existing runtime publication, atomically admits sends, and keeps network requests outside the database writer and control loop. The browser milestone requests permission directly from the tap, enrolls through authenticated actions, displays current state and sends a test. The acceptance milestone extends existing browser cases and verifies crypto against an independent published fixture, then commits and requests review.

These milestones are implemented. Future review fixes must extend existing cases for an actual tricky condition, rerun only affected tests, and finish with touched-crate clippy plus rustfmt. Do not introduce new default-instance tests, speculative caps, branches, pushes or installation steps.

## Concrete Steps

Run from `/home/jonathan/Projects/mjolnir-wt/web-push`, using ordinary mbx/Cargo storage. Every cargo test runs with elevated permissions and `MJ_INSTANCE=web-push-test`:

    MJ_INSTANCE=web-push-test cargo test -p brokk-mj-controller web_push --lib
    cargo clippy -p brokk-mj-controller --all-targets -- -D warnings
    cargo fmt --all -- --check

Expected results are 2 passing focused Rust tests and no lint/format errors. Run affected browser checks with the pinned existing dependencies:

    MJ_INSTANCE=web-push-browser tests/e2e/web/node_modules/.bin/playwright test -c tests/e2e/web/playwright.config.js --project=deterministic settings.spec.js
    node --test tests/e2e/web/viewer.unit.test.mjs

Expected results are 3 Playwright cases and 38 existing browser boundary tests passing. Do not update goldens unless user-visible text actually changed. Stage only changed files, commit on detached HEAD, and request review with the assigned worktree path.

## Validation and Acceptance

The encryption/JWT test checks RFC 8291 Appendix A ciphertext and derivation values, JWT audience/expiry and signature verification. The database regression covers acknowledged and unknown outcomes across reconnect, present suppression, away summaries, form removal/re-entry, worker recovery, coalesced presence revisions, device deadlines, expiry, summary shrinking/retirement, permanent rejection followed by test/new sends, in-flight individual/summary ownership, stale completions and restart admission. A real 1,000-session summary proves the stored coverage exceeds 4096 bytes while its actual push plaintext stays within the documented RFC 8291 section 4 allowance of 3993 bytes; lookup survives endpoint expiry.

The browser regression drives the real Settings UI with mocked routes and browser push APIs. It checks permission only from a tap, test/unsubscribe actions, temporary-unavailability retention, resolved individual/summary cleanup through the coverage API, and service-worker click routing. The existing Settings golden still fits a 390x844 viewport without sideways overflow.

On a phone, open a trusted HTTPS viewer. On iOS 16.4+, add it to the Home Screen and open that app first. Enable notifications and send a test. With the operator away, trigger a question and an important reply; check session titles, first-line bodies and conversation routing. Turn away with multiple existing questions and verify one summary. Answer elsewhere and refocus the viewer; both individual and resolved summary cards must disappear. Temporarily disconnect/reconnect a worker and verify its open question card survives. Restart the daemon after acknowledged delivery and verify no duplicate. Keep Tailscale connected when opening a tailnet-only viewer URL.

## Idempotence and Recovery

Unique device/episode reservations and acceptance receipts make ordinary replay safe. Only startup recovers in-flight rows. An unknown remote acceptance may repeat as explicitly approved; tags replace the same card. Resolved queued intents retire before sending. Summary cleanup coverage has an independent durable lifetime because displayed OS notifications can outlive subscriptions and outbox work.

Never test against the default instance or live store, never redirect build output, and stop owning processes before deleting test directories. No daemon/browser test build was installed or used against live session data. Subscription expiry allows explicit re-enrollment, while the retained coverage still supports cleanup of already shown cards.

## Artifacts and Notes

Evidence is in `/home/jonathan/Projects/mjolnir/.mj/clones/4f86b847ae014721aac99daaffd3ae50/.mj/agents/0160580e08e248d034b0153fe3f1d13c/`: `tests.log`, `clippy.log`, `format.log`, `browser.log` and `node.log`. The session handback records all commits and the final review verdict. This is the assigned report directory; source changes remain exclusively in the detached worktree.

## Interfaces and Dependencies

The public viewer action variants are `PushSubscribe`, `PushUnsubscribe` and `PushTest`. Viewer-only GET routes are `/api/push/key`, `/api/push/status?endpoint=...` and `/api/push/summary?id=...`. The dispatcher calls `push_observe`, `push_claim`, `push_result` and startup-only `push_recover`; these all use the single database writer. `READY_PUSH` is shared by admission and scheduling. All background failures propagate or are reported with context, and subscription capability URLs are removed from transport diagnostics.

Revision note (2026-11-12): Consolidated the final design and evidence after two review rounds, replacing interim mechanisms with durable admission and independent immutable summary coverage. The follow-up commit is ready for required review.
