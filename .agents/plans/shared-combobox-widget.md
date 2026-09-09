# Extract and reuse the inline combobox widget

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Fixed-choice scalar fields should look and behave consistently wherever they appear. After this change, Setup scalar values and the four selectors in Setup's Code review form show a compact value field with a dropdown glyph, open the same anchored autocomplete-style popup, preview choices with arrow keys, and accept with Tab, Enter, or a mouse click. Screens whose main content is a list of choices remain full-page lists because they are not scalar fields.

## Progress

- [x] (2026-09-09 13:05Z) Audited all `ChoiceList`, `TabStrip`, and autocomplete-popup call sites and classified their interaction roles.
- [x] (2026-09-09 13:27Z) Added a reusable combobox control, interaction semantics, state helper, rendering, and focused component tests in `mj-chat`.
- [x] (2026-09-09 13:31Z) Migrated Setup scalar choices and Code review tier/profile/model/effort selectors to the shared component.
- [x] (2026-09-09 13:34Z) Updated behavior tests for keyboard, mouse, wheel, focus, cancellation, popup rendering, and deferred discovery side effects.
- [x] (2026-09-09 13:46Z) Ran formatting, focused tests, the repository-wide test suite, and clippy. All relevant and library suites pass; one unrelated PTY fixture exceeded its fixed shutdown deadline in the full run and on two isolated retries.

## Surprises & Discoveries

- Observation: The previous compact Setup work shared only popup geometry through `AutocompletePopup`; Setup still owns the field glyph, width calculation, list rendering, and accept/cancel event policy.
  Evidence: `mj-tui/src/setup.rs` manually calls both `AutocompletePopup::render` and `ChoiceList::render`, and intercepts Tab, Enter, Escape, and mouse release.
- Observation: Wizard picker pages are not comboboxes. Their list is the primary page content and can include disabled rows, explanatory copy, navigation buttons, repository ordering, or attachment history.
  Evidence: `mj-tui/src/wizards.rs::render_picker` lays out a titled modal with a full choice list, help region, and Back/Next controls.
- Observation: Setup's Code review editor is the only other fixed-choice form surface. It currently uses `TabStrip` for tier, profile, model, and effort, including potentially long dynamically discovered labels.
  Evidence: `mj-tui/src/review_settings.rs::render_review_settings` renders every entry returned by `selectors()` through `TabStrip::render_enabled`.
- Observation: A popup row clicked before any keyboard preview can differ from the cursor snapshotted when the combobox opened.
  Evidence: The focused Setup test initially left Theme at `midnight` after clicking `Light` because state routing preferred its pending cursor over `Interaction::ComboBoxCommit`'s clicked index. Treating the commit interaction as authoritative fixed the behavior and the test.
- Observation: The repository-wide test run is currently sensitive to a pre-existing PTY fixture's five-second deadline under host load.
  Evidence: `cargo test` passed the library suites and all integration groups until `disabled_startup_waits_for_explicit_new_before_creating_a_session`; that unrelated test reported two in-flight launch operations at shutdown. Two isolated retries failed at different timing points, once before initial output and once waiting for target-preflight cleanup. The combobox-focused suites remained green.

## Decision Log

- Decision: Introduce a real `ComboBox` component and `ComboBoxState` under `mj-chat::components`, backed by a `ControlKind::ComboBox` form control rather than preserving Setup-specific event interception.
  Rationale: Rendering alone is insufficiently reusable. The form layer must consistently distinguish preview selection from commit, consume Escape without dismissing the parent, commit mouse choices, and make Tab accept while expanded.
  Date/Author: 2026-09-09 / Codex
- Decision: Migrate Code review's tier, profile, model, and effort controls, but not wizard picker pages, palettes, workspaces, config search, elicitation, reviewer panes, or tab navigation.
  Rationale: The migrated controls are scalar values embedded in a form. The excluded controls are primary-content lists, filtered search dialogs, multi-select lists, or actual navigation tabs; presenting them as collapsed fields would remove context or change their purpose.
  Date/Author: 2026-09-09 / Codex
- Decision: Keep domain changes deferred until combobox commit.
  Rationale: Profile and model changes can launch background capability discovery. Merely moving the popup highlight must not start expensive work or churn requests.
  Date/Author: 2026-09-09 / Codex

## Outcomes & Retrospective

The shared combobox now owns its affordance, clipping, anchored popup, keyboard preview and acceptance, local dismissal, mouse hitboxes, wheel navigation, and pending cursor. Setup scalar rows and Setup's Code review selectors both use it. Code review applies profile/model side effects only after acceptance, while full-page wizard and content lists remain unchanged. Focused tests and clippy pass; the only full-suite exception is the unrelated PTY timing failure recorded above.

## Context and Orientation

`mj-chat/src/components/scope.rs` defines `Form`, `ControlKind`, and typed `Interaction` events. A `Form` retains focus and pointer state across renders. `mj-chat/src/components/controls.rs` contains visible controls such as `Button`, `ChoiceList`, and `TabStrip`. `mj-chat/src/components/layout.rs` contains `AutocompletePopup`, which chooses and frames a compact rectangle above or below an anchor but does not own field or event behavior.

`mj-tui/src/setup.rs` renders the general Setup tree. Selecting a scalar with schema choices creates an `Editor` and currently draws a choice popup by hand over the selected row. `mj-tui/src/review_settings.rs` renders the nested Code review settings editor and currently expresses tier, profile, model, and effort as horizontal tab strips. Some values are discovered asynchronously; applying profile or model choices can start background discovery, so popup cursor movement must remain local until acceptance.

## Plan of Work

Extend `ControlKind` in `mj-chat/src/components/scope.rs` with a combobox variant carrying the option count, current popup cursor, and whether the popup is expanded. Extend `Interaction` with explicit combobox commit and dismissal events. The form should activate a collapsed combobox, move only its cursor while expanded, commit its current cursor on Enter, Space, or Tab, dismiss only the combobox on Escape, and emit a commit for a clicked popup row. Preserve existing behavior for every other control kind.

Add `ComboBoxState<K>` and `ComboBox` in `mj-chat/src/components/controls.rs`, exporting them from `mj-chat/src/components/mod.rs`. The state stores at most one expanded control and its uncommitted cursor. It opens from a committed index, records preview selections, reports accepted `(control, index)` pairs, closes on local dismissal, and passes unrelated interactions back to the screen. The visible component renders a clipped value with a dropdown glyph when collapsed and reuses `AutocompletePopup` plus `ChoiceList` styling when expanded. It computes popup width from the title and option labels, clips to the supplied bounds, and uses the same maximum-eight-row geometry as chat autocomplete.

In `mj-tui/src/setup.rs`, replace the `Editor` choice cursor and manual key/mouse/render policy with `ComboBoxState` and `ComboBox`. Keep text editors unchanged. The selected Setup row remains part of the surrounding tree list, but the shared component redraws its scalar value field and owns the expanded popup. Use the component's display helper for collapsed dropdown glyphs.

In `mj-tui/src/review_settings.rs`, store one `ComboBoxState<ReviewSettingsFocus>`, declare the four selector controls as comboboxes, and render each with the shared component. Opening a field snapshots its committed selection. Arrow movement updates only combobox state. Commit invokes the existing tier/profile/model/effort domain update paths, including capability discovery only after profile or model acceptance. Local Escape closes the popup without closing Setup. Adjust help text to describe opening and accepting a choice.

Update colocated component and surface tests. Component tests must cover glyph rendering, keyboard preview versus commit, Tab acceptance, local Escape, and mouse-row commit. Setup tests must continue to prove its choice popup opens inline, accepts with keyboard and mouse, and cancels without changing the draft. Code review tests must prove opening a selector does not change its value, arrow preview does not launch discovery, commit applies once, and Escape closes only the combobox.

## Concrete Steps

Run all commands from `/home/jonathan/Projects/hel3`.

First edit the component layer and run:

    cargo test -p brokk-mj-chat components::

Then migrate the two TUI consumers and run focused tests:

    cargo test -p brokk-mj-tui setup::tests
    cargo test -p brokk-mj-tui review_settings::tests

Format and validate the integrated repository:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings

The Cargo tests must run outside the restricted sandbox because they use loopback TCP and Unix sockets. Successful commands exit zero with no failed tests or clippy warnings.

## Validation and Acceptance

In Setup, focus a scalar row marked with the dropdown glyph and press Enter. The surrounding Setup page remains visible and a compact anchored menu opens. Up and Down change only the highlighted popup row; Enter or Tab applies it and closes the menu; Escape closes the menu without altering the value; clicking a row applies that row and closes the menu.

In Setup > Code review, Tier, Profile, Model, and Effort each render as one compact dropdown field rather than a horizontally scrolling tab strip. Opening Profile or Model and merely moving the highlight does not start discovery. Accepting a changed Profile or Model applies the value and starts the same supervised discovery path used before this refactor. Escape while a selector is open closes only that selector.

Wizard picker pages, workspace and history lists, palettes, chat `/model` filtering, elicitation choices, reviewer panes, and actual tab navigation retain their current layouts.

The full `cargo test` and `cargo clippy --all-targets -- -D warnings` commands pass.

## Idempotence and Recovery

The work is source-only and can be retried safely. `cargo fmt` is deterministic. If component API changes cause consumer compilation errors, update only the two intended consumers and exhaustive matches required by the new enum variants; do not mechanically migrate unrelated lists. Preserve unrelated working-tree edits and stage only files changed for this plan.

## Artifacts and Notes

The initial audit command was:

    rg -n "ChoiceList::render|TabStrip::render|AutocompletePopup::render|ControlKind::ChoiceList" mj-chat/src mj-tui/src -g '*.rs'

It found one manual `AutocompletePopup` consumer in Setup, four review selector rows rendered through one `TabStrip` loop, and many semantically distinct primary-content lists that remain out of scope.

Focused validation produced:

    cargo test -p brokk-mj-chat components::
    test result: ok. 39 passed; 0 failed

    cargo test -p brokk-mj-tui setup::tests
    test result: ok. 17 passed; 0 failed

    cargo test -p brokk-mj-tui review_settings::tests
    test result: ok. 16 passed; 0 failed

    cargo clippy --all-targets -- -D warnings
    Finished `dev` profile; no warnings

The full `cargo test` run passed the workspace library suites and all integration groups except:

    disabled_startup_waits_for_explicit_new_before_creating_a_session
    PTY child did not exit after startup quit; Waiting for 2 operations to complete before exiting

The same test failed on two isolated retries at its five-second PTY deadline. It exercises new-session lifecycle and local-target preflight rather than any migrated combobox surface.

## Interfaces and Dependencies

The final public component surface must include `mj_chat::components::ComboBox` and `mj_chat::components::ComboBoxState<K>`. `ComboBox` must accept a Ratatui frame, the screen bounds within which its popup may appear, the one-line scalar field rectangle, visible option rows, committed or pending selection, enabled and expanded state, and the owning `Form<K>` plus control identifier. `ComboBoxState<K>` must expose methods to open, query, preview, accept, and dismiss a single expanded field without knowing the domain value type.

The form interaction vocabulary must distinguish a combobox commit carrying its chosen index from a local combobox dismissal. No new crate or external dependency is required; reuse Ratatui, `Form`, `ChoiceList`, `AutocompletePopup`, and the existing theme helpers.

Revision note (2026-09-09): Created the plan after the call-site audit. The design deliberately centralizes event semantics as well as rendering because the reported bug was caused by Setup-only orchestration.

Revision note (2026-09-09 13:35Z): Recorded the implemented component and migrations, corrected the Cargo package names, and captured the mouse-commit discovery from focused validation.

Revision note (2026-09-09 13:46Z): Closed the implementation milestones and recorded final validation, including the unrelated repeatable PTY deadline limitation instead of expanding this UI refactor into lifecycle work.
