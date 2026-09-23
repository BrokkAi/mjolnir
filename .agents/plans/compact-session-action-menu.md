# Make the session action menu compact and state-aware

This ExecPlan is a living document maintained in accordance with `.agents/PLANS.md`. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must stay current while work proceeds.

## Purpose / Big Picture

The menu opened from a session row currently looks like the global command palette: it repeats recent actions, labels itself “Commands,” repeats the session name as a group, and offers actions that cannot work. After this change, the row menu names the session in its title, presents each action once in a compact task-oriented order, and makes impossible actions visibly and behaviorally disabled. The global searchable command palette keeps its current behavior.

## Progress

- [x] (2026-09-21) Inspected the command registry, palette renderer, shared choice-list behavior, and pin layout state.
- [x] (2026-09-21) Implemented the compact session-only presentation and state-aware Pin/Unpin availability.
- [x] (2026-09-21) Added focused behavior tests for grouping, title, compact controls, disabled navigation, and pin state.
- [x] (2026-09-21) Ran formatting, the full Rust test suite, Clippy with warnings denied, and diff checks.
- [x] (2026-09-21) Reviewed and committed the completed change on the current branch.

## Surprises & Discoveries

- Observation: The shared choice-list already supports per-row enabled state and skips disabled rows for keyboard and pointer input.
  Evidence: `mj-chat/src/components/scope.rs` checks `row_enabled` in navigation, activation, and pointer paths, so the menu only needs to supply accurate availability.
- Observation: The screenshot's “Jev decisions” action is absent from this checkout and the user confirmed it is gone.
  Evidence: no matching command exists in `mj-tui/src/actions.rs`; the menu's fixed order therefore begins with Changed files.

## Decision Log

- Decision: Keep the global F2 palette unchanged and specialize only the session-row menu.
  Rationale: Search and Recent are useful in a large global list but are noise in a nine-action contextual menu.
  Date/Author: 2026-09-21 / Codex
- Decision: Present Changed files first, then Organize and Lifecycle groups, with Destroy isolated by a divider.
  Rationale: A one-item Inspect heading adds more visual weight than information, while destructive action separation is still valuable.
  Date/Author: 2026-09-21 / Codex

## Outcomes & Retrospective

The session menu now has a fixed nonduplicated action list, session title, compact labels, and no query or Run button. Pin and Unpin use the actual pane layout to determine availability, and disabled rows reject both keyboard and mouse activation. The full workspace test suite and Clippy passed.

## Context and Orientation

`mj-tui/src/actions.rs` is the single registry for command labels, behavior, key bindings, and availability. `mj-tui/src/palette.rs` builds and renders both the global palette and the contextual session menu. The shared `ChoiceList` in `mj-chat` receives a Boolean enabled value for each rendered row and enforces it for keyboard and mouse interaction.

## Plan of Work

Give the session-only palette a fixed list and presentation order while retaining the registry as the authority for behavior and availability. Capture the selected session's identity and title when opening the menu. Render its title without command counts, search, Recent, or Run controls. Add Organize and Lifecycle headings and a divider before Destroy. Supply blocked rows to the shared list as disabled and recompute availability while the menu remains open. Define Pin as blocked for an already pinned session and Unpin as blocked for an unpinned session.

## Concrete Steps

From `/home/jonathan/Projects/hel`, edit `mj-tui/src/actions.rs` and `mj-tui/src/palette.rs`, add focused tests beside those modules, then run:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    git diff --check

All Cargo tests must run outside the restricted sandbox because this workspace exercises sockets.

## Validation and Acceptance

Opening a session row's menu must show the session name in the top border. Changed files appears first; Organize contains Rename, Pin, and Unpin; Lifecycle contains container settings when applicable, Move, Suspend, and Restart; Destroy appears after a divider. Search, Recent, command count, and Run are absent. An unpinned session shows Unpin muted with a reason and keyboard navigation skips it; a pinned session disables Pin and enables Unpin. The global F2 palette remains searchable and keeps Recent entries.

## Idempotence and Recovery

The change has no persistent-data or protocol effects. Tests and formatting can be rerun safely. If validation exposes a regression, adjust only the session-only branch or the command availability functions; do not remove shared disabled-row enforcement.

## Artifacts and Notes

The first focused run identified three tests that encoded the old searchable session-menu labels and interaction. They were updated to exercise the compact menu while retaining the global-palette assertions. The final focused TUI run passed 699 active tests with two ignored, and the subsequent full workspace suite passed.

## Interfaces and Dependencies

No public API, database revision, configuration, or dependency changes are required. The implementation reuses `CommandSpec`, `Availability`, `Dialog`, and `ChoiceList::render_with_rows`.
