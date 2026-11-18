# Keep a per-session stack of notification messages until the user reads or clears it

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` in the repository root.


## Purpose / Big Picture

Mjolnir marks sessions that need the user with `?` (the agent needs input) or `!` (the agent delivered something the user should read). The TUI Notifications pane (`.agents/plans/tui-notifications-pane.md`) and the web notifications screen (`.agents/plans/web-notifications-screen.md`) show one card per session with the message behind that mark. Phone and browser Web Push send the same marks. Four problems remain.

- A `!` vanishes when the agent keeps working. The daemon shows Important only while the session is idle and its latest assessment covers the last finished turn (`mj-controller/src/daemon/attention.rs`, around lines 521-535). When a later turn (a mailbox wake, a child's report) ends with an ordinary verdict, the earlier `!` is gone for good, though the user never saw it.
- A notification is replaced by a newer one from the same session, and the earlier message is lost from the card.
- Web Push repeats itself. In the live store, the coordinator session 5992a212 received 1,611 pushes with only 775 distinct bodies, often consecutive repeats. The episode identity in `mj-controller/src/web_push.rs` (around lines 265-290) comes from a turn's completed ordinal and from a signature that includes the attention, so a question that goes from `?` to Working and back is a new episode and pushes again.
- Web Push reaches sessions that never appear in either notifications list. Push walks the attention map itself, does not exclude managed sub-agents (41 sub-agent sessions received 51 pushes), and does not follow the daemon's choice of form holder, so a child and its promoted parent can both push.

The user's decisions (2026-11-17): "1. Yes, with the distinction that clicking on the session clears it, unlike a question mark. 2. I do want a full [message] stack, but we don't need to try to clear individual messages, just the entire stack at once. For each conversation, I want the most recent message shown by default with a previous and next message button. Put these to the right so that they're distinct from the buttons below the message, and then clear acts on the group as it does today." "I want a message stack, not a badge stack." And for push: "fold it in".

After this change every session keeps an ordered stack of its notification messages, one entry per finished turn whose verdict was `?` or `!`. A card shows the newest entry, with Previous and Next at the right of the card and a position such as "2 of 5". A `!` entry stays until the user reads the session (the read cursor passes it) or clears the card, even while the agent keeps working. A `?` entry stays until the user answers or clears. Clear removes the whole stack. One daemon-published list of cards decides which sessions and entries notify; the TUI pane, the web screen and Web Push all read it. Each entry pushes at most once, identified by its message.

To see it working, in an isolated instance: run a session whose turn ends with a `!` reply, then send it a mailbox message that makes it take another turn ending with an ordinary reply. The card still shows the `!` message, with "1 of 1"; with a second `!` turn it shows "2 of 2", newest first, and Previous reaches the earlier one. Opening the conversation to its end removes both. A subscribed browser receives one push per `!` message and none for the mailbox turn.


## Progress

- [x] (2026-11-17) Plan drafted from the user's decisions.
- [x] (2026-11-17) Read-only map of the worker, daemon, consumers and push (report `.mj/agents/93e77746055c80bf06caffa9666d4d48/notification-stack-map.md`, summarized in Context and Orientation). Plan finalized from it.
- [x] (2026-11-17) Milestone 1, contract: the shared types compile on their own (slice A, 493f2d0b2).
- [x] (2026-11-17) Milestone 2, worker stack: reducer, Clear and prune commands, relay protocol 71 and state 27, legacy seed, checkpoint (slice A).
- [x] (2026-11-17) Milestone 3, daemon publication: card map, attention glyph, Clear bridge, daemon protocol 106, viewer wire, entry text endpoint, StoredAgentMessageThrough (slice A).
- [x] (2026-11-17) Milestone 4, push: compatible migration 138 with new entry and delivery tables, identity per entry, reconcile from the card map, startup-hold baseline (slice B).
- [x] (2026-11-17) Milestone 5, surfaces: TUI pane and web screen with Previous, Next, position, per-entry text and Clear (slice C).
- [x] (2026-11-17) Integration: merged in a detached worktree; integrated review clean after two fixes (distinct native request identities, review verdicts on cards) and two declined findings (one form per card; id-less text after a canonical restore). Workspace clippy, the core, client, worker, TUI, CLI dashboard and controller suites and the viewer tests pass, apart from failures that also occur on master or pass alone (the worker suite times out under host load average above 200 and passes with 16 test threads).
- [x] (2026-11-17) Order cards by their oldest visible item (`since_ms`) and keep the reader's item when a new one arrives (user decision): timestamps 883d5133c and bf2b4a05f, web 3c4e3c1ce, TUI in the following commit. Skip is keyed by session and lasts until the card disappears, so new items in a skipped session do not bring it forward.
- [ ] Install (ask the user first) and check live behavior in an isolated instance.


## Surprises & Discoveries

- The question's message id is provisional when a turn completes. The question locator settles it later, synchronously for one candidate and asynchronously for several (`mj-worker/src/worker_runtime/unix/dispatch.rs:510-566`, `mj-worker/src/relay/verdict.rs:364-403`). Pushing before it settles could push the fallback id and then the corrected id. Evidence: the map's "Message targeting" row.
- Today's push episode for a session's own question is already keyed by the question's completion, but the signature also carries the attention, and an episode leaves the table when the session stops being eligible. The repeats come from a question going `?` → Working → `?` under the #1518 hold, which re-creates the episode. A durable per-entry ledger with retained deliveries removes this.
- The existing DismissQuestion relay command is refused while a turn is active, a prompt is queued or a form is outstanding (`mj-worker/src/relay/commands.rs:209-220`). It cannot serve a Clear that must work while the agent works.
- The person-input frontier advances when a person prompt is durably admitted (`mj-core/src/relay/snapshot/apply.rs:446-475`), including Quick reply, not when it is delivered. Mailbox wakes and other generated prompts leave it alone.


## Decision Log

- Decision: the session's worker owns the stack, in its relay snapshot beside the open request, and every reader asks it.
  Rationale: the worker already owns turn assessments, the open request and person_input_through, and survives daemon restarts. The daemon holds no agent state it cannot rebuild (AGENTS.md control plane rules).
  Date/Author: 2026-11-17, parent session.
- Decision: one reducer updates the stack and the open request together, so they cannot disagree.
  - Append (or update in place, keyed by completed ordinal) when a current assessment for a completion yields NeedsInput or Important, and at an explicit ACP InputRequired. A repeated assessment event for the same completion updates its entry and never appends a second one. QuestionTargetUpdated updates the matching entry's message together with the request.
  - Clear the whole stack, together with the request, on the existing transitions that already close the request: person prompt admission (including Quick reply), an answered elicitation, DismissQuestion, and an ACP conversation reset. Also on the new Clear command below. Mailbox wakes and other generated prompts leave it intact.
  - Late assessments and locator results for a completion at or before the person frontier are fenced exactly as the open request is today.
  Rationale: a single state machine with explicit transitions (AGENTS.md concurrency rule). Replying implies the user saw the card, which is an assumption stated to the user; they may narrow it to `?` entries only.
  Date/Author: 2026-11-17, parent session.
- Decision: Clear acts on the whole stack. There is no per-message dismissal.
  Rationale: the user's decision; it also avoids a per-message dismissal identity.
  Date/Author: 2026-11-17, user.
- Decision: keep the #1518 hold and today's gate for `?`; latch only `!`.
  - A `?` shows only when today's rule raises the open request: the session is idle, no prompt is active or queued, no newer person message, and no hold while a Claude or Codex agent waits on its own work (`semantic_question` and `request_hold` in `mj-controller/src/daemon/attention.rs`).
  - A `!` entry shows whenever its completion is after the read cursor, whatever the session is doing.
  - The visible entries of a session are its `!` entries after the read cursor, plus its `?` entries only while the open request is raised. The card exists when any entry is visible. Its badge is `?` when the open request is raised, else `!`. The dashboard glyph follows the badge, so an unread `!` now shows while the session works; failure and lifecycle states keep their precedence.
  Rationale: the user decided on 2026-11-13 (#1518) to hold `?` while the agent waits on its own work, after a study found about 83% of weighted false `?` alerts were such turns. The 2026-11-17 decision is about `!` disappearing. A held question already comes back when the wait ends, so it does not vanish.
  Date/Author: 2026-11-17, parent session.
- Decision: the worker prunes `!` entries the user has read. The daemon owns the read cursor (SessionRecord.viewed_through_event_ordinal). When it advances past a stack entry, the daemon sends the worker a monotone, idempotent `NotificationsRead { through }` relay command, and again on reattach. The worker drops `!` entries at or before `through`. The projection filters by the cursor in any case, so a late or lost prune only delays reclaiming space and never changes what is shown.
  Rationale: a coordinator session can take thousands of automated turns between person messages. Without a prune, read entries would accumulate in every snapshot and status update. The cursor keeps one owner; the prune is its consequence.
  Date/Author: 2026-11-17, parent session.
- Decision: a new `ClearNotifications { revision }` relay command replaces DismissQuestion for workers at the new relay protocol.
  - The stack carries a revision, the relay ordinal of its last mutation. The daemon publishes `NotificationDismissal::Stack { revision }` as the card's dismissal and validates it against the current card. The worker then compares it under its own lock, so a Clear captured before an append, a person message or a reset is refused. The revision is the command's idempotency key.
  - Clear does not require the session to be idle and leaves tool forms and their requests alone. When it clears an open question it applies the same set-aside fence as DismissQuestion (`set_aside_replies(false)`), which spares a later in-flight reply.
  - When the cleared stack has `!` entries, the daemon also records read-through at the highest cleared completion and no further, as the ReadThrough dismissal does today, so cleared messages do not come back as an unread `✓`.
  - Older workers keep DismissQuestion and ReadThrough unchanged.
  Rationale: one mechanism for clearing (user preference: one way per job). Validating at both owners keeps the decision serialized at the worker.
  Date/Author: 2026-11-17, parent session.
- Decision (user, 2026-11-17: "fold it in"): phone and web push join this change.
  - The daemon publishes one card map at its attention publication. It decides which sessions and entries notify: it excludes managed sub-agents and sessions with notifications ignored, and picks a single form holder per card by today's precedence. The TUI pane, the web screen and Web Push read it, and none of them re-derives eligibility. The TUI pane therefore stops listing sessions with notifications ignored. Host alerts (terminal bell, desktop notification, terminal title) keep their own policy in this change.
  - A child's pending person-owned form notifies as its parent, through the selected form holder; Web Push follows that holder and no longer walks descendants itself.
  - A push fires once per visible entry. For a transcript entry its identity is (session, message id), never a turn ordinal, so the same message can never push twice. An entry with no transcript message (for example a provider stop without text) uses (session, its completion), namespaced apart. A live form uses (holder session, form request id). Native input and review sources keep their existing identities.
  - A `?` entry pushes only after its question locator has settled (found, not found, or failed), so the provisional id is never pushed. The worker must always record a final locator result.
  - When several entries become visible at once, push sends them oldest first, so the newest remains on the phone under the per-session tag (`mj-controller/src/web/service-worker.js`).
  Rationale: the live-store evidence in Purpose. Identical text in two different messages still pushes twice, by design: the user asked for identity by message.
  Date/Author: 2026-11-17, user and parent session.
- Decision: push stores its new ledger in new tables, a compatible migration. New `push_entries` (identity primary key, session id, visible flag, timestamps) and `push_entry_deliveries` (per endpoint and identity) replace the old episode logic in the new daemon. The old `push_episodes` and `push_deliveries` stay for older daemons and are dropped by a later breaking migration. On the first reconcile after the migration, entries already visible are recorded as delivered without pushing, so the cutover does not replay history.
  Rationale: changing `push_episodes` to several rows per session would be breaking, because an older daemon rebuilds it assuming one row per session, and a feature request does not authorize an incompatible upgrade of the live store (AGENTS.md). Separate tables are invisible to older daemons, which neither read nor delete them.
  Date/Author: 2026-11-17, parent session.
- Decision: the web screen keeps its `×` and launch-failure cards (committed in e117d219b) as a band derived from session attention, after the stack cards. They are not part of the card map, never push, and the TUI pane does not show them.
  Rationale: the user chose `?` and `!` for the TUI pane. The old web Notifications workspace listed failures, and review flagged their loss on the web.
  Date/Author: 2026-11-17, parent session.
- Decision: an older worker (below the new relay protocol) gets a single-entry card built from today's facts (the raised open request or Important, the located question, today's dismissal), with today's glyph rules. Card text for any entry comes through the shared rule in `mj-core/src/notification_message.rs`.
  Rationale: compatibility without a second policy: the fallback builds the same card type.
  Date/Author: 2026-11-17, parent session.
- Decisions made while implementing (2026-11-17, parent session):
  - Wire: a 106 daemon always sends `notification_cards` (empty when there are none) and `ViewerSession.notification_card` as an explicit null when a row has no card; an absent field means an older daemon.
  - A session whose attention is decided by failure or lifecycle precedence publishes no card; its stack is retained and the card returns on recovery.
  - A live form, including the harness's own pending input (`native.<view>.<generation>`) and reviewer questions, is the newest item on its card, counted in "k of n". Entry text is read from the card's own session; Go to Session and Quick reply address the holder.
  - The card's dismissal depends on its badge. A `?` card carries Stack{revision} and Clear removes the whole stack. A `!` card carries ReadThrough at its largest visible Important completion, so Clear marks those entries read and never dismisses a question that is held and therefore hidden.
  - Text for an entry without a message id is bounded to that entry's own turn (from its start position through its completion), so a turn that left no text shows the placeholder rather than an earlier turn's text.
  - The push cutover baseline is the first card map after the daemon's startup recovery hold is released; a worker that reattaches later may push its visible entries once.
  - An older worker's single-entry card and the seeded entry after its upgrade use the same EntryMessage, including None for an Important whose id is unknown, so a worker upgrade does not push again.
  - The client reads entry text through the daemon (`StoredAgentMessageThrough`), never from the daemon's store directly.
- Decision (user, 2026-11-17): "group by session order by min(notifiaction_time) ... we can give up newest-first and ? ! subordering". The complaint: "i'll be reading one and then a new one arrives and now i can't see what i was in the middle of reading".
  - Cards (one per session) are ordered by the time of their oldest visible item, oldest first, then session id. There is no `?`-before-`!` band and no newest-first order. A new item in a session already on screen does not move its card; a new session's card joins the back. Skip still moves a card to the back.
  - The daemon owns the time: each stack entry records when its turn completed (stamped by the worker from the completion event, so replay gives the same value), a live form records when it was raised, and the card publishes `since_ms`, the earliest of its visible items. The TUI and the web sort by it and never derive it.
  - Inside a card, the newest item is shown when the card first appears, as decided before. While the card stays on screen, a newly arrived item does not move the selection away from the item being read; "k of n" updates instead.
  Rationale: the user's decision; the last point applies the same complaint inside a card (parent session's reading, stated to the user).
  Date/Author: 2026-11-17, user and parent session.

## Outcomes & Retrospective

Known limitations accepted on 2026-11-17:
- After a canonical restore into a fresh store (a move or a restore from a checkpoint), an unread entry without a message id keeps its identity but shows placeholder text, because the restored store has no turn-usage row to bound its text. Go to Session still opens the conversation.
- One form per card: when several forms wait in one conversation, push and the surfaces show the selected one first; the next pushes when it becomes the card's form.
- A worker that reattaches after the push cutover baseline may push its visible entries once, and an older worker whose Important id cannot be recovered may push once more at its upgrade.
- A harness's own pending input has no recorded time of its own; its card time is the native projection's last recorded activity, which can advance while the same request stays pending and move that card. Entries from before this change without a recorded completion time use 0 and sort first.


## Context and Orientation

Terms. A completion is the relay event that ends a turn. Its relay ordinal (the "completed ordinal") is in the same ordinal space as the session's read cursor, `viewed_through_event_ordinal` on the daemon's SessionRecord (proved by f8cc32025). The open request is the worker's record of the latest unanswered question (`RelaySnapshot.request`, filtered by `open_request()` against the person frontier `person_input_through`). The read cursor advances when a client shows a conversation to its end (TUI: `mj-cli/src/dashboard.rs:1776-1816`; web: `mj-controller/src/web/viewer.js` around 9080-9123, receipts in `mj-controller/src/server_runtime/run.rs:1072-1104`, stored by `mj-controller/src/database/client_state.rs:181-240`).

Worker. Turn completion and the provisional reply id: `mj-core/src/relay/snapshot/apply.rs:1426-1473` (autonomous) and `:2056-2100` (prompted; explicit ACP InputRequired at about `:2085`). Verdicts: `mj-core/src/activity/eot_glide.rs:34-76`, `mj-worker/src/relay/verdict.rs:95-106,353-359,412-495` (a background refresh can emit a second assessment event for the same completion, `:439-444`). The reducer arm for TurnAssessmentUpdated is `apply.rs:263-307`; QuestionTargetUpdated `:239-255`; person prompt admission `:374,446-475`; mailbox `:481-491`; ElicitationResolved `:2024-2043`; DismissQuestion `:507-514`; conversation reset `:2110-2119`. The set-aside fence is in `mj-core/src/assessment.rs:579-712`. Worker command admission is `mj-worker/src/relay/commands.rs` (DismissQuestion guard at `:209-220`). RelayOperationalState carries the assessment, open request and frontier to the daemon (`mj-core/src/relay/snapshot.rs:698-724,1901-1927`). Versions: `RELAY_PROTOCOL_VERSION` (68) and `RELAY_STATE_VERSION` (26) in `mj-core/src/relay.rs:165-210`; snapshot upgrade and legacy seeds in `mj-worker/src/relay.rs:428-516,1960-1987`. Checkpoint export of the assessment record: `mj-controller/src/controller/checkpoint/latched.rs:490-520`; restore `mj-worker/src/relay.rs:492-516`; the assessment checkpoint is strict and limited to 256 KiB (`mj-core/src/assessment.rs:760-792`).

Daemon. `mj-controller/src/daemon/attention.rs:20-163` is the single publication point: it recomputes affected sessions and ancestors and publishes attention, unread frontiers, dismissals and form holders into RuntimeMetadata. Form ownership and review questions `:369-458`; `semantic_question` and `request_hold` `:174-264`; Important `:521-535`; dismissal targets `:291-352`. NotificationDismissal is in `mj-core/src/attention.rs:27-34`; the dismissal action is `mj-controller/src/daemon/actions.rs:1448-1503`.

Clients. The TUI reads RuntimeMetadata (`mj-client/src/runtime_feed.rs:326` onwards; pollers `mj-controller/src/pollers/runtime_feed.rs:27-50`; dashboard drain `mj-cli/src/dashboard/drains.rs:462-465`). The web reads ViewerSession (`mj-controller/src/server/viewer_types.rs:581`, wire `server/viewer_wire.rs:59-64`, dirty tracking `server_runtime/publication.rs:90-126,213-230`). The daemon protocol is `PROTOCOL_VERSION` (105) in `mj-client/src/daemon.rs:3693`; a Rust client refuses a daemon with a different protocol, so old Rust clients do not stay connected across this bump. The TUI pane is `mj-tui/src/notifications_pane.rs` (entries `:340-393`, one-item read `:449-469`); its message text comes from `mj-tui/src/notify.rs:236-300` and the shared rule `mj-core/src/notification_message.rs`. The web screen is `inboxEntries`/`inboxCard`/`renderInbox` in `viewer.js`; its text comes from `GET /api/sessions/{id}/notification-message` (`mj-controller/src/server/handlers.rs:165-308`, `database/materialized.rs:330-480`).

Push. `mj-controller/src/web_push.rs:221-392` (eligibility, episodes, body, presence and pause on unavailable workers); storage `mj-controller/src/database/web_push.sql` and `database/web_push.rs:90-267`; the service worker tags pushes per session.


## Plan of Work

Slice A, backbone (worker, core, daemon, client protocol). First the contract, as its own commit that compiles:

    // mj-core/src/attention.rs (beside NotificationKind)
    pub struct NotificationStackEntry {
        pub completed_ordinal: u64,
        pub kind: NotificationKind,          // NeedsInput or Important
        pub message: EntryMessage,
    }
    pub enum EntryMessage {
        Locating,                            // provisional; never pushed
        Found(String),                       // stable transcript message id
        None,                                // the turn left no message
    }
    pub struct NotificationStack {
        pub revision: u64,                   // relay ordinal of the last mutation
        pub entries: Vec<NotificationStackEntry>, // oldest first
    }
    // NotificationDismissal gains Stack { revision: u64 }.

    // mj-client/src/runtime_feed.rs, in RuntimeMetadata
    #[serde(default)]
    pub notification_cards: Option<SnapshotMap<String, NotificationCard>>, // None: older daemon
    pub struct NotificationCard {
        pub badge: NotificationKind,
        pub entries: Vec<NotificationStackEntry>,   // visible entries, oldest first
        pub dismissal: Option<NotificationDismissal>,
        pub holder: Option<String>,          // form holder the card opens and replies to
        pub form: Option<CardForm>,          // a live person-owned form, with its request id
    }

The exact field names may change while implementing; record any change here. Then the worker: add the stack to RelaySnapshot and RelayOperationalState, with the reducer transitions in the Decision Log, the ClearNotifications and NotificationsRead commands, relay protocol 71 (`RELAY_NOTIFICATION_STACK_PROTOCOL`; upstream took 69 for held logins and 70 for authentication retry intent) and relay state 27. An older snapshot seeds the stack from its raised open request; no history is invented. The question locator always records a final result. The stack travels through checkpoint export and restore as its own optional record, outside the 256 KiB assessment budget. Then the daemon: compute the card map at the attention publication by the visibility rule, set the glyph from the badge, publish `notification_cards` in RuntimeMetadata and in ViewerSession, translate a Stack dismissal into ClearNotifications plus the read-through, send NotificationsRead when the cursor passes an entry and on reattach, build the single-entry card for older workers, and bump the daemon protocol to 106 with `NOTIFICATION_STACK_PROTOCOL` beside PROTOCOL_VERSION (STORED_TRANSCRIPT_READ_PROTOCOL stays 105). Extend the notification-message endpoint to take an entry (message id or completion) so the web can load any entry's text; bound text for an entry without a message id by its completion, as f8cc32025 does.

Slice B, push, after milestone 1: the new tables and compatible migration (revision 138, with a short compatibility reason beside it), reconcile from the card map, identities and ordering as decided, the cutover baseline, and removal of the old episode logic from the new daemon. Keep presence, the pause on unavailable workers and the accepted-delivery dedup.

Slice C, surfaces, after milestone 1: in the TUI pane and the web screen, a card shows its newest entry by default, Previous and Next at the right of the card text (apart from the action buttons below), and "k of n". Each entry's text loads asynchronously by entry identity (TUI: StoredTranscriptItem for a message id, the shared rule otherwise; web: the extended endpoint). Clear sends the card's dismissal and acts on the whole stack. Clear All acts on cards whose badge is `!`. Skip, Quick reply and Go to Session keep their current behavior, keyed by the card's holder. Both surfaces read only `notification_cards` and stop deriving eligibility themselves.


## Concrete Steps

Each slice works in its own detached worktree under `~/Projects/mjolnir-wt/<slice>` created from master (`git worktree add --detach`), edits and builds only there, commits on its detached HEAD, and never pushes. Slice A writes an empty marker file `contract-ready` in its report directory when milestone 1 compiles and is committed, then continues without ending its turn. Slices B and C start from that commit. The parent integrates by cherry-picking or merging onto master. Schema and protocol numbers are fixed at integration in landing order; slices do not renumber on their own.

While iterating, run only the affected tests with `cargo test -p <crate> <filter>`, outside the sandbox. Before handing back, run the boundary suites a slice touches: A runs mj-core, mj-worker, mj-client and mj-controller (daemon, attention, checkpoint and relay filters) and clippy on them; B runs mj-controller web_push and database tests and clippy; C runs mj-tui and the CLI dashboard tests, `node --test tests/e2e/web/viewer*.unit.test.mjs`, and `cargo test -p brokk-mj-controller viewer`. Wait for builds and tests in the foreground; do not end a turn while one runs.


## Validation and Acceptance

Tests follow the AGENTS.md test kinds; mark nothing as hard-won unless it pins a defect found after the code was believed done.

- Worker (tricky conditions): append on `?` and `!`; a repeated assessment for the same completion does not append; clearing by person admission, answered elicitation, DismissQuestion, reset and ClearNotifications; a mailbox wake leaves the stack; a stale ClearNotifications revision is refused; a late assessment or locator result after a person message is fenced; NotificationsRead is monotone and idempotent; journal replay rebuilds the stack; an older snapshot seeds it; checkpoint export and restore keep it.
- Daemon: `!` survives a later Working turn and a later ordinary verdict; the read cursor hides `!` but not `?`; a held `?` is not visible and becomes visible when the hold ends; a sub-agent and a session with notifications ignored have no card; a child's form appears on its parent's card with the child as holder; an older worker gets the single-entry card.
- Integration: the published card map across the daemon and mj-client at protocol 106; the viewer wire field; the schema migration against the data-access code and an older revision.
- Push: one push per entry across a `?` → Working → `?` cycle and across a daemon restart; no push for a sub-agent; a `?` entry waits for its locator; oldest-first ordering; the cutover baseline sends nothing for entries visible at migration.
- Surfaces: Previous, Next, "k of n", newest shown by default, Clear removing the whole stack; extend the existing TUI pane golden and the web screen tests rather than adding a family of examples.


## Idempotence and Recovery

The stack is rebuilt by journal replay. ClearNotifications is keyed by its revision, and NotificationsRead is monotone, so repeating either is harmless. Push identities are durable, and accepted deliveries are retained, so a restart cannot push an entry twice except when the remote accepted a send whose local acknowledgement was lost, as today.


## Artifacts and Notes

Live-store evidence for push, 2026-11-17: 5992a212 had 1,611 pushes and 775 distinct bodies; 41 managed sub-agent sessions received 51 pushes.


## Interfaces and Dependencies

No new dependencies. New constants: `RELAY_NOTIFICATION_STACK_PROTOCOL` in `mj-core/src/relay.rs`, `NOTIFICATION_STACK_PROTOCOL` beside PROTOCOL_VERSION in `mj-client/src/daemon.rs`. New types: `NotificationStackEntry`, `EntryMessage`, `NotificationStack` in `mj-core/src/attention.rs`; `NotificationCard` in `mj-client/src/runtime_feed.rs`; `NotificationDismissal::Stack`.
