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

A subsequent parent full-suite run found two UTF-8 panics in the shared golden age normalizer, not golden comparisons: `dialogs::tests::golden_dashboard_dialogs` and `tests::the_notice_log_wraps_a_long_failure_instead_of_cutting_its_tail`. The normalizer searched for `s ago` in unrelated notice text, then added one byte to a non-ASCII border character's start. The helper now trims only ASCII digits to obtain a valid boundary and applies solely to a notification header immediately under its Notifications title. After the boundary-only correction, the dialog's actual/expected diff exposed the same helper altering two notice age cells; restricting its scope fixed that too. Both tests pass against unchanged goldens, and dialog height/notice wrapping did not change.


Parent review found that the pane independently hid locally read Important cards while the shared session-row attention owner retained the glyph. Receipt and pending-dismissal handling now live in that shared owner, with `notification_dismissals` tracked as a cache input so a new dismissal target cannot leave attention stale. The notification state test checks glyph/card agreement during submission, failure recovery, accepted receipts, and target changes. Notification entries no longer carry or eagerly build text or stable IDs; rendering resolves only the first card and shares its text between layout and drawing.


The original chat API exposed `TranscriptPosition::at_stable_id` but no direct bottom-position constructor. The initial unknown-id fallback used Ctrl-End, which `mj-chat/src/chat/keys.rs:29-49` routes to an open elicitation before transcript navigation. The parent approved a narrow ownership exception for `TranscriptPosition::bottom()`. Both known and unknown targets now use direct `restore_transcript_position`; the focused CLI regression verifies the viewport reaches Bottom while preserving a pending form answer and the unsent composer draft. No serialized representation changed.


The fourth support band raises the minimum height by three rows in Standard size (one row when minimized). The existing allocator now owns all four bands, including hypothetical maximum calculations. Existing short-terminal golden fixtures were increased by those three rows. Wizard regressions exposed that the too-small-dashboard path suppressed otherwise usable modal dialogs; the path now draws an open modal over the dashboard-size notice, as the ordinary render path does.


Pane-size storage is not an extensible map of pane kinds. `mj-core/src/workspace.rs:41-45` defines a fixed `PaneSizes` struct with three fields. `mj-controller/src/database/baseline.sql:228-234` stores three named columns and an exclusive-maximized CHECK over them. `mj-controller/src/database/workspaces.rs:66-86,96-117` reads and writes exactly those columns. `mj-client/src/daemon.rs:918-920` sends the same `PaneSizes` through `SaveWorkspacePaneSizes`. Persisting Notifications requires a shared API/data-shape change and database migration, beyond the worker's ownership. Serde currently ignores unknown struct fields, but that alone cannot store a fourth size or enforce its maximum against existing panes. This was the initial scope gate; the parent subsequently authorized process-local sizing.


## Decision Log

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
