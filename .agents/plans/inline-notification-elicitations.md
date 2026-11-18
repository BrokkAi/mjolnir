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

The parent approved the geometry split and supervisor generalization after the structural investigation. Use the existing Form widget in each presentation as its geometry and pointer projection, while the dialog owns the authoritative focus. Keep existing draft fields backward compatible. An optional, defaulted surface field records which geometry slot supplied saved scroll; older drafts default to Conversation and older readers ignore the added field. Key background attachment work by session, with participating pane assignment identities and the currently requested notification as relevance checks.

## Outcomes & Retrospective

Implementation and validation are complete. The form uses one logical dialog and normal response dispatcher, with separate geometry for the two presentations. Attachments are shared by session and retain navigation-assignment protection. The earlier-message reader no longer steals notification input; accepted clipboard answers retain their request identity across presentation redraws. Targeted form tests (32), question tests (16), attachment tests (8), lifecycle retirement (1), the combined golden, and notification/clipboard regressions pass. All-targets clippy passed on the three affected crates and the final changed UI crates; formatting and diff checks passed. Initial moved-field compile mistakes, an obsolete Enter-to-open expectation, and a missing scope in the added clipboard fixture were corrected and the affected tests rerun. The initial completion commit a882023b6 was reviewed. Follow-up fixes preserve surface-specific cancellation, reject stale navigation generations through reconciliation, accept mixed form/message cards, reserve readable form height, expose rejection errors in the dialog, restore scroll to its saved surface, and register inline message/body selection. The request-order finding was declined because the parent explicitly requires displaying whichever form ActiveChat owns, rather than selecting another request from published metadata. Follow-up targeted form tests (32), attachment tests (9), notification regressions (four passing together and the mixed-card assertion passing after its fixture selection was corrected), the expanded 31/39-row golden, all-targets clippy on all three crates, and formatting pass. The follow-up commit is prepared for another required review.
