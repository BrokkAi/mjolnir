# Answer session forms in the notification pane

## Purpose and acceptance

A user can answer an explicit agent form directly in its notification card. The form has the same values, focus, validation, draft persistence, and response dispatch as the session conversation. When both views are visible, mouse interaction works independently in both rectangles. A child-held card uses the child's chat. Card navigation and Go to Session remain available, and loading or failed background attachment is reported in the card.

## Progress

- [x] Trace existing form rendering, input dispatch, and background attachment.
- [x] Obtain parent approval for separate presentation geometry and shared session attachment ownership.
- [x] Implement geometry slots, notification rendering/input routing, and session-keyed attachment supervision.
- [x] Complete targeted form, notification, golden, and attachment regression checks.
- [x] Run affected-crate clippy and formatting checks and prepare the completion commit.
- [ ] Obtain the required review of the completion commit before handback (no push).

## Context and design

The repository root contains three affected crates. mj-chat/src/chat/elicitation.rs owns the dialog's request, values, answer construction, and logical focus. Its renderer and handlers are reused by ActiveChat. mj-tui/src/notifications_pane.rs owns card selection and controls, and combined.rs draws both support panes and conversations. mj-cli/src/dashboard is the supervisor that prepares ActiveChat off the event loop, restores drafts, and retains loaded chats.

One dialog retains the logical state. ElicitationView slots hold screen-local Form registrations, pointer capture, bounds, and scroll state. Selecting a presentation projects the one logical focus into its control registrations. The new ActiveChat entry points select a geometry slot and call the existing renderer or ordinary event dispatcher; there is no second response implementation.

The supervisor keys attachments by session, and tracks participating pane assignment identities. All surfaces use the existing background prepare and ChatOpened completion path. A current notification form also retains its content session's warm chat. Navigation assignments prevent a queued completion from becoming relevant again after unobserved A-to-B-to-A navigation. Notifications request a warm chat using published card identity; only ActiveChat determines the actual current form.

## Milestones

First, split presentation state from logical form state and expose a small ActiveChat interface. Verify with the existing elicitation tests and a same-frame two-presentation regression that checks pointer capture, hitboxes, shared values, and independent scrolling.

Second, render the chat-owned form in the notification body and route focused keyboard, mouse, and paste events through the normal chat dispatcher. Extend the existing combined dashboard golden and promoted-child notification test, observing a response at the session backend boundary.

Third, generalize the attachment supervisor without duplicating preparation or draft restoration. Keep generation and pane-assignment regressions intact; verify attachment tests and the lifecycle-retirement regression. Complete clippy on all three affected crates and commit the validated change.

## Validation

Use dev-profile commands from the repository root, through the normal cargo/mbx configuration, without changing target directories. Cargo package names are brokk-mj-chat, brokk-mj-tui, and brokk-mjolnir (directory names are not package selectors). Run cargo test outside the sandbox. Existing tests use isolated fakes or isolated storage; do not start a daemon against live data.

Run cargo test -p brokk-mj-chat elicitation. Run cargo test -p brokk-mj-tui notifications_pane and cargo test -p brokk-mj-tui golden_combined_dashboard_render. Update the combined golden deliberately using MJ_UPDATE_GOLDEN=1, inspect it, and rerun without that variable. Run cargo test -p brokk-mjolnir attachment and the lifecycle-retirement regression as needed. Finally run cargo clippy -p brokk-mj-chat -p brokk-mj-tui -p brokk-mjolnir --all-targets -- -D warnings and cargo fmt --all -- --check. Save command logs in the assigned .mj/agents report directory.

## Surprises & Discoveries

The original renderer stored only one set of hitboxes. Rendering twice would make the second view overwrite the first view's mouse geometry. The parent explicitly approved separating presentation slots while retaining one logical dialog. The real attachment owner is in mj-cli, rather than the render-only combined.rs. A preexisting regression requires pane assignment identities to survive the shared attachment refactor.

## Decision Log

The parent approved the geometry split and supervisor generalization after the structural investigation. Use the existing Form widget in each presentation as its geometry and pointer projection, while the dialog owns the authoritative focus. Keep existing draft fields backward compatible. Optional, defaulted surface and viewport fields preserve both presentations independently of render order; older drafts default to Conversation and older readers ignore the added fields. Key background attachment work by session, with participating pane assignment identities and the currently requested notification as relevance checks.

## Outcomes & Retrospective

Implementation and validation are complete. The form uses one logical dialog and normal response dispatcher, with separate geometry for the two presentations. Attachments are shared by session and retain navigation-assignment protection. The earlier-message reader no longer steals notification input; accepted clipboard answers retain their request identity across presentation redraws. Targeted form tests (32), question tests (16), attachment tests (8), lifecycle retirement (1), the combined golden, and notification/clipboard regressions pass. All-targets clippy passed on the three affected crates and the final changed UI crates; formatting and diff checks passed. Initial moved-field compile mistakes, an obsolete Enter-to-open expectation, and a missing scope in the added clipboard fixture were corrected and the affected tests rerun. The initial completion commit a882023b6 was reviewed. Follow-up fixes preserve surface-specific cancellation, reject stale navigation generations through reconciliation, accept mixed form/message cards, reserve readable form height, expose rejection errors in the dialog, restore scroll to its saved surface, and register inline message/body selection. The request-order finding was declined because the parent explicitly requires displaying whichever form ActiveChat owns, rather than selecting another request from published metadata. Follow-up targeted form tests (32), attachment tests (9), notification regressions (four passing together and the mixed-card assertion passing after its fixture selection was corrected), the expanded 31/39-row golden, all-targets clippy on all three crates, and formatting pass. The follow-up commit is prepared for another required review.


## Second review follow-up

All second-review findings are fixed. Notification controls and pointer capture take precedence over text selection using their own geometry; frame reset invalidates form input even on the resize-only screen, and a newly replaced dialog cannot consume input before being rendered. Pending-attach Esc yields to the visible focused form. The attachment owner uses the same requested/cancelled surface interests for completion and reconciliation, preventing a cancelled pane from reopening when its notification releases the last interest. Drafts retain both viewport snapshots regardless of which presentation rendered last. Failed responses label only the matching request, while the transcript preserves failure notices. Notification readable height is now a preference above the structural minimum; constrained forms keep question and control rows before the button bar.

Targeted evidence: 33 elicitation tests, six notification tests, 13 combined-layout tests, focused attachment-cancellation and Esc-priority tests, expanded 21/25/31/39-row golden, simultaneous-presentation viewport roundtrip, stale-response rejection, and hidden/unrendered-dialog input regressions. The compact renderer's 31 dialog tests, updated golden replay, and focused input regression pass. All-targets clippy on the three affected crates and formatting pass. See the report-directory validation record for command logs. This review-fix checkpoint awaits a new review; no push is authorized.


## Third review follow-up

The remaining shared-chat transitions now consult the attachment owner's pane interest for render access, input access, completion acceptance, and retirement. Closing a pinned pane no longer removes a chat retained by Notifications. A notification-only completion remains warm without reopening the cancelled conversation; the renderer receives a projection of the owner's access decision rather than recomputing cancellation. Native harness forms retain quick reply: CardForm centralizes its existing reserved native request identity namespace, with no wire-format change, and the daemon uses the same constructor. Loading/failure cards use the space released by the hidden quick-reply row and place the status before long request text so compact panes show the result.

Extend the existing shared-cancellation regression through completion, actual rendering, and pane closure. Extend the published-card input regression with a native quick reply through the existing dispatcher, and extend the dashboard golden with eight-row loading/failure cards. Verify the daemon native-card projection regression, attachment filters, golden replay, and all-targets clippy on all affected crates (including mj-client and mj-daemon).

Third follow-up validation passed: the extended cancellation and native-reply tests, 28 attachment-filter tests, daemon native projection, compact-status golden replay, all-targets clippy across five affected crates, and formatting/diff checks. This checkpoint is ready for mandatory review, without pushing.


## Fourth review follow-up

Decline the minimized-height finding: notification_form_session returns None for Minimized using the same field pane_size reads, so the form minimum override cannot run in that state. The required review override was recorded before editing or committing.

Fix four confirmed transitions. Navigation leaves the composer and its clipboard channel with a chat retained by the visible notification form. Conversation form capture is found across all shown sessions and wins before notification hit-testing, selection, and dispatch, so release reaches its owner even over another session's card. A rendered form with any interior rows accepts input even when the message itself has no row. Inline selection uses the chat-owned message text and width to change the notification generation when loading becomes a form or the form page changes.

Extend the existing interaction regression with a one-row text editor and two-session capture/release. Extend the existing text-selection regression through loading, attachment, and question-page changes. Validate these focused transitions, the unchanged golden, clipboard regressions, and affected-crate clippy.

Fourth follow-up checks pass: extended compact-input and cross-session-capture regression, loading/page selection invalidation, existing clipboard target regression, unchanged dashboard golden, all-targets clippy with the same five-package selection, and formatting. Fixture-only failures (field id spelling and click/search coordinates) were corrected and only the affected tests rerun. The checkpoint awaits mandatory review; no push.


## Fifth review follow-up

Fix all six findings. The shared lifecycle input decision now gates both conversation and notification input; lifecycle-blocked form renders and input also disarm pending pointers. Global commands cancel capture on every warm chat, including a different session's inline form. Attachment acceptance requires an explicitly bound current pane interest, and reconciliation cannot adopt old work into a previously unbound assignment after all original interests disappeared. Copying a notification selection validates its generation after the redraw, using the same row-space validation as plain notification text. Keyboard ownership uses the dialog's actual rendered-interior eligibility, so border-only forms do not swallow attachment-cancel Esc. Compact fieldless and confirmation forms reserve their clickable action row and use remaining space for the question.

Extend the existing interaction regression through Moving, border-only rendering, and global-command capture cancellation. Extend the layout-restore regression to reject a queued completion both before and after binding a previously unobserved destination pane. Extend existing fieldless/confirmation tests through compact mouse submission and the selection regression through post-redraw generation checks. Targeted input, fieldless, confirmation, layout-restore and selection tests pass; the broader attachment filter (28 tests) and unchanged dashboard golden pass. All-targets clippy and formatting are the final checks for this checkpoint.

Fifth follow-up all-targets clippy across the same five affected packages, formatting and diff checks pass. Ready for required review; no push.


## Sixth review follow-up

Fix all three findings. CardForm's shared namespace owner now distinguishes completed review findings, which retain quick reply and Enter-to-open instead of requesting an elicitation. The daemon uses the shared findings constructor without changing its generated ID or serialized fields. Lifecycle-owned conversations no longer compete for mouse capture, and every render disarms their warm chat before drawing the lifecycle view. Reconciliation prunes departed pane assignments and removes session attachment records once neither the layout nor Notifications references them; current cancelled assignments remain as the bounded state needed to preserve Esc semantics. Completion generations are process-unique across replacement records, so record retirement cannot make an old queued reply match a later visit to the same session.

Extend the existing card input regression through a findings verdict, the interaction regression through a captured conversation entering Moving, and the layout-restore regression through record retirement/recreation and rejection of its old completion. Run the existing daemon native/review projection regression, attachment filters, dashboard golden replay, all-targets clippy and formatting before committing.

Sixth follow-up validation passes: extended verdict/input/retirement regressions, 28 attachment tests, daemon native/review projection, unchanged dashboard golden, five-crate all-targets clippy, formatting and diff checks. Ready for mandatory review; no push.


## Seventh review follow-up

Fix all four findings. Inline focus uses the dashboard's shared pointer-press reset for pending chords and resize mode. Dashboard capture is checked before conversation or notification routing so drag/release returns to the pressed dashboard control. Accepted clipboard work retains its logical form chat and composer even when navigation releases the notification surface; after the result applies, normal retirement captures the updated form draft. This is the narrow accepted-input lifetime needed to preserve work, rather than a second clipboard path or a retention timeout. The notification presentation records which selection revision it rendered; the input loop refreshes a newly revealed form before handling another queued key or paste. Item changes invalidate the old form rectangle immediately.

Extend the existing input regression through prefix+click+q, resize-mode reset, and a dashboard-button drag into the inline form. Extend card navigation through the renderer-readiness transition and the existing clipboard regression through pending-input ownership and completion. These focused tests, the 28-test attachment filter, unchanged golden replay, five-crate all-targets clippy, formatting and diff checks pass. No tests failed in this follow-up. Ready for required review; no push.


## Eighth review follow-up

Fix all four findings. Conversation capture now has one chat-owned query covering visible form controls and transcript scrollbars; notification arbitration and the host use the same owner. Terminal focus loss broadcasts cancellation across every warm chat, and scrollbar cancellation preserves the displayed geometry so the next click still works before redraw. Background form attachment reports success/failure only in the card; shared opening/retry notices are changed only for a current requested conversation. Live layout and maximize probes construct their bands with the same notification form reservation and actual stacked-Sessions minimum.

Extend the existing interaction regression through a real transcript-thumb drag into another session's form, terminal focus loss, and immediate recapture without redraw. Extend the existing notification regression through an unrelated failure surviving both failure and success of a background attach. Extend the 120x21 golden's state assertions to reject ineffective Profiles/Notifications maximize controls, update expected output deliberately, and replay it. Run targeted pointer/scrollbar tests and affected-crate clippy before committing.

Eighth follow-up checks pass: extended scrollbar/focus-loss and notice regressions, both existing capture tests, 28 attachment tests, golden replay with three ineffective-control removals, all-targets five-package clippy, formatting and diff checks. Ready for required review; no push.


## Ninth review follow-up

Fix all four findings. The notification geometry slot releases capture when its card item or pane mode changes or it stops being displayed. Unrendered geometry cannot claim notification input and stale presses are cancelled before that geometry returns, while the conversation's slot remains independent. Presentation identity includes pane mode so expanding from Minimized refreshes input ownership before queued input.

Reviewer responses now carry the existing logical dialog draft through the normal ChatAction dispatcher and existing reviewer response channel. Synchronous routing refusals and asynchronous lookup/answer failures restore the matching reviewer dialog with its values and inline error; unrelated/newer forms are not relabeled. Automatic reviewer plan decisions and existing late-answer notices retain their previous path. The I/O callback boxes the draft to keep ordinary messages small; no serialized format changes are needed.

Cards with forms render persistent dismissal failures in their outer border without taking answer rows. The form-content-session helper owns selected-form classification independently of minimized display interest, allowing maximize probes to measure an existing warm form without warming a minimized card.

Extended reviewer failure/draft, item navigation/held-click, hidden-frame capture, and simultaneous-surface regressions pass. Expanded goldens cover a failed dismissal beside a form and a minimized warm long-choice form whose Maximize gains rows. Golden replay, existing late-answer and reviewer-removal tests, normal inline input, all-targets five-package clippy, formatting and diff checks pass. Mechanical corrections were draft equality derivation, boxing the callback payload and its fixtures, and using the direct key constructor in the golden. Ready for mandatory review; no push.

## Tenth review follow-up

Fix all five findings. Reviewer answers now enter the existing remote-operation supervisor instead of spawning a parallel callback task. The supervisor's pending-answer feedback retains the logical chat until the result applies, including after Skip or Minimize, and its existing composer-result transfer carries delivery through a chat handoff. In-flight answers suppress resurfacing the same reviewer question. Failed draft restoration consults the authoritative current review projection; ended questions remain closed and retain the existing late-answer notice behavior.

Card forms now publish explicit provenance (elicitation, native request, or completed findings), so harness request IDs remain opaque. Missing provenance and missing card metadata retain legacy quick reply and Enter-to-open behavior instead of guessing that every undismissible waiting card is a form. Existing typed form fixtures now publish the same source the daemon supplies. The additive runtime field defaults to Unknown when absent and does not change stored data or require a migration.

Opening the earlier-message reader cancels the conversation's pointer ownership before it hides the transcript. The notification geometry slot remains independent. Extend the existing reviewer failure/draft, pointer/history, legacy-card, and daemon-to-client projection regressions; run the affected dashboard goldens, push-card compatibility case, all-targets clippy for affected packages, formatting and diff checks before committing.

Tenth follow-up validation passes: reviewer answer/draft retention and ended-request restoration, cross-surface history capture, notification navigation/input/selection, legacy quick reply, opaque-ID daemon/client projection, existing reviewer removal, push-card retry/restart compatibility, and both dashboard golden replays. All-targets clippy passes for mj-chat, mj-tui, mj-cli, mj-client, mj-daemon and mj-store. Mechanical fixes were ignoring the refusal helper's boolean result, removing obsolete callback variants and cloning a golden request still needed by its published card. The late-answer assertion now checks the conversation-notice publication rather than a phrase split by terminal wrapping. Ready for mandatory review; no push.

## Eleventh review follow-up

Fix all three findings through response ownership. Pending primary response feedback now retains the ActiveChat just as pending reviewer answers do. Projection-dependent form results remain intact in a composer-owned deferred result queue while the composer is parked; that queue transfers with LiveComposer. A new live chat distinguishes an unknown review projection from an authoritative absent review, so it does not discard a reviewer result before the host supplies the latest review. The existing reviewer surfacing transition drains deferred results once the live chat can decide their ownership.

When another pending form owns focus, a rejected reviewer answer retains its submitted draft and error as the same remote result. Resolving the visible question lets the normal surfacing path restore the failed answer's values and inline feedback. Review removal still invalidates that result instead of reviving an ended question. No second response dispatcher, timer, or alternate projection is introduced.

Extend the existing reviewer regression through A in flight, B visible, A rejected, B resolved; then through Move-style composer parking, failure delivery, and adoption before/after the authoritative review arrives. Extend the notification input regression through minimizing immediately after a primary response and expanding after rejection. Run these cases, existing parked prompt/refusal and reviewer-removal regressions, affected-crate clippy, formatting and diff checks before committing.

The existing draft format also carries optional form feedback, with a serde default for older snapshots and omission when absent. This preserves a primary rejection through retirement after its callback, instead of fixing only the in-flight interval. The notification regression serializes the retired draft and restores it onto a new chat before checking the card's inline rejection. Deferred reviewer failures are logged only when recovery can apply, so waiting behind another question does not repeatedly report the same failure.

Eleventh follow-up validation passes: expanded reviewer A/B and parked-result transfer regression, primary minimize/rejection/serialized draft restoration, 33 elicitation tests, existing parked prompt refusal and reviewer-removal cases, final focused reviewer replay, all-targets six-package clippy, formatting and diff checks. No tests failed in this round. Ready for mandatory review; no push.

## Twelfth review follow-up

Fix all four findings. Primary rejection recovery now checks the authoritative pending request list before restoring a dialog. Clipboard targets carry the full logical request identity, including reviewer routing, and remain independent of presentation focus. Deferred reviewer failures are exported through the existing serialized question-draft snapshots, with their values and inline feedback; restoration retains multiple distinct deferred drafts instead of overwriting the previous one. Compact single-row bodies place the independently scrollable question beside their answer control or action row, so accepting an answer no longer hides the request.

Extend the existing late-primary-result, clipboard staleness, reviewer upgrade-draft and compact notification input regressions. Validate the affected elicitation module and notification/dashboard render cases, clippy and formatting before committing. No push.

Twelfth follow-up validation passes: expired primary recovery, full-identity clipboard delivery, deferred reviewer upgrade snapshots, compact form input, the affected elicitation cases, elicitation golden and updated dashboard golden replay, all-targets clippy and formatting. Initial failures were fixture scope/text corrections; no outstanding test failures. All four latest findings are fixed; mandatory review pending, no push.

## Thirteenth review follow-up

Fix the five remaining findings. Draft restoration preserves reviewer snapshots until the first authoritative review view arrives, and restores them immediately when their form is surfaced. Upgrade capture gathers rejected-answer snapshots from parked composers before removing them. Message selection identities use the actual rendered message width across compact rewraps. Primary answer errors also verify the dialog's harness source, keeping identical reviewer payloads untouched. Extended recovery/order, parked snapshot, selection resize, and harness-isolation regressions cover these transitions. The parent will perform integrated review and decide on pushing after this final commit.

Final validation for the fourteenth commit passes: reviewer recovery/order and upgrade snapshots, primary/reviewer error isolation and expired primary recovery, compact selection invalidation, three existing terminal-upgrade tests, all-targets clippy, formatting and diff checks. A combined fixture initially retained a reviewer form into its independent primary-only scenario; resetting that scenario fixed the assertion, and only the failing test was rerun. No known unfixed findings from the latest reviewed commit remain; final integrated acceptance belongs to the parent. No push.
