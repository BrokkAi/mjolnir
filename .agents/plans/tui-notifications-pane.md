# Add a TUI notifications pane that steps through sessions needing the user

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` in the repository root.


## Purpose / Big Picture

In the Mjolnir (`mj`) terminal dashboard (the TUI), every session row carries an attention glyph. `?` means the agent needs the user's input. It stays until the user answers or dismisses it. `!` means the agent delivered something the user should read, and it clears when the user views it. When many sessions are open, the user has to scan the session list, open each session, and scroll back to find the message that needs them. Questions are often buried in long replies, or in an earlier reply.

After this change, the dashboard has a bordered **Notifications** pane. It shows one card at a time from a stack of every session whose attention is `?` or `!`, and each card shows the message that needs the user. The user works through the stack with four actions:
- **Clear & Next** dismisses this notification and shows the next card.
- **Later** moves this card to the back of the stack and changes nothing else.
- **Go To Message** opens that session scrolled to the message, with the composer focused.
- **Clear All !** dismisses every `!` card at once and leaves the `?` cards.

To see it working, start an isolated instance with a session that ends a turn by asking a question, open the dashboard, and focus the Notifications pane. The card shows the question. Clear & Next removes it, and the glyph on the session row disappears.


## Progress

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

Milestone 1 builds the model with no rendering. First, move the body-building code in `mj-tui/src/notify.rs` (the `?` and `!` branches, 203-234) into one function that both host notifications and the pane call. It returns the text of the message that needs the user, plus that message's stable id when one is known: the question locator's message for `?`, and the latest agent message for `!`. Then add a small model, in a new module `mj-tui/src/notifications_pane.rs` or near the dashboard state, that derives the entries from the served attention map and the published dismissal targets. Each entry carries the session id, glyph, title, recency, text, message id and dismissal target. Exclude managed sub-agents, as the web view does. A `?` with no dismissal target, such as a pending form, is still shown, but Clear is disabled with a short reason. The model also keeps the pane's own "Later" order, which lives only in memory for the TUI process. Order entries by band (`?` then `!`), then newest first, then title, then id. Test the transitions directly: Clear & Next starts the session's dismissal and moves the selection to the next entry; Later rotates; a failed dismissal brings the card back with its error; Clear All ! starts dismissals for `!` entries only.

Milestone 2 adds the pane. Register a Notifications pane kind next to Sessions, Targets and Quota: pane state, size presets, render and focus, so Tab reaches it. Pane-size storage was checked and the parent ruled that Notifications size stays in TUI memory beside the persisted three-pane `PaneSizes`. Do not add a serialized field or change the store/API. Notifications starts at Standard and resets to Standard on TUI restart. One TUI pane-size transition owner enforces the exclusive maximum: maximizing Notifications demotes any persisted maximum, and maximizing a persisted pane demotes Notifications. Stored layouts always remain valid for older readers and writers. Render a bordered block titled "Notifications (n)". It holds the card header (glyph, session title, project, age), the message text, scrollable inside the card, and a button row and footer hints for the four actions. Bind keys in the command registry with pane scope; choose keys that do not collide, and list them in the footer. While a dismissal is in flight the card shows "clearing…", and the stack has already advanced. An empty stack shows "Nothing needs you". Wire actions through the existing background dashboard action path; Clear All ! reuses the batch dismissal used by workspace clear.

Milestone 3 makes Go To Message exact. Today an already open chat keeps its position. Add an explicit "open at message" action that forces the position to the given stable id even when the chat is warm, and focuses the composer. If the message id is unknown, the chat opens at its end and says the message could not be located. Viewing a `!` session already clears it through the read cursor, so the stack loses that card on its own.


## Concrete Steps

Work from the repository root, `/home/jonathan/Projects/mjolnir/.mj/clones/5201272a9025f4709aabd57b3cdd68e0`.

    cargo test -p brokk-mj-tui notifications
    cargo test -p brokk-mjolnir dashboard
    cargo clippy -p brokk-mj-tui -p brokk-mjolnir --all-targets -- -D warnings

Live check in an isolated instance (never the default instance). Drive the TUI in tmux, with its own MJ_CONFIG_DIR and MJ_DATA_DIR. Start a session that ends with a question, focus the Notifications pane, and exercise each action. Stop the test daemon afterwards with `mj --instance <name> daemon stop`.


## Validation and Acceptance

- A golden render shows the pane with a `?` card: header, message text, buttons and footer. Extend an existing render golden rather than adding a family of cases.
- State tests cover Clear & Next, Later, a dismissal failure returning the card, and Clear All ! leaving `?` cards in place.
- `cargo test` passes for the touched crates, and clippy is clean for them.
- In the live check, Clear & Next removes the session's glyph once the daemon accepts the dismissal. Go To Message opens an already open chat at the message, with the composer focused. Clear All ! leaves `?` sessions marked.


## Idempotence and Recovery

The pane changes no daemon state except through the existing dismissal actions, which the daemon validates ("notification is no longer current" is refused). Repeating an action is safe. Reverting the change removes the pane. Saved layouts that contain it must still load in an older TUI, as Milestone 2 requires.


## Artifacts and Notes

Web reference: `mj-controller/src/web/viewer.js` 1007-1014 (Notifications workspace), 1151-1169 (rows and order), 1186-1258 (cleared rows and Clear All), 1723-1738 (glyphs).


## Interfaces and Dependencies

Use the existing crates and patterns only: Ratatui widgets, the command registry, the dashboard background action path, and `NotificationDismissal` targets. No daemon protocol or schema change is expected; if one becomes necessary, stop and report first. The shared notification text function lives in `mj-tui` and is the only builder of that text.

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
