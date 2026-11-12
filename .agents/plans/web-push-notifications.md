# Deliver phone notifications from the web viewer

This living ExecPlan follows `.agents/PLANS.md`. Work belongs exclusively in `/home/jonathan/Projects/mjolnir-wt/web-push`, initially detached at `0543d2536`. Commit coherent checkpoints on that detached HEAD; never push or edit the main clone.

## Purpose / Big Picture

A user installs the viewer on Android Chrome or an iOS 16.4+ Home Screen and enables device notifications in Settings. When the operator is away, a session asking for input or offering an important reply produces an encrypted system notification. Tapping opens that session. Opening or focusing the viewer removes notifications for resolved attention.

## Progress

- [x] (2026-11-12) Read the planning standard and inspected attention publication, presence, browser routing and the existing service worker.
- [x] (2026-11-12) Identified a required parent decision about crash-time delivery guarantees.
- [x] (2026-11-12) Parent approved at-least-once delivery, durable acceptance receipts and stable tags.
- [x] (2026-11-12) Implemented durable subscriptions, episode sequences, summary coverage, acceptance receipts and private VAPID keys.
- [x] (2026-11-12) Implemented supervised concurrent sending, presence summaries, authenticated subscription actions and device failure status.
- [x] (2026-11-12) Implemented Settings and service-worker handlers; 3 affected Playwright cases and 38 existing browser boundary tests pass.
- [x] (2026-11-12) Focused Rust tests pass (2); affected Playwright tests pass (3); existing browser boundary tests pass (38).
- [x] (2026-11-12) Touched-crate clippy with warnings denied and workspace rustfmt check pass.
- [x] (2026-11-12) Browser checkpoint committed as 980376940.
- [x] (2026-11-12) Daemon checkpoint committed as c146a8a89; first independent review completed.
- [x] (2026-11-12) Fixed all review findings and reran affected checks: 2 Rust, 3 Playwright, 38 Node; clippy and formatting pass.
- [x] (2026-11-12) Send validated implementation through the required review workflow; final verdict is recorded in the session handback.

## Surprises & Discoveries

The requirement of exactly one remote push with neither repeats nor drops across arbitrary crashes cannot be implemented by a local database receipt alone. A push service may accept the HTTP request immediately before the daemon dies, before the receipt commits. After restart the database cannot distinguish acceptance from a request that never reached the service. Notification tags replace currently shown notifications, but do not prevent a second alert after the first was dismissed. The parent explicitly approved this at-least-once residual duplicate window before implementation.

## Decision Log

Approved by parent (2026-11-12): use durable delivery intent and retry after uncertain transport outcomes, accepting possible repeated remote sends. Persist acceptance receipts so acknowledged sends never repeat after restart. Stable session notification tags replace visible cards. The residual duplicate window is remote acceptance before the local acceptance receipt commits. Retrying after that crash can alert again even if the original card was dismissed; this is explicitly accepted. The sender code states this window.

Implemented: the database writer allocates a monotonically increasing sequence per session. The episode identifier is session id plus this sequence. Current attention category and source identities are stored with each active episode. Sources include completed outcome ordinals, pending form ids/messages from the session and managed descendants, and native question owner/generation identities. Repeated observations and removal of answered forms retain the episode; category changes, newly added sources and re-entry after observed attention clearance allocate a sequence. This covers forms and promoted questions without depending solely on an assessment ordinal. Attention eligibility still comes exclusively from the daemon projection.

Implemented: reuse existing ring for P-256 agreement, HKDF-SHA256, AES-GCM and ES256 VAPID signatures, and existing reqwest for HTTPS. No new production or test dependency was needed. Published RFC 8291 Appendix A ciphertext and intermediate values validate encryption; ring's separate signature verifier validates JWTs. Key publication uses a private temporary file, no-clobber atomic publication and directory fsync.

Implemented: provisional migration 127 is compatible because it adds standalone tables older snapshot writers do not touch. No compatibility floor change. Assign its final revision at merge. Push intents, summary coverage and acceptance receipts commit on the existing single database writer. Covered rows are distinct from service-accepted receipts; an unaccepted summary can be regrouped when presence turns away again.

Implemented: each device/tag has one in-flight send; other sessions and devices overlap. Unknown transport outcomes use unbounded exponential backoff, responses 429/5xx use Retry-After when provided, 404/410 removes matching enrollment and cascades its outbox. Permanent service rejection stays visible in Settings until explicit re-enrollment. TTL is one day so an intermittently offline phone can receive queued cards. VAPID validity is the RFC maximum of 24 hours; its contact URL is the project repository.

## Outcomes & Retrospective

Implementation and validation are complete. The final focused Rust suite passed (2 tests), including durable acceptance/unknown outcomes, away-only delivery, summary coverage, expiry, recovery identity, coalesced presence changes, form changes and device retry deadlines. Browser checks passed (3 Playwright, 38 existing Node boundary tests). Touched-crate clippy and rustfmt pass. Mechanical fixes included the shared database writer API, nested-condition lint and one unused mutable test binding. No live phone delivery has been attempted. The browser checkpoint is 980376940 and the daemon checkpoint is c146a8a89. Both independent first reviewers identified summary coverage/withdrawal defects; all findings were accepted and fixed. Review regressions pass, and the follow-up commit is sent through the required review workflow. The final review verdict is recorded in the session handback.

## Context and Orientation

`mj-controller/src/daemon/attention.rs::update` publishes the sole classification through runtime metadata. `mj-controller/src/daemon/feed.rs` calls it and publishes `operator_presence`; `mj-core/src/state.rs::OperatorPresence` has `away` and `revision`. Consume this owner-produced state in one serialized dispatcher, with network requests in supervised background tasks. The controller owns lifecycle and database work, while workers own agent turns. Delivery intent must survive daemon replacement.

`mj-controller/src/web/service-worker.js` currently caches the shell and excludes API requests. Its cache version is v26 and must advance with asset changes. `viewer.js` routes conversation URLs as `/#conversation/<session-id>`. `viewer.html` and `viewer.css` contain the Settings surface. Authenticated mutations use `POST /api/actions`, validated in `mj-controller/src/server/validation.rs` and dispatched in `server_runtime/actions.rs`.

## Plan of Work

The crash guarantee is resolved as above. Store migration helpers have been inspected; the implementation adds a new migration whose final revision is assigned at merge, after the other lane's 126. Classify compatibility explicitly: new standalone subscription, episode and delivery tables are intended to be compatible because old code neither reads nor overwrites them; verify migration revision gating before claiming compatibility. Use a unique per-device/per-episode delivery key and atomic summary reservation so turning away summarizes already-open unsent sessions instead of sending a burst.

Generate a VAPID signing key once in the daemon data directory using exclusive creation and restrictive permissions. VAPID is the public-key signature proving the sender's identity to a push service. RFC 8291 encryption protects the notification body using subscription keys; aes128gcm is the required encrypted content encoding. Inspect workspace dependencies before selecting a maintained encryption crate, and validate it independently. Reuse reqwest for outbound HTTP; it is already in the controller. No dependency decision has yet been made.

Send only display title, first-line body, session identifier and conversation URL. Input uses the question/form line or 'Needs your input'; important replies use the reply line or 'Has a reply for you'. Keep eligibility in one policy expression containing NeedsInput and Important. Remove expired subscriptions on 404/410, persist service-directed Retry-After for 429/5xx, and report background errors. No arbitrary retry count or concurrency cap.

Expose the public key and device state through the viewer API and add subscribe, unsubscribe and test actions. Settings requests permission directly from a tap and shows current state and the iOS Home Screen/HTTPS hint. The worker displays every push and focuses or opens a same-origin conversation on click. On viewer focus and refreshed attention state, close shown notifications whose session is no longer eligible using registration.getNotifications; never send silent withdrawal pushes.

## Concrete Steps

All commands run from the detached worktree. Read existing migration and action patterns before edits. Use an isolated `MJ_INSTANCE=web-push-test` for every new binary/browser invocation and retain automated suites' isolated data/config paths. Run cargo tests with elevated permissions, without redirecting target output or changing mbx configuration.

Run focused tests with `cargo test -p mj-controller <affected-filter>` and store tests with their actual package/filter after locating the store owner. Run `cargo clippy -p mj-controller --all-targets -- -D warnings`, adding every other touched crate, and `cargo fmt --all -- --check`. Locate and extend the current Playwright Settings/service-worker spec, run its existing runner with mocked routes and isolated configuration. Expected result is all selected tests passing; capture actual names/results in this plan. Commit only explicitly named changed files; request agent review before handback.

## Validation and Acceptance

Encryption must decrypt using an independent implementation or the published RFC vector, and the VAPID JWT signature must verify independently. Durable integration tests simulate restart, present suppression, away summaries, resolved episodes and 410 subscription removal. Include the approved ambiguous-send behavior in crash tests. Browser tests exercise permission from the Settings tap and click routing. No live phone result can be claimed here.

On a phone, visit the trusted HTTPS viewer; on iOS add it to the Home Screen first. Enable notifications, send a test, and verify a system notification opens the viewer. Mark the operator away and trigger a question and an important reply; verify titles, bodies and session routing. Answer elsewhere, refocus the viewer and verify the old card disappears. Turn away with multiple existing questions and verify one summary. Restart the daemon after acknowledged delivery and verify no duplicate.

## Idempotence and Recovery

Database uniqueness makes replaying acknowledged episode work safe. Subscription expiry removes the device, permitting a new explicit enrollment. Never test against the default instance or live store. Stop test processes before deleting their directories. Do not delete or regenerate a valid VAPID key on restart because subscriptions depend on it. Failed sends remain observable and follow the approved durable retry policy.

## Artifacts and Notes

Initial evidence: `feed.rs:310` invokes attention update; `feed.rs:231` copies operator presence; `viewer.js:273` parses conversation hashes. Evidence is stored under /home/jonathan/Projects/mjolnir/.mj/clones/4f86b847ae014721aac99daaffd3ae50/.mj/agents/0160580e08e248d034b0153fe3f1d13c/: tests.log (2 passed), browser.log (3 passed), node.log (38 passed), clippy.log and format.log (both clean). This is the assigned report directory; repository source changes remain exclusively in the detached worktree.

## Interfaces and Dependencies

The dispatcher must accept owner-produced session attention and presence together, persist device delivery decisions, then dispatch HTTP outside the event loop. Database actions must provide atomic enqueue/receipt/expiry operations. Viewer actions must share the controller's normal authentication and validation. The implementation is in `mj-controller/src/web_push.rs` and `mj-controller/src/database/web_push.rs`; schema SQL is `database/web_push.sql`. Public device actions are PushSubscribe, PushUnsubscribe and PushTest. Authenticated viewer routes are GET /api/push/key and GET /api/push/status?endpoint=... .

Revision note (2026-11-12): Created the implementation plan and recorded the external-send/database crash ambiguity before making a persisted contract choice.

Revision note (2026-11-12): Recorded the approved delivery semantics and implemented persistence, episode sequencing, crypto, API and browser milestones; final tests and review remain.

Revision note (2026-11-12): The sender consumes OperatorPresence.revision as well as away, so a coalesced watch feed cannot miss a present-to-away transition. Unreachable/Unknown/Failed observations pause sends and retain episode identity instead of treating recovery as a new question. The extended persistence test covers both races. On phone focus, withdrawal waits for a fresh daemon snapshot instead of trusting a suspended tab's cached attention.

Revision note (2026-11-12): Service-directed retry deadlines are owned by the device row as well as the rejected intent, so new episodes and regrouped summaries cannot bypass Retry-After. The persistence regression also checks that this deadline survives reconnect and gates a newly queued episode. Browser checkpoint committed as 980376940.

Revision note (2026-11-12): Final affected checks pass; recorded evidence locations and the browser checkpoint. Daemon commit and automated review remain.

Revision note (2026-11-12, review fixes): Retain shown cards for Unknown/Unreachable/Failed attention. Encrypted summaries now carry covered session IDs (no extra conversation data), allowing focus cleanup when every covered session is resolved. The database reconciles each queued summary against its own current eligible coverage, updates its count, and retires empty summaries even when unrelated attention remains. Unavailable coverage becomes a paused individual intent. Extended existing persistence and Playwright cases with the review regressions; rerun affected checks before the follow-up commit.

Revision note (2026-11-12): All review fixes pass the focused Rust suite, affected Playwright suite, existing browser boundary tests, touched-crate clippy and rustfmt. The follow-up commit is ready for the required second review. No findings were overridden.
