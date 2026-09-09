# Add one consistent close control to terminal dialogs

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. This file is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Every dashboard and chat dialog should expose the same obvious dismissal affordance: a multiplication-sign glyph (`×`) on the upper-left border. A mouse click on that glyph and the Escape key must invoke the same close or cancel transition. People should no longer need to discover inconsistent footer “Close” buttons or remember when Escape closes versus navigates backward. Explicit Back buttons remain the way to navigate within a workflow.

The behavior is observable by opening representative dashboard dialogs such as Help, the command palette, Setup, Web access, or the workspace manager, and chat dialogs such as the configuration picker, reviewer setup, an elicitation question, or the background-task list. Each has the same upper-left glyph. Escape and a glyph click produce the same resulting state. Dirty F7 Setup and an active import ask for confirmation; committed work that cannot safely be interrupted temporarily disables dismissal.

## Progress

- [x] (2026-09-09 11:17Z) Explored the dashboard and chat modal renderers, shared `Form` interaction layer, and existing cancel actions.
- [x] (2026-09-09 11:17Z) Settled product behavior with the user: exit the whole workflow, protect only dirty F7 Setup, and confirm only import cancellation.
- [x] (2026-09-09 11:28Z) Added and focused-tested the shared non-focusable dismiss interaction and modal-title renderer.
- [x] (2026-09-09 12:21Z) Adopted the shared control across dashboard dialogs and routed Escape and pointer dismissal through the same transitions.
- [x] (2026-09-09 12:21Z) Adopted the shared control across chat popups without changing autocomplete or full-page panes.
- [x] (2026-09-09 12:21Z) Removed Close-only controls while retaining Cancel, Back, No, and action-specific controls.
- [x] (2026-09-09 12:21Z) Passed formatting, focused crate tests, the full serialized Rust test suite, and strict workspace Clippy.
- [x] (2026-09-09 12:21Z) Finalized the spacing fix and ExecPlan for the integration commit and configured-upstream push.

## Surprises & Discoveries

- Observation: `Form<K>` currently uses `Option<K>` as its mouse gesture owner, so reusing an existing Cancel control identifier for the title glyph would overwrite the footer button's hitbox.
  Evidence: `mj-chat/src/components/scope.rs` stores one `Rect` per `Control<K>` and `register` updates that rectangle.

- Observation: the background-task popup is a read-only viewer and exposes no operation-cancellation action.
  Evidence: `render_background_task_dialog` in `mj-chat/src/hel_chat/active.rs` clones and renders `session_activity().background_commands`; closing only clears the popup-open state.

- Observation: import progress must be found through a dismissal confirmation as well as through Help, otherwise progress displayed after rejecting cancellation would be stale.
  Evidence: `import_progress_mut` in `mj-tui/src/dialogs.rs` recursively follows both wrapper modes to the preserved `ImportProgress`.

- Observation: preserving the old trailing title margin is behaviorally significant in the PTY snapshots; omitting it joined the title to the border as `target─`.
  Evidence: `disabled_startup_waits_for_explicit_new_before_creating_a_session` failed until `dismissible_modal_title` restored the final styled space.

- Observation: the upstream updater fixture races with itself under parallel workspace testing on this host and can fail with `ETXTBSY`; it passes alone and in the full serialized suite.
  Evidence: the parallel `cargo test` attempts failed only in `npm_upgrade_restarts_after_the_running_package_is_removed`; the exact test and `cargo test -- --test-threads=1` passed.

## Decision Log

- Decision: The glyph is a separate, non-focusable semantic dismiss target owned by `Form`, not another member added to every dialog's control enum.
  Rationale: This preserves one shared implementation, allows footer Cancel buttons to coexist, and keeps the glyph out of the Tab order because Escape is its keyboard equivalent.
  Date/Author: 2026-09-09 / Codex

- Decision: Escape and `×` exit the entire active workflow rather than performing contextual Back navigation. Confirmation dialogs are the safe exception: dismissing a confirmation rejects the proposed cancellation and restores the underlying dialog.
  Rationale: The user selected whole-workflow dismissal and wants the two inputs to be behaviorally identical. Explicit Back buttons already provide nested navigation.
  Date/Author: 2026-09-09 / user and Codex

- Decision: Prompt only when changed F7 Setup values would be discarded or an active import would be canceled. Agent work, discovery, target tests, web loading, and ordinary wizard edits dismiss immediately. Non-cancelable committed mutations disable `×` and ignore Escape.
  Rationale: This is the user's chosen boundary between useful protection and confirmation fatigue.
  Date/Author: 2026-09-09 / user and Codex

- Decision: Remove only controls whose sole visible label and behavior are Close. Keep Cancel, Back, No, and domain actions.
  Rationale: Cancel communicates data loss or operation cancellation, while Close becomes redundant once every dialog has the standard glyph.
  Date/Author: 2026-09-09 / user and Codex

- Decision: Push the completed commit to the current branch's upstream after validation.
  Rationale: The user explicitly requested a push while implementation was in progress.
  Date/Author: 2026-09-09 / user

## Outcomes & Retrospective

The dashboard and chat modal families now share one upper-left `×` title control backed by `Form`. Glyph clicks and Escape take the same semantic path, Close-only footer buttons are gone, and explicit Back/Cancel/domain actions remain. Dirty Setup and active imports use safe-default confirmation dialogs; committed Setup, workspace, and bundle mutations disable dismissal. The chat configuration picker, reviewer setup, elicitation, and background-task viewer use the same interaction without broadening the scope to autocomplete or full-page panes.

Validation passed after merging the upstream controller/client split: `cargo fmt --check`; `cargo test -p brokk-mj-tui` (408 passed, 2 ignored before the upstream merge); the exact PTY spacing regression test; `cargo test -- --test-threads=1` across the full merged workspace; `cargo clippy --all-targets -- -D warnings`; and `git diff --check`. The parallel full suite exposed only the pre-existing updater-fixture `ETXTBSY` race documented above, so the authoritative full run was serialized. No product limitation remains.

## Context and Orientation

The repository is a Rust workspace. `mj-tui` owns the dashboard state machine and most dashboard dialog rendering. Its `Mode` enum in `mj-tui/src/lib.rs` identifies the active dashboard surface, including new/resume wizards, configuration editors, Web access, workspace management, import progress and confirmations, Help, the command palette, and F7 Setup. Their renderers live primarily in `mj-tui/src/dialogs.rs`, `mj-tui/src/setup.rs`, `mj-tui/src/wizards.rs`, `mj-tui/src/workspaces.rs`, and nearby modules.

`mj-chat` owns reusable terminal components and chat-specific popups. `mj-chat/src/components/scope.rs` defines `Form<K>`, which keeps stable control identities, focus, exact mouse hitboxes, and press/release ownership. `Interaction::Cancel` is already the common result of Escape. `mj-chat/src/hel_modal.rs` centralizes modal placement and is the appropriate home for shared title-border rendering. Chat popup behavior and rendering are in `mj-chat/src/hel_chat/`, notably `config_picker.rs`, `second_opinion.rs`, `elicitation.rs`, and `active.rs`.

A “dismiss target” in this plan means the mouse-only hitbox for the upper-left `×`. It is not a Tab stop. “Armed” means the pointer was pressed on the target and has not been dragged away before release. A “committed mutation” means work such as saving Setup, mutating a workspace, or creating a bundle that has started and has no safe cancellation operation.

## Plan of Work

First, extend `Form<K>` in `mj-chat/src/components/scope.rs`. Replace the control-only pointer owner with an internal owner that can represent either a regular control or the dismiss target. Store the current dismiss rectangle and enabled state independently of `K`. Add public methods named `register_dismiss(area: Rect, enabled: bool)` and `dismiss_is_armed() -> bool`. Registration occurs during rendering, participates in `contains` and pointer capture, and is cleared or invalidated by the same frame/reset lifecycle as ordinary hitboxes. Pressing and releasing within an enabled target returns `Interaction::Cancel`; dragging outside cancels activation. The target never joins `order`, the focus tree, or keyboard traversal. Escape remains the keyboard source of `Interaction::Cancel` and must be ignored by a dialog before calling the form when that dialog has locked dismissal.

Then add a modal title helper in `mj-chat/src/hel_modal.rs`, re-exported through `mj-tui/src/widgets.rs`. Name it `dismissible_modal_title`. It accepts the active form, popup rectangle, title text/style, and enabled state; registers the three-cell ` × ` span on the popup's top border and returns a Ratatui `Line` suitable for `Block::title`. It styles the glyph with the existing button palette: normal when available, focused/active while armed, and muted while disabled. It places the normal title immediately after the glyph without overwriting border cells or changing the modal body. Existing error or warning title styling remains on the title portion.

Adopt the helper in every dashboard modal. Remove control enum members, declarations, event arms, and renders used only for visible Close buttons. Keep footer Cancel and Back controls, but route their activation through the same function used by `Interaction::Cancel`. Change nested modal Escape handling so it closes the entire owning workflow; Back activation alone returns to a parent screen. Existing domain confirmation dialogs continue to treat dismissal as the safe negative response and restore their stored parent when applicable.

Add a dashboard cancellation-confirmation state that owns the underlying `Mode` and a specific intent. One intent discards dirty Setup; the other cancels import. Dirty Setup means the serialized mutable draft differs from the existing `original` snapshot, including changes made in nested review settings; focus, scroll, discovery status, and other transient UI fields do not make it dirty. Dismissing the new confirmation restores the boxed underlying mode. Confirming Setup discard returns to `Mode::Dashboard`; confirming import emits `DashboardAction::CancelImport` and closes the import workflow. The footer Cancel button, Escape, and `×` all enter the same confirmation path. Ordinary wizard and editor drafts are discarded without prompting.

Pass `enabled = false` while Setup is saving, workspace creation/rename/delete/recovery is mutating, or new-bundle creation is in flight. In these states both Escape and glyph interaction are inert. Initial workspace loading, Setup/reviewer discovery, target testing, Web access loading, resume scanning, and similar read-only/cancelable work remain dismissible and emit their existing cancel action when one exists.

Finally, adopt the same title and event behavior in chat popups: configuration picker, second-opinion/reviewer setup, elicitation questions, and the background-task viewer. Closing an elicitation or active review uses its existing cancellation response without adding a confirmation. The background-task viewer only closes the viewer because it exposes no operation-cancellation API. Autocomplete and full-page/split chat surfaces are not modal dialogs and remain unchanged.

## Concrete Steps

Work from `/home/jonathan/Projects/hel4`.

After the shared primitive is implemented, run focused component tests:

    cargo test -p brokk-mj-chat components::scope
    cargo test -p brokk-mj-chat hel_modal

After dashboard and chat adoption, run focused crate suites as useful while iterating:

    cargo test -p brokk-mj-tui
    cargo test -p brokk-mj-chat

Before committing, run the required workspace checks outside the restricted sandbox because the suite exercises local sockets:

    cargo fmt --check
    cargo test
    cargo clippy --all-targets -- -D warnings

The expected result is exit status zero for every command. Review `git diff --check`, `git status --short`, and the scoped diff, then stage only feature files and this ExecPlan and commit on the current branch. Push that commit to the current branch's configured upstream.

## Validation and Acceptance

Shared interaction tests must prove that an enabled dismiss target emits `Interaction::Cancel` only after press and release inside it, that dragging out prevents activation, that a disabled target is inert, that frame/reset operations prevent stale clicks, and that it changes visual state while armed without affecting focus order.

Dashboard behavior tests must prove that Escape and glyph click yield identical `Mode` and `DashboardAction` results for representative dialogs. They must cover a simple close, a nested screen that now exits its whole workflow, dirty and clean Setup, active import confirmation, a cancelable discovery/test/load, and a locked committed mutation. Tests must prove that dismissing a confirmation restores the underlying state and never selects a destructive affirmative response.

Chat tests must prove parity for the configuration picker, reviewer setup, elicitation cancellation, and task viewer. Rendering tests must show `×` at the upper-left, preserve the title, distinguish enabled and disabled states, and demonstrate that Close-only controls are gone while Cancel and Back controls remain. Existing autocomplete behavior must continue to pass unchanged.

Acceptance is complete when a person can open any included popup, see the same upper-left glyph, and observe the same result from clicking it or pressing Escape; dirty Setup and active import protect state with a confirmation; committed non-cancelable work cannot be dismissed; and the full required Rust checks pass.

## Idempotence and Recovery

The edits and tests are safe to repeat. No migrations, network state, or destructive filesystem operations are required. If a partial adoption fails to compile, finish converting each modal's renderer and event handler to the shared helper rather than adding compatibility fallbacks. Preserve unrelated working-tree changes and stage only files changed for this feature.

## Artifacts and Notes

The initial repository state was clean and synchronized with its upstream:

    ## hel4...origin/master

The design deliberately uses the existing `Interaction::Cancel` result. This keeps keyboard and pointer behavior converged at the state-machine boundary instead of maintaining parallel event paths.

## Interfaces and Dependencies

In `mj-chat/src/components/scope.rs`, provide:

    pub fn register_dismiss(&mut self, area: Rect, enabled: bool)
    pub fn dismiss_is_armed(&self) -> bool

In `mj-chat/src/hel_modal.rs`, provide a generic `dismissible_modal_title` helper that accepts `&mut Form<K>`, the popup `Rect`, the displayed title, its style, and the enabled state, registers the dismiss hitbox, and returns the composed `Line`. Re-export it for `mj-tui` callers through `mj-tui/src/widgets.rs`.

No wire protocol, configuration schema, persistent data model, or third-party dependency changes are needed. New dashboard confirmation state is crate-internal and must preserve its underlying `Mode` with indirection so the recursive type has finite size.

Revision note (2026-09-09): Created the implementation plan after repository exploration and user decisions; updated it when the user explicitly requested a push; and completed it with the implemented behavior, upstream-integration notes, and final validation evidence.
