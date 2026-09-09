# Make Setup compact and use inline choice popups

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

The terminal Setup screen currently occupies the full available width and replaces its main list with a mostly empty editor whenever a user opens a setting with a fixed set of values. After this change, Setup opens at one stable, content-derived size, keeps that size while the user navigates, groups the general display controls under an Interface page, marks choice-backed rows with a down-pointing glyph, and opens those choices in the same compact popup style used by chat autocomplete. A user can see the result by pressing F7, navigating among Setup pages, and opening Theme or another fixed choice without the outer dialog changing size.

## Progress

- [x] (2026-09-09 11:29Z) Inspected Setup state, schema mapping, modal geometry, form routing, and chat autocomplete rendering; settled the user-visible behavior.
- [x] (2026-09-09 12:02Z) Added the reusable autocomplete popup frame and preserved chat autocomplete behavior.
- [x] (2026-09-09 12:02Z) Added the virtual Interface page, stable Setup sizing, choice glyphs, and inline popup state transitions.
- [x] (2026-09-09 12:02Z) Added focused behavior and rendering tests, including real rendered-surface geometry and pointer activation.
- [x] (2026-09-09 12:02Z) Ran formatting, the full Rust test suite, and clippy; reviewed and tightened the integrated diff.
- [x] (2026-09-09 12:20Z) Integrated the first upstream batch, preserved its second Advanced setting, and regenerated the Setup documentation capture.
- [x] (2026-09-09 12:51Z) Committed the implementation, integrated all subsequent upstream modal and prompt-surface changes, reran the complete validation, and prepared the branch for the requested push.

## Surprises & Discoveries

- Observation: Setup choice editing is not a second modal object; `SetupDialog::editor` replaces the body and footer of the same full-size modal.
  Evidence: `mj-tui/src/setup.rs::render_setup` renders `ChoiceList` and Back/Use default/Apply whenever `editor.choices` is non-empty.
- Observation: The three requested Interface settings are root fields in `HelConfig`, while the existing Advanced page contains only `detailed_activity_clocks`.
  Evidence: `src/hel_config.rs::HelConfig` stores `sessions_side`, `spinner`, and `theme` at the root and `AdvancedConfig` separately. The on-disk shape must therefore remain flat for compatibility.
- Observation: Chat autocomplete has reusable visual behavior but is currently implemented inside the chat-specific module rather than as a shared component.
  Evidence: `mj-chat/src/hel_chat/autocomplete.rs::render_autocomplete` owns its popup geometry, border, clipping, and selected-row styling.
- Observation: A `ChoiceList` pointer release emits `Interaction::Select`, just like arrow navigation, rather than a separate activation action.
  Evidence: `mj-chat/src/components/scope.rs::pointer_interaction` maps a choice-list release to `Select`; Setup distinguishes a left-button release so arrows only move while clicks commit.
- Observation: `origin/master` advanced by three commits during implementation and includes overlapping Setup and workspace-structure edits.
  Evidence: `git status --short --branch` reports `hel3...origin/master [behind 3]`, and `git diff --name-only HEAD..origin/master` includes `mj-tui/src/setup.rs`, `mj-tui/src/setup/schema.rs`, and workspace manifests.
- Observation: A second upstream batch landed while the post-merge suite was running and replaces the terminal-wide modal dismissal/layout machinery.
  Evidence: `origin/master` advanced through `6ff448d0` and `2b6e4617` after merge commit `49b6b8eb`; the new commits overlap Setup's surrounding modal APIs and require a second integration pass.
- Observation: The first two post-merge workspace runs reached the PTY binary only after prolonged stress-test load and timed out while several fixtures waited for terminal capability replies; every PTY case passed in serial or isolation, and the later upstream harness update made the final full run pass all six together.
  Evidence: the final `cargo test` run after `0f730c9f` reports `termination_pty` with 6 passed and 0 failed.

## Decision Log

- Decision: Name the new section Interface and put it first at the Setup root.
  Rationale: The user chose Interface; placing it first makes the consolidated common UI settings discoverable.
  Date/Author: 2026-09-09 / Codex and user
- Decision: Interface contains Session sidebar position, Activity animation, and Theme. Advanced remains a separate root page.
  Rationale: The user explicitly chose to keep Detailed activity clocks under the existing Advanced page.
  Date/Author: 2026-09-09 / Codex and user
- Decision: Choice selection commits with Tab, Enter, or mouse click; Escape closes the popup without changing the draft.
  Rationale: This matches autocomplete interaction and the user's selected immediate-commit behavior. Nullable settings continue to expose Automatic / default as a choice.
  Date/Author: 2026-09-09 / Codex and user
- Decision: Compute one preferred size from the maximum structural needs of all Setup pages at opening time and never recompute it for that dialog instance.
  Rationale: The user requested the maximum of the contents and no resizing after opening. Later discovery, editing, and notices must wrap, scroll, or clip within the stored size.
  Date/Author: 2026-09-09 / Codex and user
- Decision: Preserve the serialized `HelConfig` shape and implement Interface as a presentation-only logical page whose fields map to their existing root storage paths.
  Rationale: Regrouping the UI does not justify a configuration migration or compatibility break.
  Date/Author: 2026-09-09 / Codex

## Outcomes & Retrospective

The feature is implemented, documented, synchronized with upstream, and fully validated. Setup now renders compactly at one stable size, exposes Interface without changing the serialized configuration, marks choice-backed rows with `▾`, and uses an anchored autocomplete-style popup for fixed choices. It also composes with the upstream shared modal title and dismissal behavior: Escape cancels an open choice popup, while the title-bar dismissal remains inert behind that popup. Implementation commit `48e04c3b`, documentation commit `d2d93846`, and the subsequent merge commits contain the completed result. The final focused Setup run passed 17 of 17, the final workspace `cargo test` run passed including all six PTY tests, and `cargo clippy --all-targets -- -D warnings` completed successfully.

## Context and Orientation

`mj-tui/src/setup.rs` owns the in-memory Setup draft, its logical navigation path, keyboard and mouse interactions, and rendering. Its `Editor` stores either free text or a vector of fixed JSON choices. `mj-tui/src/setup/schema.rs` supplies labels, help, defaults, and the choice vectors. `mj-tui/src/review_settings.rs` renders the Code Review child page reached from Setup. `mj-chat/src/hel_chat/autocomplete.rs` renders the existing chat autocomplete popup, while `mj-chat/src/components/` holds reusable TUI controls used by both crates. `mj-chat/src/hel_modal.rs` provides fixed and percentage modal geometry.

A logical path is the route shown to the user. Most logical paths are also JSON storage paths, but `Interface / Theme`, for example, must map to the root JSON path `/theme`. The main Setup modal and the Code Review child are one user workflow and must use the same stored outer rectangle. A choice popup is an overlay inside that outer rectangle; it does not replace or resize the page behind it.

## Plan of Work

First extract the autocomplete popup's shared frame and placement behavior into a public component in `mj-chat/src/components/`. The helper must clear and frame an anchored list, cap visible rows at eight, prefer a requested side of the anchor, fall back to the other side when necessary, clamp horizontally and vertically to its bounds, and return the inner rectangle for caller-owned row rendering. Refactor chat autocomplete to use this helper without changing its rows, title, selected-window calculation, or surface registration.

Next refactor `SetupDialog` around logical page entries rather than assuming every visible key is directly below `draft[path]`. The root must show Interface first and hide the physical root keys `sessions_side`, `spinner`, and `theme`. Entering Interface must show exactly those three values; reads, choice lookup, edits, and serialization must resolve each logical Interface child to its original root storage path. Advanced and every unrelated Setup path keep their current behavior and storage.

Add a stored preferred size to `SetupDialog`. At construction, traverse the initial expanded draft and measure the widest structural line across visible rows, breadcrumbs, labels, and complete button groups using Unicode terminal cell width. Help, notices, and editable text do not expand the width: help and notices wrap, and text inputs scroll. With that width, measure the tallest page using its row count, wrapped help, reserved notice area, footer, and the Code Review page's structural minimum, capped at the existing 32-row maximum. Rendering uses `centered_modal_fixed`, clamped by the normal two-cell screen margin. Pass the resulting rectangle to Code Review rendering rather than allowing that child to choose a percentage size. Never recompute the preferred size after opening.

Build list rows as styled lines. Objects and arrays keep the `›` affordance, booleans remain immediate toggles, and a scalar whose resolved storage path has non-empty `schema::choices` displays `▾` after its current value. Free-text scalars have no choice glyph.

When such a row opens, preserve its parent logical page and render it normally behind the overlay. Store the current choice index in the editor, render the reusable popup anchored at the selected row's value column, and use `ChoiceList` inside the returned content rectangle so keyboard and pointer behavior remain consistent with other forms. Prefer below the row and fall back above. Arrow keys move, Tab or Enter commits the selected JSON value immediately, a click selects and commits, and Escape removes the editor without changing the draft. The popup uses at most eight visible rows and scrolls. Free-text and add-name editors retain their existing full-page input with Back, Use default, and Apply.

## Concrete Steps

Work from `/home/jonathan/Projects/hel3`.

1. Add and test the shared popup primitive, then update chat autocomplete to call it.
2. Add logical Interface-to-storage mapping, stored sizing, common Setup/Code Review geometry, row glyphs, and overlay rendering/input.
3. Update existing Setup test helpers to enter Interface before Theme and add focused tests for the new behavior.
4. Run:

       cargo fmt --check
       cargo test
       cargo clippy --all-targets -- -D warnings

   Run every `cargo test` outside the restricted sandbox because this repository's suite uses loopback TCP and Unix sockets. Expect every command to exit zero with no warnings.
5. Review `git diff` and `git status`, stage only files changed for this work, and commit on the current `hel3` branch. Fetch the remote and merge `origin/master` only if it advanced. If a merge changes tested code, rerun the required checks. Push to the configured upstream.

## Validation and Acceptance

At 100 by 30 cells with a default configuration, the Setup border must leave unused columns on both sides. Its exact outer rectangle must remain identical after entering Interface, Advanced, a free-text editor, a choice popup, and Code Review. On smaller terminals, the requested rectangle may clamp to the modal bounds, but controls must remain reachable.

The root list must contain Interface and Advanced and must not contain Session sidebar position, Activity animation, or Theme. Interface must contain exactly those three settings. Selecting Light from Interface / Theme must update the draft's root `theme` field and produce the same serialized `HelConfig` as before this UI regrouping.

Theme and other fixed-value rows must show `▾`; free-text fields such as Listen address and port must not. Opening a fixed-value row must leave the underlying page visible and overlay the compact value popup. Arrow plus Enter, Tab, and click must commit. Escape after moving the highlight must preserve the original value. Automatic / default must remain selectable for nullable profile and target fields. A list longer than eight choices must scroll while retaining correct selection.

Existing chat autocomplete tests must continue to pass and a focused rendering test must prove the extracted helper preserves its placement and clipping behavior.

## Idempotence and Recovery

All source edits and tests are safe to repeat. Do not change branches, rewrite history, reset the worktree, or stage unrelated files. If upstream advances, merge it into `hel3`, resolve only overlapping code deliberately, and rerun validation before pushing. If a test fails, keep the dialog draft and input state semantics intact while correcting the implementation; do not weaken the behavioral assertions.

## Artifacts and Notes

Validation before upstream integration:

    cargo test -p brokk-mj-tui setup::tests
    test result: ok. 17 passed; 0 failed

    cargo test -p brokk-mj-chat
    test result: ok. 469 passed; 0 failed; 1 ignored

    cargo test
    all workspace test binaries and doctests passed; only documented live, benchmark, screenshot, and container tests were ignored

    cargo clippy --all-targets -- -D warnings
    Finished successfully with warnings denied

The final commit set must include this updated ExecPlan alongside the implementation and tests. Record the implementation commit identifier and post-merge validation in Outcomes & Retrospective.

Final validation after all upstream integrations:

    cargo test -p brokk-mj-tui setup::tests
    test result: ok. 17 passed; 0 failed

    cargo test
    all workspace test binaries and doctests passed; only documented live, benchmark, screenshot, and container tests were ignored

    cargo clippy --all-targets -- -D warnings
    Finished successfully with warnings denied

    cargo test -p brokk-mj-tui generate_documentation_screenshots -- --ignored --nocapture
    test result: ok. 1 passed; the committed Setup capture was current

## Interfaces and Dependencies

Export a small autocomplete popup frame/layout primitive from `mj-chat::components`. It returns the outer and inner popup rectangles and owns only geometry, clearing, and the modal frame; callers retain their list data, selected index, input routing, and selection-surface registration. Do not add a dependency or change public configuration types.

Inside `mj-tui`, add an internal logical-to-storage path resolver and a stored preferred Setup size. Change Code Review rendering to accept the already chosen Setup rectangle. These are crate-internal interfaces and must not alter controller, daemon, web, or serialized configuration contracts.
