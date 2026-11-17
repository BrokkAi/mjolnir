# Replace the web Notifications workspace with a notifications screen modeled on the TUI pane

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` in the repository root.


## Purpose / Big Picture

The Mjolnir web viewer (served by the daemon, source `mj-controller/src/web/viewer.js`) lists sessions that need the user in a "Notifications" entry of the workspace picker. That list shows rows, not the messages. The terminal dashboard now has a Notifications pane (`.agents/plans/tui-notifications-pane.md`, `mj-tui/src/notifications_pane.rs`). It shows one card at a time with the full message that needs the user, plus actions and a quick reply. The user asked for the web viewer to work the same way. In their words (2026-11-17): "revamp the Web UI notifications along the lines of the TUI -- separate notifications screen modeled on the new pane (full text with go to, quick reply, etc); go to needs to wire up Back so that it goes back to notifications and not the workspace. replace notifications in the workspace picker with a 'N notifications' button to the right of the Mjolnir title".

After this change, the header shows an "N notifications" button to the right of the Mjolnir title, and the Notifications entry is gone from the workspace picker. The button opens a dedicated notifications screen. The screen shows one card at a time: a header line in its own color with the glyph, session, project and age; the full message text with a one-space margin; a blank line; a Quick reply line with a Send button; and a button row. The buttons are Go to Session, Clear & Next, Skip and Clear all ! (n). Go to Session opens the session, and the browser's Back, and the viewer's own back control if it has one, return to the notifications screen, not the workspace.


## Progress

- [x] (2026-11-17) Spec written from the user's request and the TUI pane's settled decisions.
- [x] (2026-11-17) Mapped the viewer's notifications workspace, header, routing and prompt path.
- [x] (2026-11-17) Implemented the screen, header button, `#notifications` routes with Back, actions, quick reply and full text (new endpoint).
- [x] (2026-11-17) Tests: `viewer.notifications.unit.test.mjs`, Playwright specs and goldens updated, one controller route test; clippy clean. Committed in this worktree.


## Surprises & Discoveries

- The viewer wire carries no message text (only `question_message`, an id), so the text gap was real. Added `GET /api/sessions/{id}/notification-message` (returns `{session_id, text|null}`): pending form request, else the question's transcript item via `load_materialized_transcript_item`, else the latest agent message, all on `spawn_blocking`. The `?`-only fallbacks the TUI has (review activity label, last message for harnesses without open-request support) are not reproduced.
- Failed and unreachable sessions were counted by the old web list; the screen now lists only `?` and `!`, as decided. The dashboard's own band ordering still uses `hasNotification`.
- Launch-failure notices used to show in the Notifications workspace; they now show on the notifications screen too.
- Review round (2026-11-17): Skip is a rotation over unique ids; quick reply builds its action with `submissionBody` and sends with `sendSubmission`, the composer's own path (one command id, "Delivery unconfirmed" on a lost acknowledgement, never a replay), and a line starting with `/` goes to the session's composer as an unsent draft because local commands need the open conversation; only the submitted draft version is cleared; failed message reads are not cached and retry on the next showing or Retry; the cache key carries the holder and pending request ids, so summary rows gained `pending_elicitation_ids`; the selection rule lives in `mj_core::notification_message`, used by both the endpoint and the TUI's `notification_message`.
- Launch failures are listed on the screen as cards after the `!` cards and counted in the header button, even though the TUI pane does not list them: the old web workspace was the only place they were reachable across workspaces, and a workspace dashboard shows only its own. Their dismissal is the viewer's existing local one; Go to Session appears when the failure names a session.
- A first visit (empty hash) with notifications opens the screen; a link that names a workspace now opens that workspace instead of being redirected.
- Playwright specs and goldens (`viewer_dashboard.txt`, `viewer_new_session.txt`) were regenerated: the header button sits outside `#app`, the picker lost its entry, and the compact-cards capture now shows the workspace.


## Decision Log

- Decision: copy the TUI pane's semantics exactly where they apply, as listed below.
  - Content: entries are sessions whose daemon-owned attention is `?` (needs input) or `!` (Important). Sub-agents are excluded; a parent promoted by a child's pending approval shows the form holder's request and opens the form holder.
  - Order: `?` before `!`, newest first within each band, then title, then session id. One card per session.
  - Clear & Next: dismisses through the existing daemon dismissal (Question for `?`, ReadThrough for `!`), using the identity (session plus dismissal target) captured when the action starts. The daemon refuses a stale target, and the card then shows the refusal.
  - Skip: moves the card to the back of the stack, held only in the page.
  - Clear all !: dismisses only `!` cards. Clearing a `?` sets that ask aside for the turn classifier, so a bulk clear would bury real questions. The old web Clear All also cleared questions; this changes on purpose.
  - Quick reply: Enter sends when the line has text, and Enter on an empty line does Go to Session. Sending submits a prompt to the captured card's session through the viewer's existing prompt path. A draft belongs to the session it was typed for and comes back when that card is shown again; it is never sent to another session.
  - Full text: when the located message is outside the session's loaded transcript tail, the text comes from the daemon's indexed one-item read (`load_materialized_transcript_item`, the operation behind the protocol-105 `StoredTranscriptItem`). The card says "loading…" while the read is in flight and "message not available" when it finds nothing.
  - No Go To Message: the user dropped it in the TUI on 2026-11-17. Go to Session opens the session normally.
  Rationale: the user asked for the web screen "along the lines of the TUI", and these were the user's decisions there.
  Date/Author: 2026-11-17, parent session.


## Outcomes & Retrospective

The screen, route (`#notifications`, `#notifications/<session>` for a conversation opened from it), header button, actions and per-session drafts are in. Back and the back control return to the screen. Quick reply does not dismiss the card; the daemon lowers attention when the agent resumes. The endpoint's database branches (question item, latest agent message) have no test of their own; only the form branch and the 404 are covered.


## Context and Orientation

The daemon owns attention: `mj-controller/src/daemon/attention.rs` projects `SessionAttention` (`?` needs input, `!` important, `✓` unread, `×` failed) and the dismissal target per session (`NotificationDismissal`). The web viewer is `mj-controller/src/web/viewer.js`. Its current Notifications workspace is around lines 1007-1014 (entry), 1151-1169 (rows and order), 1186-1258 (cleared rows and Clear All) and 1723-1738 (glyphs), as of the TUI mapping. Viewer unit tests live in `tests/e2e/web/*.unit.test.mjs` (for example `viewer.card-queue.unit.test.mjs`), and the server and API tests in `mj-controller/src/server/tests.rs`. The private memory note `web-viewer-dev` lists test commands and the action and capability architecture; the implementer should rediscover them from the code. The TUI text owner `notification_message` (`mj-tui/src/notify.rs`) shows which message a card refers to: for `?`, the question locator's message, or a pending form's request; for `!`, the latest agent message.


## Plan of Work

First map the viewer: header layout, workspace picker, the current Notifications workspace, how routes map to browser history (pushState and popstate), the session view's back control, how prompts are submitted, and what the viewer wire already carries per session (attention, dismissal targets, question message id, latest agent message text). Then build the notifications screen as its own route. Opening a session from it pushes a history entry whose Back returns to the screen, keeping the current card where possible. Add the header button with a live count of `?` plus `!` sessions, and remove the picker entry. Wire the actions to the existing endpoints. If the viewer lacks the message text for a card, add the smallest server-side read that returns it, reusing `load_materialized_transcript_item` on the daemon's blocking database path, and expose it through the viewer's existing API style. No new store schema. Nothing on the browser's main thread may block; requests are asynchronous and show in-flight state.


## Concrete Steps

Work in the worktree `/home/jonathan/Projects/mjolnir-wt/web-notifications` (detached HEAD at da4af1295). Run the viewer unit tests with node, as the existing tests do, and any `mj-controller` tests touched with `cargo test -p brokk-mj-controller <filter>`. Run clippy for `brokk-mj-controller` if Rust changes.


## Validation and Acceptance

- Viewer tests cover:
  - the order;
  - Clear & Next using the captured identity, including a stale refusal;
  - Skip;
  - Clear all ! leaving `?` cards;
  - quick reply: Enter sends to the captured session, the per-session draft, and Enter on an empty line opening the session;
  - Go to Session then Back returning to the notifications screen;
  - the header count.
- A golden or render test covers the screen's layout if the viewer has that kind of test; otherwise, a DOM-structure assertion.
- If an API was added, an integration test drives it from the client side the viewer uses.
- A manual check in an isolated instance's web viewer, if feasible; otherwise, state what only tests cover.


## Idempotence and Recovery

The screen changes daemon state only through existing dismissal and prompt actions, which the daemon validates.


## Artifacts and Notes

The TUI pane's render capture is at `/home/jonathan/Projects/mjolnir/.mj/clones/5201272a9025f4709aabd57b3cdd68e0/.mj/agents/6ba3b2b07be9ab739a4e8e4af0412bba/question-pane.txt`, for visual reference.


## Interfaces and Dependencies

Use the viewer's existing frameworks and patterns. No new dependencies.
