# Make sidebar sizing global and adapt workspace navigation

This ExecPlan is maintained according to `.agents/PLANS.md`. Keep its Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

The Sessions sidebar should retain its chosen width across workspaces and application restarts. Medium and large sidebars should wrap workspace tabs across up to five content rows, then scroll vertically. Tiny sidebars should show the active workspace in a dropdown. Keyboard and mouse navigation must remain usable, including Unicode names and attention badges.

## Progress

- [x] (2026-11-04) Traced workspace view restoration, workspace settings persistence, combined layout, and the existing anchored pane menu.
- [x] (2026-11-04) Chose shared configuration for sidebar size and existing pane menu for the tiny workspace chooser.
- [x] (2026-11-04) Implemented global sidebar preference and ordered background persistence.
- [x] (2026-11-04) Implemented wrapping workspace rows, scrolling, and tiny dropdown.
- [x] (2026-11-04) Extended rendered goldens, Unicode hit testing, workspace restoration, config round-trip, and pointer-drag routing coverage.
- [x] (2026-11-04) Isolated tmux instance confirmed dropdown selection, unchanged tiny size across workspace switches, saved configuration, and tiny size on reopening another workspace.
- [x] (2026-11-04) Final full core, controller, TUI, and CLI suites passed, including CLI integration, PTY upgrade/termination tests, and doctests. Workspace clippy passed with warnings denied; formatting and diff checks passed.
- [ ] Commit, request review, address findings, and push to origin/master.

## Surprises & Discoveries

The tool catalog exposes sub-agent spawning, but the session rejects it with `this session does not allow Mjolnir sub-agents`. Implementation and checks therefore run in the parent session.

Existing workspace snapshots persist all three support pane sizes in `PaneSizes`. Its Sessions value must no longer override the shared sidebar preference; the stored database representation remains readable without a migration.

Dragging outside the workspace pane also needs ownership in `component_handles_mouse`, before the CLI routes the pointer to conversation text selection. The scrollbar now retains that ownership until release or cancellation. The rendered regression exercises this boundary.

## Decision Log

- Decision: Persist only the Sessions size globally, in `Config.sidebar_size`; preserve per-workspace Targets and Profiles sizes.
  Rationale: This directly matches the requested sidebar behavior without broadening the scope to other panes or changing the database schema.
  Date/Author: 2026-11-04 / Codex.
- Decision: Wrap existing workspace tabs rather than convert them to one workspace per row. Cap at five content rows, excluding borders.
  Rationale: Retains the current compact navigation and displays more workspaces at medium and large widths.
  Date/Author: 2026-11-04 / Codex.
- Decision: Reuse the anchored pane menu for the tiny dropdown and the ordered background setting writer for persistence.
  Rationale: Keeps keyboard, pointer, cancellation, and bounded shutdown behavior shared with existing controls.
  Date/Author: 2026-11-04 / Codex.

## Outcomes & Retrospective

All requested behaviors are implemented and validated. The existing workspace-manager golden now shows medium/large overflow, wheel and track navigation, selected-workspace visibility, and the tiny chooser. Four existing dashboard goldens received only the new tiny dropdown marker. The reviewed push remains.

## Context and Orientation

`mj-tui/src/dashboard_panes.rs` owns `DashboardState.pane_sizes`, the explicit sizes of Sessions, Targets, and Profiles. `mj-tui/src/dashboard_workspaces.rs` currently replaces them on workspace switches and on background workspace-cache loads. `mj-core/src/config.rs` defines both the runtime configuration and its stored representation. `mj-cli/src/dashboard/workspace_settings.rs` serializes background saves; `mj-cli/src/dashboard.rs` drives and flushes those saves.

`mj-tui/src/combined.rs` measures and allocates the bordered workspace pane above Sessions. `mj-tui/src/workspaces.rs` draws workspace tabs and handles keyboard and pointer navigation. `mj-tui/src/pane_controls.rs` implements menus anchored to a screen rectangle, with existing scrolling and dismissal. Rendered tests use Ratatui's terminal backend and checked-in files under `mj-tui/tests/golden/`.

## Plan of Work

First add the shared size preference with Standard as the default. Initialize Sessions from it. Workspace restoration keeps the current Sessions size and resolves the exclusive-maximum rule by demoting workspace-specific maxima when Sessions is maximized. Serialize configuration writes off the event loop and flush them within the existing shutdown budget.

Next calculate workspace tab rows from cell widths, using the same calculation for measurement and drawing. Reserve a scrollbar column when rows overflow. Preserve visible selection on workspace changes and support wheel scrolling of the viewport. Reuse existing Unicode truncation and attention-badge rendering. Keep child/reviewer workspace headers compact.

Finally add a workspace operation to the existing anchored menu for tiny mode. Open it from the active workspace label or Enter/Space and return the ordinary SelectWorkspace action upon activation. Keep management in its existing menu button.

## Concrete Steps

Work from the repository root. Use `cargo fmt --all` for formatting. Run Cargo tests outside the restricted sandbox, leaving mbx build storage untouched. Run focused tests during implementation, then `cargo test -p brokk-mj-core -p brokk-mj-tui -p brokk-mjolnir` and `cargo clippy --all-targets -- -D warnings` after the final implementation round. Golden updates use `MJ_UPDATE_GOLDEN=1` and require inspection of the resulting diff.

## Validation and Acceptance

A rendered regression must show wrapped workspaces, five-row overflow with a scrollbar, wheel scrolling and selection visibility, and the tiny dropdown's open/selection/dismissal transitions. Extend the existing workspace-restoration test to show that cached or new workspaces cannot reset sidebar size. Configuration round-trip coverage must prove the global preference survives saving and reloading with an isolated configuration directory. Required crate suites and clippy must pass. Any live CLI invocation uses a named instance and isolated config/data directories.

## Idempotence and Recovery

No database migration or live store changes are needed. Legacy per-workspace Sessions values remain accepted but cannot override the shared preference. Failed background saves are surfaced by the existing setting writer. Stage only task-owned files. On a rejected push, fetch and merge origin/master without rebasing.

## Artifacts and Notes

Keep validation logs outside Cargo target directories so mbx cleanup cannot remove evidence. Record decisive results here when complete.

Focused workspace tests passed (15 tests), followed by all five affected golden cases. Initial full validation passed controller (2,272 tests) and core (562 tests plus subprocess cases), then identified the expected golden marker updates. Final logs are `/mnt/optane/tmp/mj-sidebar-tests-final.log` and `/mnt/optane/tmp/mj-sidebar-clippy.log`. Live captures are under `/mnt/optane/tmp/mj-sidebar-live-cabfc0f9/`: `tiny.txt`, `dropdown.txt`, `switched.txt`, and `reopened.txt`. That named instance used a fake home, isolated config/data, and no agent profiles; it has been stopped.

Final validation exited successfully: controller 2,272 tests, core 562 tests plus subprocess checks, TUI 570 tests, CLI 236 unit tests plus all integration/PTY suites and doctests. `cargo clippy --all-targets -- -D warnings` finished successfully in the dev profile. The copied live-test binary was removed after stopping the isolated daemon; captures remain.

## Interfaces and Dependencies

Use the existing `PaneSize`, `PaneSizes`, `WorkspaceSettingPersistence`, `PaneMenu`, Ratatui, and theme glyph/scrollbar helpers. Add no crate or external dependency. Keep shared workspace row interpretation in `mj-tui/src/workspaces.rs` and use it from the combined layout.

Revision 2026-11-04: Initial plan records the design and disabled-delegation constraint before implementation.

Revision 2026-11-04: Recorded implementation, pointer-ownership correction, focused validation, and isolated live evidence before final validation.

Revision 2026-11-04: Recorded successful full validation before committing and requesting review.
