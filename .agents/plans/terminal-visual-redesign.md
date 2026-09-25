# Redesign the Mjolnir terminal workspace

This ExecPlan is a living document maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Make the terminal interface feel like one carefully designed workspace: a quiet graphite canvas, clearly distinguished navigation and conversation surfaces, readable session metadata, and obvious focused controls. The user explicitly selected the terminal interface; web and desktop styling are outside this change. Existing keyboard shortcuts, mouse targets, themes, and asynchronous operations must keep working.

## Progress

- [x] (2026-09-25) Inspected terminal rendering, shared themes, input geometry, existing tests, and screenshot generation; confirmed terminal scope with the user.
- [x] (2026-09-25) Implemented shared colors, controls, and surface hierarchy; extended semantic contrast review to selected rows.
- [x] (2026-09-25) Refined workspace navigation, session presentation, and support panes while preserving their geometry.
- [x] (2026-09-25) Refined conversation and composer styling; capture fixture now renders real messages and an unsent draft through an in-memory chat backend.
- [x] (2026-09-25) Inspected all four rendered screenshots; 788 terminal tests, 604 chat tests, semantic contrast checks, formatting, and clippy pass.
- [x] (2026-09-25) Completed workspace validation, documentation tests, all 11 repaired PTY integration tests, and final all-target clippy with warnings denied.
- [x] (2026-09-25) Prepared the validated changes for the required commit in the existing detached checkout, without pushing; this plan is included in that commit.
- [x] (2026-09-25) User subsequently requested a PR, review, fixes, and merge when green. Created `codex/terminal-visual-redesign`, integrated current master, and opened PR #1148.
- [x] (2026-09-25) Independent production and test reviews found one monochrome focus regression. Fixed permanent Create/New emphasis so it remains distinguishable from keyboard focus; added a rendered Up/Right/Enter regression.
- [x] (2026-09-25) Validated the review fix: 789 terminal UI tests, 11 PTY tests after integrating master, formatting, and all-target clippy pass. Merge is authorized after the final checks on [PR #1148](https://github.com/BrokkAi/mjolnir/pull/1148) pass; GitHub records that gate and the merge outcome.

## Surprises & Discoveries

The repository already has five themes, ASCII symbols, `NO_COLOR`, contrast checks, mouse geometry checks, and deterministic SVG captures. The original screenshot fixture painted an empty conversation even though it contained transcript data. The checkout is detached at HEAD; preserve it and commit there rather than creating or switching branches.

The first full test run passed 600 chat tests and found four assertions tied to old visual colors. Keep the generic Conversation heading in primary text and update assertions for the new author and keyboard-hint colors. Review also found that High Contrast and Darcula selection backgrounds did not support every semantic status foreground. Extend the existing contrast test to selected surfaces and correct the palettes rather than dropping status information.

The final broad run passed all crate unit tests, but two PTY tests exposed synchronization concerns. A PTY is a simulated terminal backed by an operating-system process. Ratatui writes only changed cells, so a visible “New workspace” label arrived as `New`, a cursor-position sequence, and `workspace`; searching raw bytes could not find it. The create-session test advanced from database admission and later timed out on quit. Both pass after reconstructing the screen, waiting for complete visible wizard headings, and observing wizard closure before the next action. This does not establish a production shutdown defect; no lifecycle code changed. The raw stream remains available for terminal-mode assertions. A parser regression covers split cursor sequences and UTF-8, retained cells, and erasure across more than 64KB of input.

PR review found that `active_control()` and `focus_control()` intentionally share bold reverse styling in monochrome. Permanently styling Create/New as active therefore made two action buttons appear focused after keyboard focus moved to Open. Restrict that permanent primary accent to color themes; monochrome continues to reserve bold emphasis for the focused or armed action. The regression exercises the real dashboard and verifies both rendered focus and the action activated by Enter.

## Decision Log

- Decision: Restrict the redesign to the terminal after the user's clarification.
  Rationale: This is the interface the user wants to improve.
  Date/Author: 2026-09-25 / Codex.
- Decision: Evolve existing rendering and shared styles without adding a production dependency or changing session lifecycle behavior.
  Rationale: The mature layout and input geometry provide valuable behavior; surface, typography, selection, and spacing can improve them without destabilizing asynchronous control operations.
  Date/Author: 2026-09-25 / Codex.
- Decision: Use neutral graphite and mint for the default Midnight theme, preserving other selectable themes and accessibility modes.
  Rationale: Strong hierarchy and restrained emphasis reduce the current uniform weight of adjacent panels.
  Date/Author: 2026-09-25 / Codex.
- Decision: Preserve all current panels and their positions, as the user explicitly confirmed after asking about the repository's “dashboard” terminology.
  Rationale: This task is a visual redesign of the existing terminal, not a new screen or a change to navigation structure.
  Date/Author: 2026-09-25 / Codex.
- Decision: Add the already locked `vte` parser as a test-only CLI dependency to reconstruct the screen in PTY tests.
  Rationale: Visual changes legitimately alter cursor updates; screen assertions must follow terminal semantics rather than depend on how a label is split into writes. No production dependency or runtime behavior changes are needed.
  Date/Author: 2026-09-25 / Codex.

## Outcomes & Retrospective

Implemented a complete terminal styling pass while preserving the existing pane layout and navigation. Midnight now uses neutral graphite, warm text, mint actions, and blue keyboard and author accents. Session titles, activity, metadata, conversation rows, the composer, tables, dialogs, fields, buttons, and scrollbars share that hierarchy. Light has a coordinated warm palette; Darcula, High Contrast, monochrome, and ASCII remain supported. All semantic text colors satisfy the existing 4.5:1 contrast threshold on every content and selection surface.

The capture fixture now renders an attached conversation with real messages and a draft, making future visual review useful. All four SVGs were regenerated and visually inspected. PTY tests now observe screen cells, so cosmetic redraw changes no longer invalidate their synchronization. Runtime session and shutdown behavior was not changed.

Subsequent PR review fixed monochrome Create/Open focus ambiguity. Independent review found no further actionable regressions. The merged upstream PTY fixture and all terminal UI tests pass, including the new rendered focus transition, and all-target clippy passes on the reviewed source.

## Context and Orientation

`mj-chat/src/theme.rs` owns terminal color palettes and reusable panel, title, selection, and hint styles. `mj-chat/src/components/controls.rs` renders reusable interactive controls. The combined dashboard in `mj-tui/src/combined.rs` allocates rectangles to session navigation, conversation, prompt, targets, and quota; those rectangles also define input targets. `mj-tui/src/render/` renders the supporting panes. `mj-tui/src/workspaces.rs` renders workspace navigation. `mj-chat/src/chat/transcript/render.rs` draws conversation rows and titles, while `mj-chat/src/chat/active/render.rs` draws the composer and conversation regions. A composer is the text input where the user drafts a prompt. `mj-tui/src/docs_screenshots.rs` renders fixtures through the real terminal renderer into SVG files under `docs/src/assets/screenshots/`.

## Plan of Work

First update the shared theme and control styles, retaining explicit foreground and background colors and contrast of at least 4.5:1 for ordinary readable text. Distinguish selected controls, text fields, and panel titles without changing their hit rectangles. Then refine dashboard workspace branding, session headings and metadata, support surfaces, and empty states using those styles. Finally give conversation messages and the composer a consistent hierarchy and generate representative captures using real chat rendering. Keep ASCII and monochrome behavior functional throughout.

Three collaborators own separate areas: shared theme and reusable controls; dashboard and support-pane rendering; conversation rendering and screenshot fixtures. The primary agent integrates and commits changes.

## Concrete Steps

Work from `/home/ryan/.codex/worktrees/4387/mjolnir`. Inspect changed files with `git diff --check` and `git diff --stat`. Run `cargo fmt --all -- --check`. Run all Cargo tests with elevated permissions as required by AGENTS.md, because existing tests use loopback sockets. Use the ordinary dev profile and normal build storage, never redirect Cargo artifacts to `/tmp`.

    cargo test
    cargo clippy --all-targets -- -D warnings
    NO_COLOR= cargo test -p brokk-mj-tui generate_documentation_screenshots -- --ignored --nocapture

The captures are deterministic test data and never connect to a live daemon. Clearing `NO_COLOR` for this command makes the Settings description agree with the explicitly rendered Midnight palette. Any interactive invocation must use `--instance visual-redesign` with isolated configuration/data directories; do not use the host default instance. Existing PTY tests retain their `terminal-regression` named instance and temporary configuration/data roots. Review SVGs as rendered images at their native dimensions. Stage only the files changed for this redesign and commit in the existing checkout.

## Validation and Acceptance

Existing render tests must continue to demonstrate mouse and keyboard operation, narrow-terminal layouts, selection, scrolling, and pane controls. Shared palette contrast tests must pass for every theme. Screenshots must show readable session navigation, a visually dominant conversation, a distinct composer, and consistent modal controls. The new dashboard screenshot should contain actual messages rather than an empty splash. Check representative small terminal sizes with the existing rendering tests; do not add tests that simply enumerate cosmetic implementation details. Full dev-profile tests and clippy should pass, and any unrelated environment failure must be recorded accurately.

## Idempotence and Recovery

Changes affect presentation and test fixtures only. No database, configuration migration, daemon replacement, or process lifecycle changes are required. Screenshot generation can be repeated. Preserve unrelated working-tree changes and never reset the checkout. Stop any temporary preview process before removing its temporary files.

## Artifacts and Notes

Preview artifacts remain the existing dashboard, new-session, command-palette, and setup SVGs under `docs/src/assets/screenshots/`. These are captures of the real renderer, not concept mockups. All four were inspected in Chromium; the conversation capture shows user, agent, and tool messages together with an unsent draft. Existing render tests cover compact layouts and input geometry.

Validation used the dev profile. The broad workspace test run passed every unit suite and every integration suite except the two PTY assertions described above. Documentation tests also passed. The repaired PTY suite passed all 11 tests; its exact final source passed in 5.96 seconds, including the current-screen cancellation assertion. Final `cargo clippy --all-targets -- -D warnings` passed. The ignored screenshot generator passed with `NO_COLOR=`. Formatting and staged/unstaged diff checks passed. Passing production suites were retained rather than unnecessarily repeated after changes confined to the PTY test helper.

## Interfaces and Dependencies

Continue to use Ratatui's `Style`, `Block`, `Line`, `Span`, and existing frame rectangles. Keep semantic styles in `mj-chat::theme` and reusable interactive controls in `mj-chat::components`. Do not add a workspace crate, production dependency, font dependency, daemon request, or blocking operation to rendering. The PTY test helper uses `vte` 0.14, already present in Cargo.lock, as a direct dev dependency only. Any new shared helper must be used by its callers and preserve `UiTheme::Mono` and the configured symbol set.

Revision note (2026-09-25): Created the plan after repository inspection and the user's terminal-only scope clarification. Updated implementation progress, recorded the explicit decision to keep the current panels, and documented validation findings, contrast corrections, completed visual inspection, and terminal-test synchronization repairs.
