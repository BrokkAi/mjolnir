# Collapse Sessions to their summary rows and release v2.1.3

This ExecPlan is a living document maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Alt-G promises a compact dashboard, but minimized Sessions currently switches to a separate multi-column grid. After this change, minimizing Sessions keeps the familiar vertical list and shows only each session's existing top summary line. The `You:` and `Agent:` preview rows disappear while minimized, and restoring the pane shows the current full rows unchanged. The validated repair will ship as patch release v2.1.3.

## Progress

- [x] (2026-09-07 17:20Z) Traced the Alt-G state change and the minimized grid rendering path.
- [x] (2026-09-07 17:31Z) Replaced minimized grid rendering with one-line vertical session rows and updated behavior tests and README.
- [x] (2026-09-07 18:05Z) Passed all 350 TUI tests and diagnosed the existing macOS CI failure as a canonical-path assertion bug.
- [ ] Run full tests, Clippy, release builds, license checks, and release-version checks.
- [ ] Commit the behavior change, prepare and commit v2.1.3, push master, verify CI, tag, and verify the release workflow.

## Surprises & Discoveries

- Observation: Commit `6e88b976` changed only the Alt-G pane-size state. `render_sessions` still substitutes `render_sessions_grid` whenever Sessions is minimized.
  Evidence: `mj-tui/src/render.rs` returns from `render_sessions_grid` before laying out the ordinary session rows.
- Observation: Project headings share a table row allocation with the first session beneath them, so mouse hitboxes can span the heading plus the one-line session even though no message previews are rendered.
  Evidence: The focused Alt-G render test shows no `You:` or `Agent:` text and passes after asserting rendered behavior rather than equating hitbox height with content height.
- Observation: The macOS CI failure compares a canonicalized stored project directory (`/private/var/...`) with the non-canonical temporary path (`/var/...`).
  Evidence: Job 101815217520 failed only `first_launch_creates_a_workspace_and_local_session_without_terminal_input`; startup intentionally canonicalizes the directory before storing it.
- Observation: The full suite retained one integration assertion that a short minimized Sessions selection surface was exactly one grid row high.
  Evidence: `dashboard::tests::short_bordered_grid_can_be_selected_and_copied` expected height 1; the vertical list correctly exposes the two content rows allocated on a short frame.

## Decision Log

- Decision: Minimized Sessions will use the existing vertical list and the existing `session_top_line`; standard and maximized rendering will remain unchanged.
  Rationale: This directly implements the requested behavior: keep the first line and remove only the `You:` and `Agent:` rows.
  Date/Author: 2026-09-07 / Codex

## Outcomes & Retrospective

Work is in progress.

The TUI behavior milestone is complete: minimized Sessions uses the existing first summary line in a vertical, scrollable list; standard and maximized rendering remain unchanged. Focused TUI tests pass. Full repository and release validation remains.

## Context and Orientation

`mj-tui/src/lib.rs` owns `PaneSize` state and maps Alt-G to minimized Sessions, Targets, and Quota. `mj-tui/src/render.rs` lays out and draws Sessions. Its ordinary row renderer already has `session_top_line`, followed by the `You:` and two agent-preview rows. Its minimized path currently bypasses those rows for a three-column grid. `mj-tui/src/combined.rs` calculates band heights. `tests/e2e/reliability_lab.py` contains the repaired CI reliability fixture already committed locally as `70162eab`.

## Plan of Work

Change the Sessions row laydown so callers can request summary-only rows. Use that mode when the Sessions pane is minimized, preserving project headings, selection, scrolling, transitions, hitboxes, and the exact existing first-line content. Calculate minimized height from those one-line rows, bounded so the conversation retains useful space. Remove or retire the grid-only code and adjust tests to assert the user-visible absence and restoration of `You:` and `Agent:` lines. Then follow `RELEASING.md`: validate the clean release commit, bump the workspace version to 2.1.3, synchronize manifests and lockfile, regenerate licenses, validate again, push master, wait for green CI, tag v2.1.3, push the tag, and verify release publication and registry workflows.

## Concrete Steps

Work from `/home/ryan/code/mjolnir`. Run `cargo fmt --check`, focused `cargo test -p brokk-mj-tui`, the reliability scenario, `cargo test`, `cargo clippy --all-targets -- -D warnings`, release builds, license generation/checks described by CI and `RELEASING.md`, and `node scripts/release-version.mjs check v2.1.3` on the clean release commit.

## Validation and Acceptance

At standard size, a rendered session contains its summary line, `You:`, and agent preview rows exactly as before. After Alt-G, the same pane contains the same session summary line but no `You:` or `Agent:` preview lines, and the conversation area is taller. Pressing Alt-G again restores the original rendered rows and geometry. All required repository and release validations pass before tagging.

## Idempotence and Recovery

Formatting and validation commands are safe to repeat. Do not move or recreate the existing v2.1.2 tag. If release validation or CI fails, fix master and repeat validation before creating v2.1.3. Do not tag a failing commit.

## Artifacts and Notes

The existing v2.1.2 tag points to `5de829da`, whose CI failed. The local smoke-fixture repair is commit `70162eab` and must be included in v2.1.3.

Plan revision note (2026-09-07 18:05Z): Recorded complete focused TUI validation and the macOS canonical-path CI diagnosis and repair.

## Interfaces and Dependencies

No new dependency is needed. Keep `DashboardState`, `PaneSize`, and persisted workspace layout formats compatible. The change is confined to TUI layout/render behavior and its tests, plus the release metadata required by `RELEASING.md`.
