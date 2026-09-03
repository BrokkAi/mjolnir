# Blank a two-cell halo around every modal

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

When a centered terminal dialog appears, dashboard and transcript characters currently remain immediately outside its border. Dense text therefore appears to run into the dialog, most visibly around the wide plan-review dialog. After this change every centered modal clears itself plus two terminal cells outside each border. The dialog keeps its current size and placement, while the surrounding two-row and two-column halo is visibly blank.

## Progress

- [x] (2026-09-03 20:42Z) Inspected every dashboard and chat modal path and identified the incomplete screen-edge-only margin behavior in commit `4e98115d`.
- [x] (2026-09-03 20:42Z) Fast-forwarded the clean current branch to `origin/master`, including the shared `mj-chat/src/hel_modal.rs` geometry module.
- [x] (2026-09-03 20:46Z) Added shared halo clearing and routed every centered modal through render-aware helpers.
- [x] (2026-09-03 20:47Z) Added shared-helper and plan-review behavior regressions.
- [x] (2026-09-03 20:54Z) Ran formatting, focused tests, the full test suite, and all-target Clippy successfully.
- [x] (2026-09-03 20:54Z) Committed the validated implementation and completed plan on the current branch.

## Surprises & Discoveries

- Observation: Commit `4e98115d` centralized modal geometry but did not implement a blank exterior margin.
  Evidence: Modal renderers still call `frame.render_widget(Clear, popup)`, which resets only cells inside the dialog border; the shared `modal_area` function merely keeps the dialog itself away from the containing area's edge.

- Observation: Dashboard modals already share `centered_modal` and `centered_modal_fixed`, while three chat modals obtain raw centered rectangles.
  Evidence: `mj-tui/src/dialogs.rs`, `help.rs`, `palette.rs`, `resume.rs`, and `wizards.rs` use the selectable helpers; `mj-chat/src/hel_chat/elicitation.rs`, `config_picker.rs`, and `active.rs` use the raw percentage or fixed geometry helpers.

- Observation: An existing overlay behavior test required the entire underlying transcript sentinel to remain visible, including the part newly covered by the requested halo.
  Evidence: The first full `cargo test` run failed only `hel_chat::active::tests::an_elicitation_overlays_the_chat_instead_of_replacing_it`; its rendered buffer showed the sentinel prefix outside the halo and blank cells beneath the halo. The assertion now proves that content remains above and below the overlay while content beneath the halo is intentionally obscured.

## Decision Log

- Decision: Build on `4e98115d` instead of reverting it.
  Rationale: Its cross-crate consolidation is the correct dependency direction and removes duplicate centering implementations; only the meaning and rendering of the margin is incomplete.
  Date/Author: 2026-09-03, user and Codex.

- Decision: Apply a two-cell blank halo on all four sides to every centered modal, but not to anchored autocomplete, reviewer split panes, the terminal-too-small replacement screen, or the full-screen workspace selector.
  Rationale: These exclusions are not dialogs centered over underlying content. Clearing around them would erase useful adjacent content or conflict with their full-screen purpose.
  Date/Author: 2026-09-03, user and Codex.

- Decision: Keep sizing geometry pure, but put the actual `Clear` operation in one shared modal helper.
  Rationale: Non-rendering code uses centered geometry to compute hit boxes and wrapped sizes. The render helper can reuse that geometry while owning the user-visible rule exactly once.
  Date/Author: 2026-09-03, Codex.

## Outcomes & Retrospective

Every centered modal now clears a two-cell exterior halo through the shared `hel_modal` implementation. Dashboard dialogs obtain it automatically from their selectable centering helpers, while the chat's plan review, elicitation, configuration picker, and reviewer picker use render-aware centering helpers from the same module. The raw-clear audit now finds only the shared implementation, anchored autocomplete, and the terminal-too-small replacement screen.

The focused shared-modal tests passed 5/5, focused elicitation tests passed 18/18, and the TUI suite passed 273 tests with its existing timing test ignored. After correcting the one obsolete overlay assertion, the complete default-member `cargo test` passed, including the PTY and socket integration tests. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all`, and `git diff --check` all completed successfully. No functional gap remains.

## Context and Orientation

This is a Rust workspace. `mj-chat/src/hel_modal.rs` contains terminal-cell geometry shared by the chat renderer and the dashboard TUI. A modal is a bordered, centered overlay such as an agent question, plan review, confirmation, wizard, Help overlay, or command palette. Ratatui's `Clear` widget resets every cell in the rectangle where it is rendered. A halo is the rectangle two cells larger than the modal on every side, clipped to the containing frame area so it never addresses cells outside the terminal or pane.

`mj-tui/src/widgets.rs` re-exports the shared modal helpers for dashboard code. Dashboard renderers use `centered_modal` or `centered_modal_fixed`, which also register the dialog body with `FrameSurfaces` for mouse selection. Chat renderers use raw centering because elicitation owns several selectable subregions and the configuration and reviewer pickers register their body in their caller. Those interaction rectangles must continue to describe the dialog body only; the blank halo is visual and must not capture input or selection.

## Plan of Work

In `mj-chat/src/hel_modal.rs`, define a shared operation that expands a modal rectangle outward by `MODAL_SCREEN_MARGIN` using Ratatui cell geometry, intersects the result with the containing area, and renders `Clear` over the result. Update the selectable `centered_modal` and `centered_modal_fixed` entry points to accept the current `Frame`, invoke this operation, and then register only `bordered_content(popup)` as before.

Update every dashboard modal call site under `mj-tui/src/` to pass its frame into the shared selectable helper and delete the immediately following `Clear` on the popup. This covers dashboard editors and confirmations, the web dialog, new/resume wizards, the Resume browser, Help, and the command palette. Keep pure calls used only by `popup_height` and `resume_sessions_pane` unchanged.

Update the three chat modal paths. The agent elicitation and configuration picker should call the shared clear operation after computing their centered rectangle and before drawing the border. The second-opinion reviewer setup should clear the halo in `active.rs` after centering; remove the old inner-only `Clear` from `second_opinion.rs`. Do not change `autocomplete.rs`, which draws an anchored popup rather than a centered modal.

Add a unit test in `mj-chat/src/hel_modal.rs` that fills a test frame with nonblank, styled sentinel cells, invokes the shared clear operation for a known popup, and proves that all cells in the expanded two-cell rectangle are reset while cells immediately beyond it remain untouched. Cover clipping against a containing rectangle whose origin is nonzero. Add an elicitation regression that draws a plan-review modal over sentinel content and proves that the rows and columns immediately outside all four borders are blank for the full two-cell thickness.

## Concrete Steps

Work from `/home/jonathan/Projects/hel2`. Modify sources only with focused patches, then format with:

    cargo fmt --all

Run focused behavior tests outside the restricted sandbox:

    cargo test -p brokk-mj-chat hel_modal
    cargo test -p brokk-mj-chat hel_chat::elicitation
    cargo test -p brokk-mj-tui

Check that raw clears remaining in modal-related source belong only to explicit nonmodal behavior:

    rg -n 'render_widget\((ratatui::widgets::)?Clear' mj-chat/src mj-tui/src

Run the repository-required validation outside the restricted sandbox:

    cargo test
    cargo clippy --all-targets -- -D warnings

Inspect `git diff --check` and `git status --short`, stage only this plan and the source/test files changed for the halo, then commit them on the current branch.

## Validation and Acceptance

The new shared-helper test must fail against `4e98115d` because sentinel characters remain outside the modal border and pass after the helper clears the expanded rectangle. The plan-review regression must reproduce the reported class of display: underlying text is visible elsewhere, but the complete two-cell ring immediately outside the plan-review border contains blank, reset cells. The border position, interior text, selection surfaces, and dialog controls remain unchanged.

All focused tests, the complete default-member `cargo test`, and `cargo clippy --all-targets -- -D warnings` must exit successfully. Formatting and `git diff --check` must report no issues.

## Idempotence and Recovery

The geometry and rendering edits are deterministic and safe to reapply. If a test exposes a modal bypassing the shared helper, route that renderer through the helper rather than adding a local clear. Do not revert `4e98115d`, rewrite published history, change branches, or stage unrelated working-tree files.

## Artifacts and Notes

The original failure is a wide `Plan review` box with dashboard and transcript glyphs directly adjacent to its left and right borders. The expected shape after this work is two blank columns beside both vertical borders and two blank rows above and below the horizontal borders, clipped only when the containing render area itself ends.

## Interfaces and Dependencies

No CLI, protocol, persistence, or configuration interface changes. `mj-chat::hel_modal` remains the shared cross-crate terminal primitive and continues using Ratatui. `MODAL_SCREEN_MARGIN` remains `2`. Pure centering functions continue returning the dialog `Rect`; the render-aware shared operation owns clearing the expanded, clipped halo. `FrameSurfaces` continues registering only the bordered content rectangle, not the halo.
