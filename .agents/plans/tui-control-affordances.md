# Make TUI controls recognizable without accent color

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Users should recognize controls by their shape and function: `›` opens a view, `▾` opens choices, and buttons have a distinct surface. Model and effort choices should open compact dropdowns showing names rather than descriptions and duplicate identifiers. Accent remains useful for focus, selection, primary actions, and semantic activity across the entire TUI.

## Progress

- [x] (2026-09-27) Inspect shared controls, config picker, prompt rendering, and theme usage; confirm implementation and push authorization.
- [x] (2026-09-27) Implement anchored config dropdowns and navigation glyphs.
- [x] (2026-09-27) Audit and neutralize idle control styling throughout the TUI.
- [x] (2026-09-27) Validate behavior, themes, isolated workspace tests, clippy, formatting, and screenshots.
- [x] (2026-09-27) Prepare the validated task-only commit on master for the authorized upstream push.

## Surprises & Discoveries

The existing config picker snapshots advertised values and uses shared Form input handling. ChoiceList supports single-click activation and tracks the clicked selection in Form metadata. AutocompletePopup already supplies bounded anchored placement and eight visible rows. Shared actionable styling is the main source of idle accent throughout dashboard and dialogs. The first full workspace run exposed controller launch tests reading the host database (schema 56, while this checkout supports 55). Fresh MJ_CONFIG_DIR and MJ_DATA_DIR roots isolate those reads; the failing launch tests pass with these roots. Existing navigation-control keyboard focus could intercept Enter before the picker; moving picker routing ahead of those controls makes overlay input ownership explicit. The picker now uses Form as the sole cursor owner instead of mirroring its selection, so single-click activation cannot apply a stale cursor.

## Decision Log

On 2026-09-27 the user selected the whole-TUI scope and then authorized implementation and pushing to master. Retain existing semantic colors and primary-action emphasis; neutralize only affordance-only accent. Reuse Form, ChoiceList, TextField editing, and AutocompletePopup rather than adding an independent widget framework. Typing filters either picker; a current-value check is independent of the navigation cursor. Outside clicks dismiss without activating the underlying chat control.

## Outcomes & Retrospective

Implementation and validation are complete. Model/effort use anchored names-only dropdowns; navigation glyphs and neutral surfaces make controls discoverable across the TUI. Form owns each dropdown cursor, and overlay routing prevents click-through and keyboard activation of underlying navigation controls. All workspace tests pass with fresh configuration/data roots, including 614 chat tests, 832 TUI tests, 1,851 controller tests, and the worker, CLI, real-terminal, and upgrade suites. Clippy passes with warnings denied, formatting and diff checks pass, and all four documentation SVGs were regenerated and parsed successfully. No database/protocol changes were made. Existing unrelated working-tree files remain untouched. The final task commit is prepared for the user-authorized push to origin/master.

## Context and Orientation

`mj-chat/src/chat/config_picker.rs` owns model/effort selection state, input, and rendering. `mj-chat/src/chat/active/render.rs` draws the prompt controls and registers their exact mouse regions. `mj-chat/src/theme.rs` defines semantic styles and Unicode/ASCII symbols. Shared Form and ChoiceList in `mj-chat/src/components/` handle keyboard and safe press/release gestures; AutocompletePopup lays out a bounded overlay relative to a control. `mj-tui/src/surface_controls.rs` uses the same theme for dashboard buttons, pins, pane controls, and menus. The workspace is Rust; use its existing Cargo/mbx build configuration unchanged.

## Plan of Work

First add the navigation glyph, concise choice-name formatting, and anchored pickers. Keep one picker open; snapshot choices, preserve underlying submitted values, and check live availability at commit. Remove Apply/Cancel and modal dismiss chrome; support Enter/click commit, Esc/outside dismissal, and typing/backspace filtering. Anchor to the current model/effort hitbox, or the prompt origin for slash-command access when a chip is not visible. Keep dropdown geometry within the conversation bounds and clear stale geometry each frame.

Next change idle actionable foregrounds to normal text while preserving surfaces and monochrome affordances. Audit direct accent uses in chat, dashboard, settings, dialogs, and wizards. Underline links, retain semantic identity/activity colors, and preserve focused/selected/default-action emphasis. Add navigation markers to Tasks and enabled Subagents. Disabled Subagents keeps its explanation without a navigation marker. Add dropdown glyphs only to selectable model/effort values and register only fully visible controls.

Finally update behavioral tests for anchored layout, names-only choices, filtering after navigation, pointer selection, scrolling, cancellation, disabled values, refreshes, shutdown, and narrow/ASCII/themed rendering. Review the diff, run required checks, then stage only task files, commit on the current master branch, and push origin master. Do not include pre-existing untracked files.

## Concrete Steps

Run commands from `/home/jonathan/Projects/mjolnir`. Use `cargo fmt --all -- --check` after formatting changed Rust files. Run focused `cargo test -p brokk-mj-chat -p brokk-mj-tui` outside the restricted sandbox, followed by `MJ_CONFIG_DIR="$test_root/config" MJ_DATA_DIR="$test_root/data" cargo test` (create `test_root` with `mktemp -d /tmp/mj-tui-affordances.XXXXXX`) and `cargo clippy --all-targets -- -D warnings` using the normal dev profile and existing build storage. Use `--instance tui-affordances-test` for any CLI/TUI interactive invocation; automated tests retain their isolated fixtures.

## Validation and Acceptance

Model/effort chips display a dropdown marker and open a bounded compact list containing only advertised names (the value is used if the name is empty). Clicking a row or pressing Enter emits its original configuration value once. Current-choice marks remain on the committed choice while arrow navigation changes the cursor. Typing filters, including after navigation; empty results cannot submit. Esc/outside clicks do not submit or trigger controls behind the popup. Refresh cannot reorder the snapshotted choices; removed choices and shutdown cannot submit stale changes. Long lists scroll, narrow panes keep hitboxes aligned, and ASCII mode uses ASCII markers. Tasks/Subagents open through their existing interactions and expose `›`. Across all themes idle controls are neutral, focus is clear, and primary actions and semantic colors retain their meaning. Required Cargo checks must pass or any independently established pre-existing failure must be reported precisely.

## Idempotence and Recovery

Changes affect presentation and local input state only, with no schema or protocol changes. Repeating tests is safe with their isolated state. Preserve unrelated working-tree files and do not reset the tree. Fix failed checks before committing; never redirect Cargo output storage.

## Artifacts and Notes

Validation completed successfully:

    MJ_CONFIG_DIR="$test_root/config" MJ_DATA_DIR="$test_root/data" cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    NO_COLOR= cargo test -p brokk-mj-tui generate_documentation_screenshots -- --ignored --nocapture
    git diff --check

The first unisolated workspace run failed 15 controller launch tests because the host store was newer than this checkout; all pass with the isolated roots. No host-store migration or downgrade was attempted. Existing screenshots capture the shared neutral control styling. Behavioral render tests exercise the model/effort dropdown and navigation glyphs in every theme and ASCII mode.

## Interfaces and Dependencies

Extend the shared Glyphs struct with a navigation marker, Unicode `›` and ASCII `>`. Reuse existing config-choice data and SetConfig actions unchanged. No new dependency or crate is required. Keep choice-name interpretation shared between dropdowns and command completion.

Revision note: Initial implementation plan records the approved whole-TUI scope and explicit push authorization.

Revision note: Implementation and focused-test evidence recorded; picker input ownership and monochrome primary-action distinction clarified.

Revision note: Recorded successful clippy and screenshot checks, and the isolated data-root requirement discovered during full validation.

Revision note: Final validation passed, including the complete isolated workspace run; outcomes and publication readiness recorded.
