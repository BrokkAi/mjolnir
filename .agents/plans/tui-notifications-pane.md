# Add a TUI notifications pane that steps through sessions needing the user

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` in the repository root.


## Purpose / Big Picture

The Notifications pane shows the question or reply that needs attention across workspaces, including requests held by managed children. Go to Session opens the content holder through ordinary navigation with its composer focused; it never targets or loads a historical message. Quick reply is a single-line presentation of the real Prompt editor, with Send and conditional Enter behavior. Drafts and command-bound in-flight feedback belong to their destination session, so card changes cannot retarget a reply. Clear & Next dismisses the captured daemon target, Skip locally defers that exact card, and Clear all ! affects only Important cards.

Text is shared with host notifications and resolved lazily for the displayed card. A buried question uses the one-item read; no chat-history merge or Go To Message mechanism returns. Notifications size, skipped order and reply editors live in the TUI process. The header uses the existing secondary palette color; text has one-space margins and a blank line before Quick reply. Mouse drags select the body through the transcript selection owner, including its highlighting, smart copy and edge scrolling. Selection expires when the displayed card or its row space changes.

## Progress

- [x] (2026-11-17, 066ae3121 review) Replace separate pending fields with one ComposerRequest queue and an exhaustive dispatcher for search, session listings and every ChatDaemonRequest. Attached and detached editors share it and the per-session daemon bridge; queued requests/generation state travel with the live composer.
- [x] (2026-11-17, 066ae3121 review) Separate NotificationSend and NotificationRestore, with distinct hit areas. Ctrl+Enter/Send always send the current field; Alt+Enter and Restore unsent restore only.
- [x] (2026-11-17, 066ae3121 review) One CLI reply-outcome helper reports every failure with session/full text and restores it into an empty ordinary composer after the card disappears. Existing drafts remain intact; notices retain copyable text. Unconfirmed admission is also reported without replay.
- [x] Focused validation passed: 14 queue/control/recovery/history/transfer/registry tests and four affected render goldens (18 unique tests). Final three-crate dev/all-targets clippy passed with warnings denied. Refreshed exactly one golden footer label from Alt+Enter send to Ctrl+Enter send after inspecting the diff; the other three goldens remain unchanged. No commit.

- [x] (2026-11-17, f5b4539b4 review) Retain refused text with its error separately from the draft. Captured Alt+Enter/Send restores recoverable text and preserves displaced drafts; only a successful send removes matching recovery text.
- [x] (2026-11-17, f5b4539b4 review) Show selected search matches beside query/shared status in one row. Render each recalled newline as ⏎ with matching Unicode cursor mapping; keep payload newlines literal.
- [x] (2026-11-17, f5b4539b4 review) Share the existing session-prompt listing dispatcher/results with detached editors and record successful replies through Prompt history. Extend existing notification/history tests.
- [x] Focused validation passed: six existing history tests, four notification state tests and the existing attached-session prompt-list refresh regression (11 unique tests). Final dev/all-targets clippy passed for mj-chat and mj-tui with warnings denied. No commit or golden refresh.

- [x] (2026-11-17) User-requested mouse selection: register the notification body with the shared selection owner, reuse transcript wrapping/copy rules and highlighting, and invalidate its row space on card identity changes. Extend one notification state test.
- [x] (2026-11-17) Accepted Quick reply review: keep focus navigation/editing available during sends; suppress only duplicate in-flight drafts; let the editor consume history-search Enter; share the existing background history request/result owner with destination project binding. Extend the existing reply state test.
- [x] Scoped validation passed: four notification state tests, twenty existing history tests, two existing CLI drag-selection tests and four unchanged notification-bearing goldens (30 unique tests). Dev/all-targets clippy passed for mj-chat, mj-tui and mj-cli with warnings denied; rustfmt/diff checks passed. No commit.

- [x] User request 2026-11-17: add ordinary Go to Session, single-line Quick reply and Send; rename Later to Skip; use modifier bindings, per-destination drafts and captured sends. Reuse the real ChatState Prompt editor and existing durable asynchronous startup-prompt submission/outcomes.
- [x] Ten unique affected tests passed across scoped runs/reruns: notification state/scroll/host (4), registry collision (1), prompt receipts (1), and four render goldens. Final dev/all-targets clippy passed for mj-chat, mj-tui and mj-cli with warnings denied. No commit.

- [x] Restore this round's uncommitted Earlier-messages-reader experiment to HEAD 428c8f9f0 before implementing the user's replacement decision.
- [x] Remove Go To Message, its Enter binding/button/footer, pending navigation state, summary-boundary/locator navigation, stored-range reads, loaded-history merging and omission metadata. Preserve one-item question text reads, legacy count tails and Clear/Later identity.
- [x] Delete the CLI navigation test and retained-history gap/removal extensions; adjust notification state and read-owner tests, retain the one-item transport integration and legacy unchunked count assertion.
- [x] Twenty unique affected tests passed, including four reviewed golden updates. Dev/all-targets clippy passed for all six touched crates with warnings denied. No commit; parent runs boundary suites.

- [x] 11619f2aa issue #2: capture the displayed Important card's ReadThrough boundary and identity in pending navigation; resolve the newest nonempty agent message at or before that boundary, retaining the captured card in the CLI action path.
- [x] Extend the existing CLI navigation regression: summary A, capture Go To, then hydrate newer reply B; navigation must anchor A while a newly opened card targets B.
- [x] Focused CLI navigation passed (one test); dev/all-targets clippy for brokk-mj-tui and brokk-mjolnir passed with warnings denied. No commit; issue #1/history merging remains untouched pending the parent's design decision.

- [x] 64316e6d3 review: add normalized omission ranges to ProjectionWindow at the trim, stored-read and feed-selection owners; carry that authority into ChatState initialization and updates.
- [x] Preserve loaded rows only in explicit omissions (or beyond an older live frontier); absent covered identities are removed. Stale history hydration consults the same authority, including canonical items still awaiting local prefix conversion.
- [x] Extend the existing gap regression with actual projected interim/real terminal tools: covered fallback removal, retained gap message/anchor, and no resurrection from a stale stored read. CLI's bounded fixture now carries its window.
- [x] Focused prefix/gap coverage: nine unchanged prefix tests passed; the extended gap test passed after fixture corrections, then CLI navigation passed (eleven unique tests). Requested dev/all-targets clippy for brokk-mj-chat and brokk-mjolnir passed. No commit.

- [x] Parent compatibility follow-up: retain unchunked StoredProjectionTail for legacy count requests; only protocol-103 start-position requests receive the new chunked StoredProjectionRange. The database range owner is unchanged.
- [x] Extend the daemon/client integration with a pre-103 count action shape and a raw single-frame assertion, retaining the over-8-MiB test on the new range reply.
- [x] Both requested tests passed (integration 1, CLI navigation 1). Dev/all-targets clippy passed for mj-client, mj-controller and mj-cli with warnings denied. No commit.

- [x] 36aecf9ed review: replace the local count estimate with a start-position range, selected through the existing stored-tail read owner and a single daemon database snapshot; Unsupported is terminal for older daemons. Protocol remains 103.
- [x] Enable existing chunked transport for protocol-103 StoredProjectionRange and stored item replies; legacy count-mode StoredProjectionTail remains unchunked. Extend the daemon/client integration test past 8 MiB, including store advancement and older-daemon compatibility.
- [x] Merge loaded history and gapped live windows by stable identity and position, with live entries winning. Extend the existing chat-prefix regression using a gap produced by ProjectionWindow::trim.
- [x] All requested validation passed: daemon/client range and over-8-MiB integration (1), existing chat-prefix filter including the new gap regression (10), CLI navigation (1), read-owner cancellation/generation (1). Dev/all-targets clippy passed for mj-controller, mj-client, mj-chat and mj-cli. No commit.

- [x] 1d2150be0 review: confirmed StoredProjectionTail forwards an arbitrary requested item count into the SQL LIMIT; no new daemon API is needed.
- [x] Keep notification navigation pending until the chat anchors the stable id. Add a stored-history request under the existing read owner, using latest position minus target position plus eight leading context items.
- [x] Convert the returned history off the event loop; hydrate the live chat without replacing its drafts/forms, including same-frontier reads, and retain the loaded prefix across later live tails.
- [x] Extend the existing CLI navigation and read-owner cancellation regressions for old targets, same-frontier hydration, later live updates, read errors and still-absent targets.
- [x] Affected tests passed: CLI notification navigation (1), read-owner generation/cancellation (1), TUI notifications (3), existing chat-prefix regressions (10). Final dev/all-targets clippy passed for mj-chat, mj-tui and mj-cli. No commit.

- [x] Review follow-up on f7b8ed60b: traced the projection path before editing; confirmed live SessionControl snapshots return the bounded managed-session view rather than the unbounded database projection.
- [x] Parent authorized an indexed one-item read; the existing `(session_id, stable_id)` primary key covers it, with no migration.
- [x] Captured card identity at pointer-down and carried it through Clear/Later/Go To actions; keys capture at the key press. Pending question navigation waits for its locator.
- [x] Added the bounded read operation on the daemon database read path and client Unsupported result; integrated it into the existing generation/cancellation owner and shared message text builder.
- [x] Extended state/navigation and daemon/client integration regressions pass: 3 notification tests, 2 unchanged goldens, 1 CLI navigation test, 1 read-owner cancellation test and 1 daemon/client integration test. Dev/all-targets clippy passes for brokk-mj-tui, brokk-mjolnir, brokk-mj-controller and brokk-mj-client.

- [x] Recency follow-up: preserve positive millisecond activity timestamps for sorting; unknown/zero sorts after known activity. Existing state regression extended for sub-second values, None and zero.
- [x] Requested attention-navigation golden, session-action-menu golden, notification tests and TUI clippy passed. Attention golden refreshed only for seven previously incorrect age labels; card identities/order/layout expectations remain unchanged.

- [x] Parent full-suite follow-up: zero activity timestamps normalize to unknown; overlay-safe card-header normalization uses its own glyph/separator structure. No golden refresh.
- [x] Parent full-suite follow-up: loading now follows the existing summary/projection request owners through the existing hydration status; completed empty summaries and absent/cancelled/failed reads show the placeholder. Palette golden plus notification tests: 3 passed. Clippy for TUI and the CLI request-owner wiring passed.

- [x] Review follow-up on 41eb625c2: route promoted approval content/navigation through `waiting_child_of`, retain daemon dismissal identities, and keep the displayed family subscribed.
- [x] Review follow-up: move the existing summary-selection owner into the TUI model, include the displayed content session from any workspace, and render `loading…` before content arrives. Extended notification state tests pass.
- [x] Review follow-up: resolve summary-only Important navigation through the existing pending-position mechanism after full projection; focused CLI test (1 passed) and clippy for brokk-mj-tui/brokk-mjolnir, all targets, passed.

- [x] (2026-11-17) Mapped the existing TUI pieces (map at `.mj/agents/961b8b3a6fdecc7b67fb3627c4be3cac/map.md`; facts restated below).
- [x] (2026-11-17) Read the full plan and checked pane-size storage before implementation; found the explicit stop condition below.
- [x] Parent authorized process-local Notifications size; shared PaneSizes, API and store remain unchanged.
- [x] Milestone 1: a shared owner of the notification text, the entry model and its order, with state tests.
- [x] Milestone 2 (rendering, keys and local sizes implemented; 33 render golden tests pass): the pane: rendering, keys, buttons and actions, with a golden render.
- [x] Milestone 3 (warm/cold stable-id navigation and direct bottom navigation with a pending form pass the focused CLI regression): Go To Message forces the chat position to the message, including for an already open chat, and focuses the composer.
- [x] Isolated live tmux check passed for empty pane, focus, sizing and resizing; test daemon stopped. Nonempty/harness behavior is covered by tests, not this live run.
- [x] Full TUI suite run (592 passed, 2 ignored, 6 failures corrected and targeted rerun passed), CLI dashboard tests (112 + 4 PTY passed), and dev clippy for both crates/all targets passed.
- [x] Parent approved the additive `TranscriptPosition::bottom()` constructor in mj-chat; implemented direct restoration and extended the CLI regression with a pending form. Focused CLI validation and clippy passed.


## Surprises & Discoveries

- Search, session listing and Ctrl+C formerly had separate pending fields/pumps. A shared enum queue now eliminates that host-maintained subset; its dispatcher exhaustively matches both request categories and nested ChatDaemonRequest variants, with no wildcard arm.
- The ordinary notice/recovery path was bypassed by both direct admission failures and durable Quick reply refusals. Both now call one outcome helper; checking the composer and restoring only when empty is one synchronous UI transition.
- mj-tui has Tokio only as a dev dependency. Expose the existing nonblocking ChatDaemonSender alias from mj-chat rather than adding a runtime dependency just to name the bridge.

- A command refusal previously discarded its captured text after removing the pending entry. Recoverable entries now own that text/error independently; unrelated success cannot erase them. Restoring also preserves the displaced draft rather than merely moving the data-loss case.
- Reverse search stores its selected match in input, independently of its query. The compact host now renders both and uses shared status formatting from history_search_footer. Newline markers and cursor columns use the same sanitized display mapping.
- Search and Up/Ctrl-P were separate asynchronous owners: session listings formerly returned via ChatIoUpdate while reverse search used composer updates. Session listings now use the existing composer result channel/generation guard for both attached and detached editors, with supervised task cancellation.

- The detached reply editor reused Prompt input but had no history dispatcher or project binding. The existing ActiveChat dispatcher can move onto ChatState without a second fetch path: both hosts use its composer result channel and existing generation checks; the composer owns cancellation.
- Notification body rows now use the transcript wrapper and copy metadata together, so copy joins soft-wrapped words while preserving hard line breaks and wide Unicode. The reply editor and controls are outside that registered surface.

Quick reply uses a separate real ChatState Prompt editor per destination session, including the existing platform key normalization. Bare global pane hints such as `?` must yield to this text owner; key matching and advertised labels share that eligibility. Send reads plain editor text, not the saved-draft envelope. The existing durable queue owns acceptance/refusal and uncertain-admission retries; matching outcomes settle the Quick reply line before the generic conversation-draft restoration path. Errors reset the visible card's scroll so a failure cannot remain above the viewport. The extended state regression verifies this on a genuinely scrolled long card. The header-color golden locates the metadata header itself, avoiding the identically named session-list row. Reconstructed AFK frames confirm modal content coordinates remain unchanged; diff alignment of repeated blanks can otherwise make the resized background look like dialog movement.

The abandoned reader experiment was fully restored before this removal. Removing Go To also removes the second transcript input: ChatState again derives its entries only from the live projection, with its ordinary asynchronous prefix conversion. No missing-entry reconciliation or extra coverage metadata remains necessary. The card's one-item read remains independent of chat entries and still provides buried question text.

Summary-only navigation previously saved a query for the latest agent message, rather than the displayed reply's boundary. That query could resolve against a later projection. The existing daemon-published ReadThrough ordinal supplies the immutable completion boundary without extending the summary wire shape; the pending position now carries that boundary and the original NotificationCard. When no daemon dismissal target exists, the already-loaded summary frontier supplies the boundary at capture time.

The live snapshot already carries ProjectionWindow, but the chat discarded it; the old metadata carried only an omitted-item count and could not distinguish trim gaps from removals. ProjectionWindow now publishes normalized half-open omission intervals, rebuilt at the database actor reader from its selected turn ranges and extended by actual trims/feed clipping. Within covered positions, a missing identity is authoritative removal. The chat also retains its canonical input handles so rows awaiting local prefix conversion are not mistaken for removals while a stored history read completes. The terminal regression drives the existing transcript projector's consumption path; no terminal-content inference or special fallback-tool rule is added to the chat.


The first chunking fix classified the legacy StoredProjectionTail reply as chunkable. Older clients can reassemble fragments but then reject that reply kind in their own is_chunked check, so even small count-mode replies broke existing TUIs. Count mode must retain its original unchunked reply. A new StoredProjectionRange reply can safely be chunked because only protocol-103 clients send start_position. The integration now sends the old JSON action shape and decodes exactly one raw ResponseEnvelope, so a ReplyChunk cannot hide behind the modern client's decoder.


A local count is not a stable range: after a chat through 600 estimates 579 items for target 30, a store through 610 starts that tail at 32. StoredProjectionTail now accepts an exclusive choice of count or start position; the start-position SQL query and all projection fields share one read transaction and use the existing `(session_id, position, stable_id)` index. The generic chunk decoder already verifies the reassembled reply against the same is_chunked classification used by the writer; adding the stored projection/item kinds enables both ends without a transport redesign. ProjectionWindow::trim can retain a streaming old turn and the latest tail while omitting completed middle turns. Both hydration and subsequent live projection updates must preserve every missing identity, not just entries before the first live row.


The previous navigation helper treated a known stable id as a successful jump even when the chat had no corresponding entry. ChatState then fell back to Bottom during resolution. ChatState::apply_materialized also rebuilds only when its event ordinal changes, so a wider stored tail at the same frontier cannot be installed by that method alone. The stored-tail API already forwards the caller's count into load_materialized_projection_tail and its SQL LIMIT. Stored transcript conversion can reuse TranscriptSnapshot's background construction; chat hydration must preserve the loaded prefix when the next bounded live projection arrives.


Later must retain the full card identity too: a session-only deferral would still defer a new question published between press and release. Its local ordering now matches both session and dismissal target. Pending question navigation also retains that identity; replacement requests terminate through the existing missing-message notice instead of satisfying the old request's locator. The wire has no terminal "locator failed" fact, so an unresolved current question stays pending with an explicit waiting-for-locator notice rather than silently consuming an End anchor.


The transcript table already has `PRIMARY KEY(session_id, stable_id)` (baseline.sql). The exact-key read returns an old item independently of the 512-item tail; its daemon operation uses the existing blocking database read path. Protocol 103 introduces the operation; the client returns Unsupported without sending an action when endpoint metadata is older. No schema/persisted-format change is needed.


The path called a full projection in the TUI is not necessarily whole history. `request_materialized_projection` accepts the worker/runtime snapshot and only prepares it in the background. The live `ClientSessionControl::snapshot` path in `mj-controller/src/session_manager/client_backend.rs:152-163` returns `handle.view().snapshot.materialized`; `SessionControlBackend::snapshot` has the same default in `mj-client/src/session.rs:432-444`. That live view can already omit history before the daemon/runtime tail. Only the no-live-handle fallback at client_backend.rs:164-170 invokes the unbounded database reader. Under the parent's explicit instruction to stop if this path does not supply messages beyond the 512-item tail, the worker stopped before source edits.

An unbounded daemon-owned read exists: `database::load_materialized_session` -> `load_materialized_session_with` -> `load_materialized_session_bounded(..., None)` -> `read_materialized_transcript` without LIMIT (`mj-controller/src/database/materialized.rs:12,1416-1430,1599-1620`). A narrow indexed read could instead select a TranscriptItem by `(session_id, stable_id)` from `materialized_transcript_items`; the exact-key lookup pattern already exists in `hydrate_terminal_outputs` at materialized.rs:657 and `materialized_transcript_item_exists` at 439. There is no current client method exposing a body read by that key. `StoredProjectionTail` accepts a transcript count, but remains a bounded-count API; it is not a documented unbounded or stable-ID message read.

The seconds-precision filter added for age handling discarded the attention fixture's valid 10/20/30 ms activity timestamps before sorting. Those entries fell back to their identical inherited `updated_at` value (the fixture varies `created_at`, not `updated_at`) and tied by session ID, selecting asks instead of remote. Sorting now retains positive millisecond precision, while formatting alone renders an age rounded to epoch zero as unknown. Unknown timestamps remain zero after fallback and sort after positive known recency. The existing state regression independently checks sub-second recency and unknown/zero ordering.

After restoring the correct card identities, the only remaining golden differences were seven `<age>` cells for the fixture's sub-second epoch timestamps. Those old age expectations masked the epoch-zero age defect; they now correctly say `age unknown`. A mechanical diff check verified exactly seven age-only changes, without changing card identities, ordering or layout.

The palette's leaked numeric age came from a fixture activity timestamp of 2,000 ms after epoch (not a real zero timestamp). The old title-anchored normalizer missed it because the context menu covered the title. Header normalization now recognizes its own attention glyph, border and two metadata separators, preserving isolation from notice-dialog clocks. Independently, ingestion accepted zero timestamps as `Some(0)`; summary/full preparation now rejects zero, and card age formatting treats timestamps rounding to epoch zero as unknown without dropping their millisecond ordering precision. The existing state test verifies zero ingestion and unknown-age rendering.

The previous loading decision inferred pending work from absent fields. The existing hydration status now defaults to Unavailable, records completed startup summaries as Summary, and uses Loading only while the existing SummaryReads/projection owners report pending work. The CLI projects those owners' current sets into the same field; cancellation/failure ends Loading. Summary remains distinct from full Ready so Important navigation still waits for a projection-backed stable ID. No parallel loading flag or fetch path was added. The existing summary-only ingestion assertion was updated to the explicit Summary state.

Startup summary storage selects the newest agent message as `(position, text)` (`mj-controller/src/database/materialized.rs:329`), but the summary shape has no stable message ID. The review follow-up therefore keeps that source and wire shape unchanged: `NotificationPosition::LatestAgentMessage` remains pending until the shared `notification_position` owner can read the current full projection's ID. Taking a projection cache for background work and accepting a newer summary both mark hydration Loading, preventing an older cached ID from naming newer summary text.

The existing hydration selection was in CLI `wanted_summary_sessions`; it now delegates to one TUI model owner with the same ordinary workspace/pane/family membership plus the displayed content session. The existing `SummaryReads` background mechanism is unchanged. A promoted parent card resolves its actual content holder using `waiting_child_of`; its parent's dismissal target stays untouched. The existing family subscription also retains that parent's family while the child's approval is displayed.


A subsequent parent full-suite run found two UTF-8 panics in the shared golden age normalizer, not golden comparisons: `dialogs::tests::golden_dashboard_dialogs` and `tests::the_notice_log_wraps_a_long_failure_instead_of_cutting_its_tail`. The normalizer searched for `s ago` in unrelated notice text, then added one byte to a non-ASCII border character's start. The helper now trims only ASCII digits to obtain a valid boundary and applies solely to a notification header immediately under its Notifications title. After the boundary-only correction, the dialog's actual/expected diff exposed the same helper altering two notice age cells; restricting its scope fixed that too. Both tests pass against unchanged goldens, and dialog height/notice wrapping did not change.


Parent review found that the pane independently hid locally read Important cards while the shared session-row attention owner retained the glyph. Receipt and pending-dismissal handling now live in that shared owner, with `notification_dismissals` tracked as a cache input so a new dismissal target cannot leave attention stale. The notification state test checks glyph/card agreement during submission, failure recovery, accepted receipts, and target changes. Notification entries no longer carry or eagerly build text or stable IDs; rendering resolves only the first card and shares its text between layout and drawing.


The original chat API exposed `TranscriptPosition::at_stable_id` but no direct bottom-position constructor. The initial unknown-id fallback used Ctrl-End, which `mj-chat/src/chat/keys.rs:29-49` routes to an open elicitation before transcript navigation. The parent approved a narrow ownership exception for `TranscriptPosition::bottom()`. Both known and unknown targets now use direct `restore_transcript_position`; the focused CLI regression verifies the viewport reaches Bottom while preserving a pending form answer and the unsent composer draft. No serialized representation changed.


The fourth support band raises the minimum height by three rows in Standard size (one row when minimized). The existing allocator now owns all four bands, including hypothetical maximum calculations. Existing short-terminal golden fixtures were increased by those three rows. Wizard regressions exposed that the too-small-dashboard path suppressed otherwise usable modal dialogs; the path now draws an open modal over the dashboard-size notice, as the ordinary render path does.


Pane-size storage is not an extensible map of pane kinds. `mj-core/src/workspace.rs:41-45` defines a fixed `PaneSizes` struct with three fields. `mj-controller/src/database/baseline.sql:228-234` stores three named columns and an exclusive-maximized CHECK over them. `mj-controller/src/database/workspaces.rs:66-86,96-117` reads and writes exactly those columns. `mj-client/src/daemon.rs:918-920` sends the same `PaneSizes` through `SaveWorkspacePaneSizes`. Persisting Notifications requires a shared API/data-shape change and database migration, beyond the worker's ownership. Serde currently ignores unknown struct fields, but that alone cannot store a fourth size or enforce its maximum against existing panes. This was the initial scope gate; the parent subsequently authorized process-local sizing.


## Decision Log

- Decision: accepted 066ae3121 review: use a single typed composer request queue and exhaustive dispatcher for all attached/detached Prompt requests. Forward daemon mutations through the same host bridge, ordered per session and concurrent across sessions, and carry the queue and history generation with live-composer transfers.
  Rationale: a new request kind must force a dispatch decision at compilation, rather than relying on each host to remember another pump. Background reads retain their existing generation guards and cancellation.
  Date/Author: 2026-11-17 / parent review request, implementation worker.
- Decision: accepted 066ae3121 review: Send has its own command/control bound to Ctrl+Enter and always submits current text. Restore unsent is a separate command/control bound to Alt+Enter; ordinary Enter retains its send/session default.
  Rationale: a refusal cannot change the meaning or target of a Send gesture.
  Date/Author: 2026-11-17 / parent review request, implementation worker.
- Decision: accepted 066ae3121 review: handle direct and durable Quick reply failures through one CLI outcome helper. Always log the session/full unsent text; when no card remains, put it unsent in an empty conversation composer, preserving occupied drafts. Report uncertain admission with full text but do not restore/replay while its status is unknown.
  Rationale: a detached editor is unreachable after its card disappears, and background failure must remain visible and recoverable without overwriting newer work.
  Date/Author: 2026-11-17 / parent review request, implementation worker.

- Decision: accepted f5b4539b4 review: keep recoverable reply text/error per session until matching text is sent successfully. Alt+Enter and the clickable Restore unsent button restore the captured candidate; Enter sends the displayed draft. Preserve displaced drafts in the same recovery collection and rotate restored entries to the back so every retained reply remains reachable. Restoring cancels reverse search through the editor owner and preserves its original draft, not its temporary match.
  Rationale: editing and unrelated receipts must never destroy an unsent submission; restoring must preserve newer work too. The gesture captures the restore text and original card/session just as Send captures text.
  Date/Author: 2026-11-17 / parent review request, implementation worker.
- Decision: accepted f5b4539b4 review: factor status formatting at the Prompt history owner, show the selected match/query/status together in the compact host, and map literal newlines to visible ⏎ cells only for presentation.
  Rationale: one status interpretation and one display/cursor mapping prevent invisible content and divergent search modes; payload text remains unchanged.
  Date/Author: 2026-11-17 / parent review request, implementation worker.
- Decision: accepted f5b4539b4 review: move session-prompt list dispatch/results onto the existing composer owner used by both hosts, retain its generation checks, request listings for each detached destination and record successful Quick replies in Prompt history.
  Rationale: Up/Ctrl-P and reverse search need their respective existing request paths; no new database or daemon operation is needed.
  Date/Author: 2026-11-17 / parent review request, implementation worker.

- Decision: user request 2026-11-17: reuse the global transcript selection state machine and transcript row-copy helpers for notification text; give each card/text-width row space a distinct surface identity.
  Rationale: drag, edge scrolling, highlight, clipboard routing and soft-wrap copy semantics have one owner. Removing a surface invalidates its selection before rendering/routing; reply/buttons retain their own pointer ownership.
  Date/Author: 2026-11-17 / user request, implementation worker.
- Decision: accepted quick-reply review: dispatch detached history through the existing composer history owner, bind the destination session project, and offer Enter to the editor before pane actions. Keep multiple command-bound sends only for distinct drafts.
  Rationale: the editor decides modal consumption; history results retain their existing generation guard and supervised cancellation. A pending network action cannot capture keyboard focus or prohibit editing a different draft.
  Date/Author: 2026-11-17 / parent review ruling, implementation worker.

- Decision: User request 2026-11-17: Go to Session first, Quick reply above the buttons with Send at right, Later renamed Skip, one-space text margins and a blank separating line, and a distinct header color.
  Rationale: the user requested ordinary session opening, not message targeting. Use the card text's form holder when applicable. Quick reply uses the real Prompt's key handler in single-line presentation; draft and in-flight request belong to the destination session, with immutable card/command identity. The existing durable prompt queue owns admission, retries and delivery receipts; successful delivery clears the matching line, refusals retain it and show a card error. Use Alt+g session, Alt+n Clear & Next, Alt+s Skip, Alt+c Clear all ! and Alt+Enter Send, plus Enter send/session. Bare letters remain text. Palette secondary already provides a distinct header color.

- Decision: User decision 2026-11-17: drop Go To Message.
  Rationale: reaching old messages needed either a history merge that failed six review rounds or a modal Earlier messages reader the user found not useful ("that's not very useful"). Remove navigation and all machinery that existed only for it. A buried question's full text still shows on the card through the protocol-103 one-item read. Clear and Later keep their captured card identity; legacy count-mode StoredProjectionTail stays unchanged and unchunked.

- Decision: resolve pending Important navigation to the newest nonempty agent message at or before the captured ReadThrough boundary, preserving NotificationCard through the CLI; never recompute the boundary from the current card after hydration.
  Rationale: the parent accepted issue #2 and explicitly allowed boundary-based resolution. This keeps message selection in the shared notification-position owner and needs no daemon, schema or persisted-format change. Issue #1 is deferred; do not touch history merge or prefix hydration for this round.

- Decision: extend ProjectionWindow with optional, default-empty omission ranges computed by selection owners. Use those ranges in both live updates and stored-history hydration; keep covered identities only when the canonical live input still contains them. Pass the window through ActiveChat and treat apply_materialized without a window as a complete projection.
  Rationale: parent accepted the 64316e6d3 finding and explicitly authorized extending metadata at its owner. A missing rendered entry can mean omission, deliberate removal or pending conversion, so the reader must consult published coverage and canonical identity rather than infer meaning from tool content. The field is additive runtime metadata, omitted when empty; no store schema or persisted format changes.
  Date/Author: 2026-11-17, parent session and implementation worker.


- Decision: preserve legacy count-mode StoredProjectionTail replies unchanged and unchunked. Wrap start-position results in the new StoredProjectionRange reply, classified for the existing chunk transport alongside StoredTranscriptItem. Keep the shared database range read and client request owner; only mode-specific reply wrapping/decoding differs. Protocol remains 103.
  Rationale: parent identified that older clients reject chunked StoredProjectionTail after reassembly. Only 103 clients request start_position, so the new reply kind isolates the new large-response behavior without changing existing tail seeding.
  Date/Author: 2026-11-17, parent session and implementation worker.


- Decision: extend StoredProjectionTail with mutually exclusive optional items and start_position selectors and keep one database selection/parser owner for both. A notification jump sends the located position minus eight preceding positions; the daemon chooses its snapshot's end. Older endpoints return Unsupported without an unknown request, and no client retry adjusts counts. The operation and chunking remain in unshipped protocol 103.
  Rationale: parent accepted the race and authorized this shape; a start-position query makes new stored output incapable of excluding the target.
  Date/Author: 2026-11-17, parent session and implementation worker.

- Decision (reply classification corrected by the compatibility follow-up above): classify new range and stored item replies for the existing chunk transport, and merge loaded/live entries by `(position, stable id)` in one helper with live identities replacing matching loaded identities.
  Rationale: the existing reply decoder validates the same chunk classification; individual bodies and requested history can exceed a frame. A gapped live view is not evidence that a loaded middle item was deleted.
  Date/Author: 2026-11-17, parent session and implementation worker.


- Decision: a pending notification position is consumed only after ChatState confirms the target entry was anchored, or after an explicitly reported failure. Missing chat entries trigger the existing count-limited stored-tail operation through SummaryReads' generation/cancellation owner. The count spans target to latest position plus eight leading items to show the question's immediate context; it is not a cap. Prepared entries hydrate the live chat without replacing interactive state, and the widened history remains retained across live tail updates.
  Rationale: parent accepted all reviewers' 1d2150be0 finding and specified this design. A stable id alone does not prove that the visible chat can resolve it; loading and anchoring are distinct states. No new daemon API, protocol or persisted format is introduced.
  Date/Author: 2026-11-17, parent session and implementation worker.


- Decision: bind pointer actions to an immutable snapshot of the rendered card, captured when the Form arms the control, and carry the session/dismissal identity in the resulting dashboard action. Keys capture when pressed. Keep missing question locators pending through the existing notification-position map and retry when operational updates arrive.
  Rationale: parent approved issues 1 and 3 independently of content acquisition; the existing Form pointer cancellation still owns cancellation on session-order/modal changes, without a second freshness check.
  Date/Author: 2026-11-17, parent ruling and implementation worker.

- Decision: expose one stored transcript item by exact `(session_id, stable_id)` through daemon/client, as another read kind in the existing generation/cancellation owner. Read only for the displayed located question absent from the loaded tail; Missing and Unsupported are terminal card states.
  Rationale: parent authorized this operation instead of reading thousands of coordinator messages. The existing primary key covers it, so no migration is required. Protocol 103 permits a definitive older-daemon Unsupported result without probing unknown actions.
  Date/Author: 2026-11-17, parent ruling and implementation worker.


- Decision: stop and return the bounded-snapshot finding before implementing the f7b8ed60b review fixes.
  Rationale: the parent explicitly requires stopping if the purported full projection cannot supply a located question outside the runtime tail. The live snapshot path returns the already bounded view; choosing a daemon-owned unbounded or stable-ID read changes the agreed data acquisition design.
  Date/Author: 2026-11-17, review follow-up worker.

- Decision: preserve every nonzero stored activity timestamp at millisecond precision for ordering, independently of the one-second precision used to render its age.
  Rationale: presentation rounding must not erase newer/older distinctions or replace real activity with fallback ties. Unknown/zero recency sorts last, and an age rounded to epoch zero remains `age unknown`.
  Date/Author: 2026-11-17, parent recency follow-up.

- Decision: derive loading from actual existing request-owner state, with an explicit Summary stage in TranscriptHydration and Unavailable as the initial state. Empty hydrated content uses the message owner's normal placeholder.
  Rationale: missing text does not prove that any work is running, and loading must end when an empty summary completes or a read is cancelled/failed. Summary content availability and full-projection identity availability must remain distinct for delayed Important navigation.
  Date/Author: 2026-11-17, parent full-suite follow-up.

- Decision: defer Go To for summary-only Important cards through the existing pending-position map rather than extend startup summaries or independently choose a target from chat state. `notification_position` is the single owner for the message identity; full projection completion retries the pending request.
  Rationale: summary storage does not currently return a stable ID, and keeping one projection-backed message owner avoids a new wire shape and a second interpretation of which message an Important card means.
  Date/Author: 2026-11-17, review follow-up worker.
- Decision: use `waiting_child_of` for promoted-card text and destination, retain daemon-published parent dismissal identities, and add the resolved displayed session to the existing background hydration owner's set.
  Rationale: approvals must lead to the conversation that can answer them, and a workspace-spanning card must have the same content-loading path as ordinary session rows.
  Date/Author: 2026-11-17, review follow-up worker.

- Decision: derive local receipt and in-flight dismissal visibility only in `presentation_attention`, the session-row attention owner. Track dismissal-target revisions in the shared session-facts/order caches. Notification entries contain metadata only; materialize the top card's message once per render and reuse it for sizing and drawing, and resolve its ID separately when Go To runs.
  Rationale: parent review requires the session glyph, workspace badges, host notifications and pane to read one attention fact, and avoids scanning every waiting transcript on render/key/footer queries.
  Date/Author: 2026-11-17, parent review and implementation worker.

- Decision: add `TranscriptPosition::bottom()` in mj-chat and use direct position restoration for unknown-message navigation, with a pending-form regression in the CLI.
  Rationale: parent approved the narrow ownership exception; direct restoration bypasses form/composer key handling and reuses the existing Bottom anchor without changing its persisted representation.
  Date/Author: 2026-11-17, parent session.

- Decision: hold Notifications size beside PaneSizes in TUI memory only, initially Standard. It resets on TUI restart. Maximizing Notifications clears the other three maxima; maximizing a persisted pane resets Notifications to Standard.
  Rationale: parent ruling avoids a schema/API change for a size preference; older writers cannot respect a fourth maximum in the current STRICT schema.
  Date/Author: 2026-11-17, parent session.

- Decision: stop before implementation and return the pane-size storage finding to the parent.
  Rationale: the assignment explicitly requires stopping before any needed daemon protocol, schema or persisted-format change, including this storage question. A process-local size could avoid the boundary but changes the settled persistence design and requires a parent ruling.
  Date/Author: 2026-11-17, implementation worker.

- Decision: the pane lives in the TUI only and holds `?` and `!` sessions. `✓` (unread) and `×` (failed) are excluded.
  Rationale: the user chose "TUI" and "? and |" on 2026-11-17. `|` is not an mj glyph, so it was read as `!`, and the user was told so.
  Date/Author: 2026-11-17, user and parent session.
- Decision: Clear All clears only `!` cards, and its label says so ("Clear all ! (n)").
  Rationale: dismissing a `?` now also tells the turn classifier that the ask is set aside (commit 8b7cfadfd and its follow-ups), so it will not come back. One key press would permanently bury real questions. The web viewer's Clear All also dismisses questions; this pane differs on purpose.
  Date/Author: 2026-11-17, parent session (proposed to the user, who did not object).
- Decision: "Later" replaces the proposed "Keep Unread".
  Rationale: "unread" already names `✓`, a daemon-owned state. The action changes no daemon state; it only reorders the stack inside this pane.
  Date/Author: 2026-11-17, parent session (proposed; not objected to).
- Decision: one card per session, ordered `?` band before `!` band, newest first within a band, with title then session id breaking ties.
  Rationale: the daemon holds one attention state, and one open question, per session. Newest first within bands matches the user's rule for every web workspace view (2026-11-12) and the web Notifications view (`mj-controller/src/web/viewer.js:1160-1169`).
  Date/Author: 2026-11-17, parent session.
- Decision: the card shows the message text from the same function that builds host notification bodies.
  Rationale: "the text of the message that needs the user" must have one owner. Two builders would drift.
  Date/Author: 2026-11-17, parent session.


## Outcomes & Retrospective

All three accepted 066ae3121 findings are complete without a commit. One typed queue/exhaustive dispatcher services attached and detached editors, with per-session ordered daemon bridges that allow unrelated sessions to proceed independently. Ctrl+C before the first pump retains its captured session/project, even when the project changes before dispatch. Separate Send and Restore gestures preserve current-text submission. A shared CLI outcome helper always logs failures with the session/full text, recovers into an empty ordinary composer after the card disappears, and preserves occupied drafts with copyable notice text. Unconfirmed admission is reported without replay. Eighteen unique affected tests and final dev/all-targets clippy for all three crates passed. Only the intentional footer binding label changed in the existing layout golden. Details: `.mj/agents/6ba3b2b07be9ab739a4e8e4af0412bba/qr-dispatch-report.md`.

The four accepted f5b4539b4 Quick reply findings are complete without a commit. Refused text/error and displaced drafts remain independently recoverable; captured Alt+Enter/Restore unsent gestures restore and cycle them without sending, while Enter sends the displayed draft. Unrelated successful receipts preserve other failures. Selected search matches and shared query/status render together; newline markers and cursor mapping expose multiline recalls without changing the payload. Up/Ctrl-P now receives the destination's stored prompt listing through the existing generation-checked composer path; successful Quick replies update local history and request a fresh listing. Eleven unique scoped tests and affected-crate all-targets clippy passed. The initial Unicode assertion was corrected to use existing wide-cell extraction; the initial clippy replacement lint was fixed mechanically. Evidence is in `.mj/agents/6ba3b2b07be9ab739a4e8e4af0412bba/qr-followup-report.md`.

The mouse-selection follow-up and three accepted Quick reply findings are complete without a commit. Notification text uses the global selection state machine and the transcript wrapper/copy metadata; controls stay outside that surface and an in-flight drag retains the pointer across them. Detached Prompt history now shares the attached composer dispatcher, cancellation, result channel and generation guard, bound to the destination project. Pending sends no longer capture keyboard focus or prevent another draft, and search Enter cannot submit. Thirty unique scoped tests and three-crate all-targets clippy passed. No golden was changed because existing cases have no selection highlight, and all four affected render goldens match exactly. Evidence: `.mj/agents/6ba3b2b07be9ab739a4e8e4af0412bba/selection-review-report.md`.

The user request of 2026-11-17 for Go to Session, Quick reply, Send, Skip, text margins and a distinct header is complete in the working tree without committing. Go to Session uses ordinary session navigation and the content holder, with the composer focused. Reply editors/drafts are session-bound and use the real Prompt handler; newline input is suppressed/flattened for the single-line presentation. Pointer gestures capture the card, destination and text; sends retain that identity under a stable prompt command ID. The existing asynchronous durable prompt queue owns submission, retries and receipts. Successful delivery clears only the matching line; refusals retain it, uncertain admission stays in flight, and errors remain visible on the card. The real conversation composer is independent. Alt+g opens, Alt+n clears/next, Alt+s skips, Alt+c clears Important entries, Alt+Enter sends, and plain Enter sends nonempty text or opens on empty. Alt+Up/Down scroll the card. The secondary palette supplies the blue metadata header; the new text margins, separator and reply row have the intended four-row height cost.

Ten unique scoped tests passed, including the extended pointer/input and receipt-owner regressions and four updated render comparisons. Final dev/all-targets clippy passed for brokk-mj-chat, brokk-mj-tui and brokk-mjolnir with warnings denied. Whitespace checks pass. Golden changes are limited to the requested controls/spacing/color sample and consequent pane/background resizing; AFK modal content positions are unchanged. Mechanical iteration fixes: a bool/unit match arm, bare `?` ownership, corrected header-color locator, and a mixed-source compile caused by editing the new editor API during a build (rerun with frozen sources). No daemon API, protocol, schema, persisted-format, history-merge or Go To Message mechanism was added. Evidence and failure-log paths: `.mj/agents/6ba3b2b07be9ab739a4e8e4af0412bba/quick-reply-report.md`; text capture: `quick-reply-pane.txt`.

User decision 2026-11-17 to drop Go To Message is implemented without committing. Removed its Enter action, button/footer, pending CLI navigation, summary-boundary and locator wait, start-position stored reads, history merge, omission metadata, navigation-only chat methods and tests. The rejected reader experiment was first restored to HEAD 428c8f9f0; the existing Earlier messages reader is unchanged. Buried question text still uses the indexed, chunked protocol-103 one-item read, with existing loading/unavailable states; count-mode StoredProjectionTail retains its shipped request shape and unchunked reply. Clear/Later identity, child request text, cross-workspace hydration, recency, shared attention and pane sizing remain. Twenty unique scoped tests passed: notification/host state (3), prefix (10), window (1), read owner (1), daemon/client integration (1) and four goldens. The integration retains found/missing/Unsupported and actual over-8-MiB item transport plus a legacy count single-frame check. Six-crate dev/all-targets clippy and whitespace checks passed. Only four golden files changed: 33 button rows and one footer row; the shorter footer also reveals the existing next shortcut in newly available width. No card text, age, dialog or height differences. Details, resolved expected-comparison failures, logs and a text capture are in `.mj/agents/6ba3b2b07be9ab739a4e8e4af0412bba/drop-goto-report.md` and `drop-goto-pane.txt`. Parent owns boundary-suite validation and committing; no live/default-instance operations or schema/persisted-format changes.

The accepted 11619f2aa issue #2 is fixed without committing. Summary-only Important navigation now captures NotificationCard plus its ReadThrough completion boundary, retaining both in the CLI pending-position path. The shared notification-position owner resolves the newest nonempty agent message at or before that captured boundary after hydration; a newer reply cannot retarget it. The existing CLI regression captures summary A (position 3), then receives reply B (position 4) and its updated dismissal target, and verifies the pending jump anchors A while a fresh card targets B. The focused regression (one test) and dev/all-targets clippy for brokk-mj-tui and brokk-mjolnir passed outside the sandbox; diff whitespace checks passed. No protocol, schema, summary wire shape or persisted format changed. Issue #1 remains deferred and history merge/prefix hydration code is untouched. Evidence: `.mj/agents/6ba3b2b07be9ab739a4e8e4af0412bba/summary-boundary-fix.md`.

The accepted 64316e6d3 removal finding is fixed without committing. ProjectionWindow now declares normalized omitted position ranges from the trim, SQL selection and client clipping owners. ActiveChat carries that authority into updates; history merges preserve missing entries only in declared omissions or beyond an older live frontier. Covered missing identities are removed, including when a stale stored-history result arrives. Canonical identities awaiting local prefix conversion still count as present. Restoring a complete projection clears earlier omissions. The existing gap regression drives real fallback-terminal creation and ACP replacement, proving the covered fallback disappears while a middle-gap message and its anchor survive; it also checks stale hydration and full-window restoration. Eleven unique scoped tests passed across the initial prefix run and corrected failing-test reruns, plus CLI navigation. Dev/all-targets clippy passed for brokk-mj-chat and brokk-mjolnir. Mechanical fixes: parent-visible helper scope, fixture streaming reset and canonical tool identity, and single-range iterator lint. Evidence and resolved failure logs are in `.mj/agents/6ba3b2b07be9ab739a4e8e4af0412bba/coverage-removals-fix.md`. No mj-transcript source, schema, persisted format, protocol number, golden, or live instance changed.

The parent compatibility finding is fixed without committing. Count-mode requests retain the exact legacy unchunked StoredProjectionTail reply, including requests using the old JSON action shape. Start-position requests now receive a separate StoredProjectionRange reply that is classified for chunk transport; StoredTranscriptItem remains chunked. The database range owner is unchanged, and the client request helper accepts the corresponding reply kind for each mode. The integration reads a single raw response frame for a legacy count request and asserts StoredProjectionTail before exercising the modern client. The over-8-MiB range test uses StoredProjectionRange and still validates exact reassembly; oversized item transport and older-endpoint Unsupported checks also pass. Both requested focused tests and three-crate dev/all-targets clippy pass. Protocol remains 103, with no schema or persisted-format change.


All three accepted 36aecf9ed findings are fixed without committing. History navigation now sends only a start position through the existing stored-tail operation; one database transaction selects all rows from that position through its own snapshot's end. The old count mode remains supported, while older daemons return Unsupported for position mode and the existing navigation failure path reports it visibly. Protocol remains 103 and no migration was added. Start-position range and item replies use the existing chunk transport; the real daemon/client integration successfully returned both a range and an individual body larger than 8 MiB, with exact content retained. That same integration observes a store advancing from 600 to 610 and proves target 30 remains in the range starting at 22. One identity merge now preserves loaded entries in every gap of a live projection and lets live entries replace matching identities. The extended existing prefix regression constructs its gap using ProjectionWindow::trim, verifies immediate hydration and later updates preserve the middle message and anchor, and checks live content wins without duplicates. All 13 focused tests and four-crate dev/all-targets clippy passed. No golden refresh or live/default-instance operation was needed.


The 1d2150be0 buried-question navigation defect is fixed in the working tree. A stable id alone no longer consumes the pending jump: ChatState confirms a concrete entry anchor. Missing entries retain the pending request and show `loading earlier history…`; the existing SummaryReads owner loads a count-limited stored tail covering the target through the latest loaded position plus eight preceding context items. TranscriptSnapshot conversion runs on a blocking background thread. Hydration widens the chat at the same event frontier without replacing its composer or forms, and the loaded prefix survives later bounded live updates. Failed reads and returned history still lacking the target consume the request only with an explicit `Could not load that message` notice and Bottom; successful jumps clear only their own loading notice. The existing CLI navigation regression covers both outcomes, actual anchor ordinal, draft/form preservation and a later live projection. Fifteen focused tests and final three-crate dev/all-targets clippy passed. No new daemon API, protocol/schema/persisted-format change, golden refresh, full suite or commit was made.


The three accepted f7b8ed60b review issues are fixed without committing. Pointer actions capture the rendered card at press and retain its identity through the dashboard action; Later matches the full identity, and Clear lets the daemon refuse a stale target. Existing Form/begin_surface_frame cancellation remains the sole gesture cancellation owner; no second freshness check was added beside it. Question navigation waits for a locator, retries after operational updates even without a new projection, and uses the existing unavailable-message notice if its original request changes. A primary-key single-item read fetches located questions outside the runtime tail through SummaryReads' existing generations, semaphore and cancellation. Found, missing and unsupported results terminate; loading is shown only for a pending read, and missing/unsupported show message not available. Protocol 103 adds this operation, with an explicit client Unsupported result for older endpoints and no database migration. All eight focused tests and four-crate dev/all-targets clippy pass; neither golden changed. The initial CLI fixture compile error was fixed by converting an array into SnapshotMap. No further live harness run was requested for this round; API behavior is tested against the real isolated daemon/client transport.


The recency regression is fixed: nonzero sub-second activity timestamps retain their ordering, and remote is again the selected card in the failing attention-navigation cases. All requested focused tests and TUI clippy pass. The attention golden changed only in seven age-label cells whose sub-second epoch timestamps must display `age unknown`; no ordering expectation was refreshed. No commit.

The parent-reported palette regression is fixed without changing its golden. Overlay-safe header normalization removes only changing card ages; zero timestamps are unknown at ingestion/recency selection. Loading now means actual pending hydration, and completed empty summaries render the placeholder. The requested palette golden and both notification state tests pass unchanged (3 tests); clippy/all targets passes for brokk-mj-tui and brokk-mjolnir, including the necessary CLI request-owner wiring. No commits or golden writes were made in this round.

Review follow-up on committed feature 41eb625c2 is complete in the working tree, without committing. Promoted child approvals show and open the form holder through the existing navigation helper while retaining daemon dismissal targets. The existing background summary-hydration owner includes the displayed content session in any workspace and transfers that demand when Later changes cards; unhydrated cards say `loading…`. Summary-only Important navigation remains pending until the shared projection-backed message owner supplies the stable target, then retries on accepted full-projection completion. The two extended notification state tests and the extended CLI notification-navigation regression pass. Clippy for both touched crates/all targets passes with warnings denied. No new daemon protocol, summary shape, schema, persisted format, fetch path, test family, or commit was introduced.

The shared message owner, local notification stack, scoped keys/buttons, four-band layout, and warm/cold stable-id positioning are implemented. Existing goldens were regenerated for the visible fourth pane, with one added question-card case in dashboard-layout-render. Full TUI validation found six mechanical regressions, all corrected and passing on a targeted rerun; 112 CLI dashboard tests and four dashboard PTY tests pass, and clippy passes for both crates/all targets. The isolated live daemon/TUI check passed for the empty pane, Tab focus, minimize/restore, resize to 80x24 and responsive detach; its daemon is stopped. No authenticated harness was configured, so daemon-backed nonempty dismissal behavior was not checked live. The parent-approved `TranscriptPosition::bottom()` constructor resolves unknown-message navigation even with an open form; the extended focused CLI test passes and preserves both drafts. Parent review also moved pending-dismissal/read-receipt visibility into the shared session-row attention owner, tracked target revisions in the attention cache, and made message text lazy for the displayed card. The two notification state tests and question-card golden pass; the affected attention-navigation golden passes unchanged, and the dismissal golden was refreshed for the now-consistent row/workspace glyphs. Clippy passes for brokk-mj-chat, brokk-mj-tui and brokk-mjolnir, all targets, with warnings denied. No implementation decisions remain open and no commit was made.


## Context and Orientation

The daemon (`mj daemon-run`, crate `mj-controller`) decides each session's attention. `mj-controller/src/daemon/attention.rs` projects a `SessionAttention` per session (NeedsInput `?`, Important `!`, Unread `✓`, Failed `×`). It also projects, per session, the target a dismissal must name: `NotificationDismissal::Question` for `?` (the open question, identified by its completed ordinal) and `NotificationDismissal::ReadThrough(completed_ordinal)` for `!` (attention.rs lines 291-349). The runtime feed publishes both to clients.

The TUI is the crate `mj-tui`, hosted by the `mj` binary in `mj-cli`. The TUI keeps the served attention map (`mj-tui/src/lib.rs` near 954, read in `mj-tui/src/dashboard_sessions.rs` 97-116) and a `SessionDetail` per session (`mj-tui/src/ingest.rs` 47-88). `SessionDetail` holds the transcript snapshot and projection cache, pending elicitations (questions a tool put to the user), the question locator target, and the latest agent message.

The question locator picks the reply that holds an open question. Its target reaches the TUI through `mj-cli/src/session_presentation.rs` 37-46 (`operational.question_message()`) and is stored on the session detail (`ingest.rs` 1526-1545). When the TUI opens a chat that is not already open, `mj-cli/src/dashboard/io.rs` 940-952 positions it at that reply with `TranscriptPosition::at_stable_id(id)`. A chat that is already open keeps its current position.

Host notifications are built in `mj-tui/src/notify.rs`. Lines 203-230 build the body for `?` from a pending elicitation, or else from `materialized_agent_message_text` over the transcript projection already in memory. Lines 231-234 use the cached latest agent message for `!`. This is in-memory state access, not a fetch.

Dismissal from the TUI is asynchronous. `NotificationDismissalState::{Submitting, Accepted}` (`ingest.rs` 94-101) tracks it. Starting a dismissal marks Submitting (1334-1349), and completion applies it or sets an error notice (1351-1363). The daemon action (`mj-controller/src/daemon/actions.rs` 1380-1416) submits a relay `DismissQuestion` for `?` and persists a read receipt for `!`. The workspace "clear" control already batches per-session dismissal targets (`mj-tui/src/pane_controls.rs` 574-587). The CLI host runs dashboard actions as background I/O and reports completion (`mj-cli/src/dashboard/actions.rs` 297-329; handled at `io.rs` 835). Nothing here blocks the render loop, and the new pane must keep it that way (AGENTS.md: the TUI never does blocking I/O on its event or render loop).

Dashboard panes: `mj-tui/src/dashboard_panes.rs` 27-109 holds pane size state and presets for the support panes Sessions, Targets and Quota (62-70). Pane sizes persist per workspace in the daemon store (`mj-controller/src/database/workspaces.rs` 62-99, table `workspace_pane_sizes`). Layout is generic tiling (`mj-tui/src/tile_layout.rs`). Commands live in a registry with pane scope, footer hints and actions (`mj-tui/src/actions.rs`: Tab "Next pane" at 1254-1277, pane size at 1279-1290; key mapping in `mj-tui/src/keybinds.rs` 123-126). Clickable buttons follow `render_session_buttons` (`mj-tui/src/render/sessions.rs` near 1623).

Tests: render tests use Ratatui's `TestBackend` (`mj-tui/src/test_support.rs` 75-78; examples in `mj-tui/src/render/tests.rs`). Documentation screenshots and goldens are produced in `mj-tui/src/docs_screenshots.rs`. State and input tests call `DashboardState::handle_key` (`mj-tui/src/tests.rs`).


## Plan of Work

Retain shared text, cross-workspace hydration, one-item text reads, presentation_attention and pane-size reconciliation. Add Go to Session and Send to the command registry, rename Later to Skip, and replace the pane's letter bindings with Alt modifiers. The Notifications input owner routes unmodified characters to its session's real ChatState Prompt editor before pane shortcuts; paste targets are session-bound and newlines are flattened. Single-line rendering reuses the editor's cursor and readline state.

Capture the card, content holder and reply at the gesture/key event. The existing StartupPrompts queue owns background admission, retry and delivery receipts. Route matching receipts back to the Quick reply line; accepted delivery clears the matching draft, refusals preserve it with a card error, and uncertain admission remains in flight with explicit status. The conversation's composer draft remains independent. No new daemon/store contract is required.

Extend the notification state/pointer tests and the existing durable prompt-refusal regression; update the existing layout golden with reply text and actual header color. Inspect the other affected render goldens before refreshing. Run scoped tests and three-crate dev/all-targets clippy outside the sandbox; do not commit.

## Concrete Steps

For the 066ae3121 follow-up, keep package selection fixed with `-p brokk-mj-chat -p brokk-mj-tui -p brokk-mjolnir`. Run filters `chat::history::tests`, `control_c_queues_abandoned_prompt_persistence_and_keeps_local_history`, `notifications_pane::tests`, `durable_refusals_restore_images_once_without_duplicating_recalled_input`, `pending_image_submission_and_deleted_markers_survive_a_live_composer_transfer`, and `no_two_commands_claim_the_same_key_in_one_pane`, plus the four existing notification-bearing goldens below. Rerun changed conditions only after fixes. Run all-targets clippy for the same three packages with `-D warnings`. All validation ran outside the sandbox, on the dev profile with normal Cargo storage.

For the f5b4539b4 follow-up, keep package selection fixed with `-p brokk-mj-chat -p brokk-mj-tui` and run the filters `chat::history::tests`, `notifications_pane::tests` and `an_accepted_prompt_lists_the_sessions_prompts_again_for_up`. Rerun only failed/changed conditions after fixes. Run `cargo clippy -p brokk-mj-chat -p brokk-mj-tui --all-targets -- -D warnings` outside the sandbox on the dev profile. All checks passed.

For the selection/review follow-up, run `cargo test -p brokk-mj-tui notifications_pane::tests`, the four golden filters listed below, `cargo test -p brokk-mj-chat history`, and `cargo test -p brokk-mjolnir dragging_`. All passed outside the sandbox on the dev profile.

Run scoped notification input/pointer tests and the command-registry collision test in brokk-mj-tui, the existing durable_refusals_restore_images_once_without_duplicating_recalled_input regression in brokk-mjolnir, and the four existing notification-bearing goldens (layout, attention navigation, session action menu, operator-away dialog). Review actual-versus-expected differences before updating expected output. Run:

    cargo clippy -p brokk-mj-chat -p brokk-mj-tui -p brokk-mjolnir --all-targets -- -D warnings

Use dev profile, normal Cargo target storage and elevated permissions for validation. Do not commit.

## Validation and Acceptance

Through handle_key and the existing pointer regression, verify real editor typing/kill/yank, single-line input, captured destination/card/text, session-specific drafts, Skip, empty Enter opening, child form-holder opening/sending, in-flight state and failure/success outcomes. Verify the existing asynchronous prompt owner settles Quick reply independently of the real composer. Check the intended new rows, margins, footer modifiers, button order and header color in the existing render goldens. No message-position, stored-range, omission metadata, daemon/schema change or new test family is part of this request.

## Idempotence and Recovery

The pane changes no daemon state except through the existing dismissal actions, which the daemon validates ("notification is no longer current" is refused). Repeating an action is safe. Reverting the change removes the pane. Notifications size and Later ordering reset at process restart; the persisted three-pane sizes remain valid for older clients.


## Artifacts and Notes

Web reference: `mj-controller/src/web/viewer.js` 1007-1014 (Notifications workspace), 1151-1169 (rows and order), 1186-1258 (cleared rows and Clear All), 1723-1738 (glyphs).


## Interfaces and Dependencies

Use the existing crates and patterns only: Ratatui widgets, the command registry, the dashboard background action path, and `NotificationDismissal` targets. Protocol 103 retains only the new read-only StoredTranscriptItem operation, using chunked replies; legacy count-mode StoredProjectionTail remains unchunked. No store schema or persisted format changes. The shared notification text function lives in `mj-tui` and is the only builder of that text.

Revision note (2026-11-17): recorded the fixed-column pane-size storage and the required parent decision before implementation, following the assignment's explicit stop condition.

Revision note (2026-11-17): parent approved process-local pane sizing, with restart reset and exclusive maximum transitions.

Revision note (2026-11-17): shared text/model and notification key transition tests are implemented; use the published Cargo package names, and leave committing to the parent as assigned.

Revision note (2026-11-17): unified the fourth support band with the existing allocator and preserved modal rendering when the dashboard alone is too short. All 33 render golden tests now pass; full validation remains.

Revision note (2026-11-17): recorded completed validation/live evidence and the ownership-boundary question for a direct bottom-position API; stable-id navigation passes, but the unknown-id warm-form edge remains unresolved.

Revision note (2026-11-17): parent approved the mj-chat constructor; unknown-message navigation now uses direct position restoration, and the existing CLI regression includes an open form with draft text.

Revision note (2026-11-17): implemented parent review fixes for one attention owner and lazy top-card text, and added target-revision invalidation assertions to the existing state-transition test. Focused validation is in progress.

Revision note (2026-11-17): all approved follow-up work is complete. Focused CLI pending-form navigation, notification state tests, the question-card golden and the existing attention-navigation golden pass; the dismissal golden was refreshed for the shared attention owner. Dev clippy passes for all three touched crates/all targets. The previously completed isolated live check was not repeated for these tested internal corrections.

Revision note (2026-11-17): repaired UTF-8 boundaries and scope in test-only notification-age normalization after the parent's full-suite run. Inspected the intermediate dialog golden diff; refreshed no goldens and changed no runtime layout. Only the two reported tests were rerun, both passing; final TUI all-targets clippy passed with warnings denied.

Revision note (2026-11-17): implemented the three accepted review findings after 41eb625c2. Declined findings remain outside this work. Notification state tests pass; summary-only navigation and affected-crate clippy are in progress. No commits.

Revision note (2026-11-17): the three accepted review fixes after 41eb625c2 are complete. Focused notification state tests (2), the notification navigation regression (1), affected-crate clippy/all targets, rustfmt check and diff check passed. The CLI assertion checks the stable target and the saved agent anchor's durable ordinal; it does not assume agent anchors serialize stable IDs. No commits.

Revision note (2026-11-17): fixed the parent-reported palette age/loading regressions at their sources, strengthened existing notification transition coverage, and passed the requested focused tests plus affected-crate clippy. The palette golden has no remaining diff and was not refreshed.

Revision note (2026-11-17): corrected the recency filter to preserve millisecond precision and confined epoch-zero handling to age formatting. Verified both requested goldens, notification state tests and TUI clippy; refreshed only seven demonstrably incorrect age-label cells, preserving every card identity/order/layout expectation.

Revision note (2026-11-17): recorded the f7b8ed60b review stop condition and decisive live-snapshot/database-reader evidence. No source edits, tests, builds or commits were performed in this round.

Revision note (2026-11-17): completed the parent-approved indexed read and captured-card/deferred-question follow-ups. Eight focused tests and four-crate clippy pass; no golden refresh, migration or commit.

Revision note (2026-11-17): fixed 1d2150be0 buried-question Go To by retaining pending navigation until the target is installed and anchored. Existing count-limited tail reads now run under the same generation/cancellation owner. Fifteen focused tests and three-crate clippy pass; no commit.

Revision note (2026-11-17): user removed Go To Message; navigation-only machinery and tests are gone, all 20 scoped tests and six-crate clippy pass, and the pane retains buried question text. No commit.

Revision note (2026-11-17): user-requested ordinary session opening and Quick reply are implemented; ten scoped tests and three-crate clippy pass. No commit.

Revision note (2026-11-17): completed shared notification selection and the accepted focus/search/history Quick reply review fixes. Thirty scoped tests, four unchanged goldens included, and three-crate dev/all-targets clippy passed; no commit.

Revision note (2026-11-17): completed all four accepted f5b4539b4 Quick reply findings. Eleven unique scoped tests and two-crate dev/all-targets clippy passed; no commit or golden refresh.

Revision note (2026-11-17): completed the three accepted 066ae3121 findings. Eighteen unique focused tests and final three-crate dev/all-targets clippy passed. The existing layout golden changed only its reviewed Send shortcut label; no commit.
