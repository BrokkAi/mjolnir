# Make support-pane sizing explicit and stable

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

The terminal dashboard currently couples keyboard focus to pane height: pressing Tab can resize Sessions, Targets, or Quota, while Alt-G changes all three through a hidden two-position layout mode. After this change, Sessions, Targets, and Quota each show three title-bar controls and hold an explicit minimized, standard, or maximized size. Tab only moves the keyboard, mouse clicks and Alt-Z resize one pane, and Alt-G switches predictably between the all-standard layout and a Sessions-focused preset. A user can verify the result by watching pane borders stay fixed while tabbing, clicking the `▁`, `▪`, and `□` title controls, and using Alt-Z and Alt-G.

## Progress

- [x] (2026-09-03) Inspected the current layout allocator, focus reducer, command registry, mouse routing, title rendering, minimum-size behavior, tests, and user documentation.
- [x] (2026-09-03) Settled the interaction and allocation design with the user.
- [x] (2026-09-03) Replaced the global layout state and commands with explicit per-pane sizing.
- [x] (2026-09-03) Rendered styled, clickable size controls and stable focus-independent allocations.
- [x] (2026-09-03) Updated behavior tests, README guidance, and the parallel testing runbook.
- [x] (2026-09-03) Ran formatting, the full test suite, and Clippy with warnings denied; all passed.
- [x] (2026-09-03) Prepared the validated result for a direct commit to the current branch.

## Surprises & Discoveries

- Observation: the current implementation exposes only two `PaneLayout` variants, but collapsed Sessions has three observable shapes because its renderer also depends on terminal aspect ratio and a 40-row border threshold.
  Evidence: `DashboardState::sessions_compact`, `minimized_grid_rows`, and `minimized_grid_bordered` jointly select the grid, portrait list, and borderless tiny grid.
- Observation: full-pane selection surfaces exclude their borders, so title clicks reach dashboard mouse handling; the one-row summaries register their full rectangle, but the selection engine replays an undragged click on mouse-up.
  Evidence: `route_selection_event` returns a synthetic left-button press for `SelectionAction::Click`.
- Observation: retaining an otherwise-unused terminal-size cache solely for the old aspect-ratio layout made focus-independent sizing harder to reason about.
  Evidence: after removing `sessions_compact`, the only remaining use of `frame_size` was assigning it during render, so the field was removed.
- Observation: the one-row Targets and Quota forms need command availability checks as well as compact rendering; otherwise their hidden selections can still trigger row actions from the command path.
  Evidence: minimized-pane tests now prove navigation and row actions do nothing until the corresponding table is restored.

## Decision Log

- Decision: replace global modes with independent `Minimized`, `Standard`, and `Maximized` states for Sessions, Targets, and Quota only.
  Rationale: focus and geometry become independent, while Conversation can remain the residual region and Prompt can remain content-driven.
  Date/Author: 2026-09-03, user and Codex.
- Decision: allow one maximized pane. Maximizing another demotes the previous maximum to Standard but leaves every other state unchanged.
  Rationale: the maximized pane gets deterministic priority without silently erasing unrelated minimized choices.
  Date/Author: 2026-09-03, user and Codex.
- Decision: Alt-Z cycles the focused support pane; Alt-G restores all Standard unless already all Standard, in which case it applies Sessions Maximized with Targets and Quota Minimized. Neither command changes focus.
  Rationale: Alt-Z is the individual keyboard path and Alt-G remains a fast global preset.
  Date/Author: 2026-09-03, user and Codex.
- Decision: use `▁`, `▪`, and `□`, with white-on-dark-gray inactive chips and bold black-on-cyan active chips.
  Rationale: these are single-cell standard Unicode glyphs and reuse the TUI's established button palette without requiring a Nerd Font.
  Date/Author: 2026-09-03, user and Codex.
- Decision: minimized Sessions always uses the grid, keeps its border and controls, and retains the adaptive two/five content-row threshold. Minimized Targets and Quota stay visible as one-row summaries.
  Rationale: an explicit mouse interface cannot disappear on short terminals, and one state should not change representation based on aspect ratio.
  Date/Author: 2026-09-03, user and Codex.

## Outcomes & Retrospective

The dashboard now owns three independent pane sizes, renders the chosen state in
every support-pane title, and computes height without consulting keyboard focus.
Minimized panes remain visible and focusable, while row-only operations are
unavailable until their tables are restored. Mouse chips, Alt-Z, and Alt-G all
use the same state transition methods, including the single-maximum invariant.

Validation completed on 2026-09-03. `cargo fmt --all` completed cleanly;
`cargo test` passed the full workspace, including the PTY and fault-injection
coverage; and `cargo clippy --all-targets -- -D warnings` passed. The focused
TUI result was 275 passed, 0 failed, and 1 timing test ignored.

## Context and Orientation

`mj-tui/src/lib.rs` owns `DashboardState`, keyboard focus, mouse reduction, and the current `PaneLayout`. `mj-tui/src/actions.rs` is the single command registry used by key handling, the footer, help, and the F2 palette. `mj-tui/src/combined.rs` computes six vertical bands: Sessions, Conversation, Prompt, Targets, Quota, and footer. `mj-tui/src/render.rs` renders the three support panes and their title lines. `mj-cli/src/dashboard.rs` routes terminal mouse gestures through text selection before forwarding clicks to `DashboardState`.

A support pane means Sessions, Targets, or Quota. Conversation is not keyboard-focusable and consumes the height left after the other bands. Prompt is focusable but its height follows wrapped input and is not manually resizable.

## Plan of Work

First, replace `PaneLayout` with public `SupportPane` and `PaneSize` enums and three Standard size values in `DashboardState`. Add accessors and one setter that enforces the single-maximum invariant. Make the focus ring unconditional. Implement Alt-Z as `CycleFocusedPaneSize` and replace the old layout command with an Alt-G pane preset command. Alt-Z on Prompt reports guidance. Minimized Targets and Quota remain focusable but their hidden row commands and selection movement do nothing until restored. Mouse control clicks set the requested size without changing focus; ordinary summary clicks may focus the pane but must not resize it.

Next, generalize `allocate_combined_heights`. Every allocation first reserves the state-specific support minima, the three-row Conversation, the three-row Prompt, and the footer. Prompt grows toward its desired height first. A maximized pane then grows to its complete content height without its Standard cap, bounded by remaining frame space. Standard panes grow afterward in physical order with Sessions capped at one third of frame height and Targets and Quota capped at one quarter. Conversation receives the residual. This order must not inspect focus.

Minimized Sessions is a fixed three-column grid with two content rows below a 40-row frame and five at or above it, always plus a two-row border. Remove the portrait-list branch, borderless branch, and support-pane omission. Minimized Targets and Quota are fixed one-row summaries. Standard and Maximized panes use their existing list/table renderers and scrolling.

Finally, replace the Sessions legend with a left title `Sessions · <workspace>` and add right-aligned size controls to all three support panes. Each control is a padded three-cell chip, controls have a one-cell unstyled gap, and the active state uses the focused-button palette. Record exact hitboxes each frame and test them before row/pane hitboxes in mouse handling. At narrow widths, reserve all controls and maximize workspace visibility: abbreviate Sessions to `S` before ellipsizing the workspace. Quota refresh status and collapsed readings truncate before pane labels or controls.

Update README, generated help/footer expectations, the reliability runbook, and tests that describe the old modes. Do not rewrite the completed historical combined-dashboard ExecPlan.

## Concrete Steps

Work from `/home/jonathan/Projects/hel2`. Use `apply_patch` for source and documentation edits. After each coherent milestone, run focused package tests with elevated permissions because Cargo tests exercise sockets. At completion run:

    cargo fmt --all
    cargo test
    cargo clippy --all-targets -- -D warnings

Update this file with results, stage only files changed for this task, and commit directly to the current branch. Do not push.

## Validation and Acceptance

Reducer tests must prove all panes start Standard; Alt-Z cycles only the focused support pane and preserves focus; Prompt gets a notice; a new maximum demotes only the prior maximum; and Alt-G implements the asymmetric preset while preserving focus. Tab and Shift-Tab must reach all four focus stops at every size without changing any band height.

Allocation tests must prove Prompt growth precedes a maximum, the maximum precedes Standard growth, Standard caps do not depend on focus, Conversation never falls below three rows, and too-small messages report the dynamic requirement. A maximized pane whose content exceeds available height must scroll rather than hide another band's minimum.

Rendering tests must verify all three glyphs, foreground/background/modifier styles, direct mouse hitboxes, the active state, minimum-width title priorities, and removal of the Turn/Step legend. Grid tests must cover portrait and landscape widths, both sides of the 40-row content threshold, an always-present border/title, scrolling, and selection/copy coordinates. Targets and Quota tests must cover independent one-row summaries that remain focusable and restorable.

The full `cargo test` suite and `cargo clippy --all-targets -- -D warnings` must pass. By hand, a 32-column TUI must retain workspace text and all size controls; Tab must not move pane borders; each mouse chip and Alt-Z must change exactly one chosen state; Alt-G must alternate all-Standard with the Sessions-focused preset when invoked repeatedly.

## Idempotence and Recovery

The changes are ordinary source edits and can be reapplied safely. Pane sizes are client-local and require no configuration migration or persistent data cleanup. If a checkpoint fails, fix forward without resetting unrelated worktree changes. Cargo build artifacts are disposable; repository-tracked snapshots must not be regenerated unless a failing test proves they belong to this feature.

## Artifacts and Notes

The intended expanded title is conceptually:

    Sessions · workspace                                      ▁  ▪  □

The three glyph backgrounds, not the surrounding one-cell gaps, form the clickable regions. At 32 columns the title uses `S · <workspace>` and ellipsizes only the workspace portion needed to retain all controls.

## Interfaces and Dependencies

Define public `SupportPane { Sessions, Targets, Quota }` and `PaneSize { Minimized, Standard, Maximized }` enums in `mj-tui/src/lib.rs`; `PaneSize::default()` is Standard and `cycled()` follows the displayed order. `DashboardState` exposes typed `pane_size`, `set_pane_size`, `cycle_focused_pane_size`, and `toggle_pane_preset` operations. Remove `PaneLayout`, `pane_layout`, `cycle_pane_layout`, and aspect-dependent `sessions_compact`.

Replace public `CommandId::CyclePaneLayout` with `TogglePanePreset` and add `CycleFocusedPaneSize`. The command registry binds the former to Alt-G and the latter to Alt-Z; footer/help/palette text comes from those same specifications. No new crate or dependency is needed.

Revision note (2026-09-03): Initial plan created from the completed design conversation so implementation can proceed under `.agents/PLANS.md`.

Revision note (2026-09-03): Recorded the completed implementation, two design-relevant discoveries, documentation updates, and final validation evidence.
