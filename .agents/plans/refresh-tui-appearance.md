# Refresh Mjolnir's terminal appearance and restore activity spinners

This ExecPlan is a living document maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture


Mjolnir's terminal UI should feel polished and alive. Users will see a coordinated midnight palette, softly rounded panels, readable focused controls, and the configurable animated spinners that existed before the Hel rewrite. The session navigator, conversation, composer, dialogs, and resource summaries should look like one application. Existing navigation and mouse selection must continue to work, including on small terminals.

## Progress


- [x] (2026-09-07) Created isolated worktree `.mjolnir/worktrees/tui-sparkle` from `b4e7debb`, then branch `agent/tui-sparkle` after the user requested a branch push and PR.
- [x] (2026-09-07) Inspected the dashboard, shared controls, chat rendering, screenshot generator, and spinner history; identified the Hel rewrite as the removal point.
- [x] (2026-09-07) Implemented shared theme across dashboard, conversation, composer, workspace picker, forms, dialogs, help, and palette. Added padded conversation/composer interiors and aligned shortcut columns.
- [x] (2026-09-07) Restored six configurable spinner styles, F2 preference cycling with supervised persistence, config migration, and active-only animation timing.
- [x] (2026-09-07) Reviewed dashboard, palette, wizard, and active conversation captures at 110×40, 80×24, and 32×20. Full workspace tests and Clippy passed; the live tmux harness passed 79 checks with seed 523.
- [x] (2026-09-07) Final narrow-footer checks passed (3 chat and 10 TUI tests), along with final Clippy, rustfmt, and diff checks.
- [x] (2026-09-07) Committed implementation as `ea3ce678`, pushed `agent/tui-sparkle`, and opened https://github.com/BrokkAi/mjolnir/pull/977.

## Surprises & Discoveries


The previous spinner implementation was removed by commit `cc9fd10d`. The current chat has no activity animation and dashboard loading dialogs use four ASCII frames. Existing screenshot generation uses the real Ratatui test backend, but its SVG color encoder drops RGB colors, so it must be updated to represent the new palette faithfully.

Visual review caught long command-palette shortcut labels running into command names; the final design aligns shortcuts to the right with an explicit gap. Adding horizontal breathing room to the transcript required using the actual padded content rectangle for wrapping, selection, and scrollbar geometry, including terminals only two to four cells wide in the component tests. Review status labels remain present after completion, so animation now checks a typed `RuntimeReviewView::is_working()` predicate instead of assuming any review label means ongoing work.

## Decision Log


- Decision: Build the visual vocabulary in `mj-chat/src/theme.rs`, which both chat and dashboard can use.
  Rationale: This is an existing dependency boundary; another crate or duplicated palette would add unnecessary maintenance.
  Date/Author: 2026-09-07, Codex.
- Decision: Use slate/navy surfaces, teal focus, lavender secondary accents, and rounded borders; preserve pane allocation and control hitboxes.
  Rationale: The rough appearance comes from default terminal colors, heavy double rules, and competing emphasis. Shared styles improve the whole surface without disrupting navigation.
  Date/Author: 2026-09-07, Codex.
- Decision: Restore the old spinner vocabulary using the existing frontend timing and configuration paths.
  Rationale: Animation should reveal real activity while idle screens stay inexpensive; the user explicitly requested the former spinners.
  Date/Author: 2026-09-07, Codex.
- Decision: Advance the config schema to version 3, upgrade version 1 and 2 configurations in memory, and serialize appearance saves with an in-flight flag.
  Rationale: Older strict parsers must treat the new preference as a newer configuration instead of rejecting an unknown field at the same schema version. Blocking a second appearance save until completion prevents out-of-order writes while all other UI operations remain available.
  Date/Author: 2026-09-07, Codex.

## Outcomes & Retrospective


The visual refresh and spinner restoration are implemented and visually reviewed. The full workspace test suite, Clippy, and 79 real terminal acceptance events pass. A final narrow-footer refinement keeps complete palette/help hints visible at 32 columns and shares the fitting logic between chat and dashboard. The original checkout remains clean; all source changes and commits belong to the requested worktree.

The completed implementation is published on `agent/tui-sparkle` in PR #977. No task work remains. Visual validation used deterministic renderer captures and a private tmux lab; physical microphone capture and emulator-specific glyph rendering remain outside this appearance task.

## Context and Orientation


Mjolnir is a Rust workspace. `mj-tui/src/combined.rs` allocates the terminal into sessions, conversation, prompt, targets, quota, and footer. `mj-tui/src/render.rs` draws dashboard content. Other files in `mj-tui/src` draw dialogs and wizards. `mj-chat/src/hel_chat/active.rs` draws the live conversation and composer; `mj-chat/src/components/controls.rs` provides shared form controls. Ratatui is the terminal rendering library. Its test backend stores a screen in memory so the same production drawing code can be inspected without launching external services.

`mj-tui/src/docs_screenshots.rs` contains an explicitly invoked capture test that writes `docs/src/assets/screenshots/*.svg`. This is product imagery, not internal planning documentation. Spinner preferences belong with existing user configuration and animation scheduling belongs in frontend event handling, with no filesystem access during rendering.

## Plan of Work


First add a shared palette and small style/block functions in `mj-chat/src/theme.rs` and export it from `mj-chat/src/lib.rs`. Apply the vocabulary to chat, controls, modal backgrounds, dashboard panels, session emphasis, resource bars, empty states, and keyboard hints. Preserve all geometry that determines mouse hitboxes and selectable text unless updated together.

Restore historical spinner styles in a shared rendering module, persist the selected style with the existing configuration, and use the existing UI wakeup path to advance active animations. Replace crude loading indicators and show activity in the live conversation. Keep shutdown and in-flight background operations responsive and report existing errors normally.

Update the SVG capture encoder to support RGB colors. Generate captures through real UI rendering, inspect the resulting visuals, and refine any clipping, contrast, or spacing issues. Use focused behavior tests where state or animation scheduling changes, then run the required workspace checks. Commit only task files, push `agent/tui-sparkle`, and open a PR with a concrete description and validation evidence.

## Concrete Steps


Run commands from `/home/ryan/code/mjolnir/.mjolnir/worktrees/tui-sparkle`. Every `cargo test` runs with escalated permissions because socket-based tests are invalid in the restricted sandbox. Cargo output stays in ordinary build storage; when reusing the existing checkout's build cache set `CARGO_TARGET_DIR=/home/ryan/code/mjolnir/target` explicitly.

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo test -p brokk-mj-tui generate_documentation_screenshots -- --ignored --nocapture
    MJ_CHAT_CAPTURE_PATH=/tmp/mj-chat-preview.json cargo test -p brokk-mj-chat capture_chat_preview -- --ignored
    git diff --check

Expected results are passing tests, warning-free Clippy, and regenerated captures depicting the production UI. Stage changed paths explicitly and commit them to `agent/tui-sparkle`; do not stage unrelated files. Push with `git push -u origin agent/tui-sparkle`, then use `gh pr create` with a description prepared in a temporary file and passed through `--body-file`.

## Validation and Acceptance


At normal terminal sizes the dashboard and dialogs share rounded slate borders, teal focus, readable subdued metadata, and distinct selected rows. The composer and transcript match. Small terminal layouts still keep key instructions and pane sizing accessible. Busy conversations display advancing historical spinner frames; idle conversations avoid unnecessary animation. Loading and cancellation remain responsive. Existing state and mouse selection tests pass, with changed visual assertions reflecting the new intended appearance. Screenshot captures must use the actual RGB palette rather than a substitute color table.

## Idempotence and Recovery


All edits are isolated in the named worktree and branch. Test and capture commands can be repeated safely. Do not delete or reset existing worktrees. If validation fails, fix the relevant task code and rerun the affected tests before broader checks. Do not publish a branch or PR until validation has a clear outcome.

## Artifacts and Notes


Initial checkout was clean at `b4e7debb` (`Prepare v2.1.3`). Historical spinner commits include `ee204ccf` (scan default), `fb09403f` (refined animations), and `bb75da33` (globe and colors). Final validation transcripts and PR reference will be recorded here as available.

The isolated chat suite passed 431 tests. Spinner, config migration, preference persistence, animation gating, and review activity tests passed 25 focused checks. Full workspace test output is retained in `/home/ryan/code/mjolnir/target/tui-sparkle-tests.log`; generated screen previews are disposable review artifacts under `/tmp`, not Cargo build output.

The final full workspace run passed chat (433), controller (684), core (841), TUI (353), worker (108 plus 4 executable tests), CLI (202), and the active integration/PTY tests. An unrelated web-viewer retry timeout occurred in the first loaded run and passed both its isolated rerun and the complete second run without a production change. Clippy passed with raw RGB constructors narrowly scoped to the paired foreground/background palette, scan gradient, and screenshot encoder; the lint still rejects scattered raw colors in call sites.

The last footer refinement passed 13 focused tests, including actual 32-column rendering and every width from 0 through 80. Final Clippy and formatting checks passed after that refinement.

Live acceptance evidence is `target/reliability-artifacts/tui-components-seed-523-2156836/live-evidence.json` in this worktree, with 79 passed recorded events. The harness was brought up to date for existing automatic workspace entry and launch readiness before rename, in addition to updating selectors for the new visual chrome. It used disposable fake ACP sessions and left the user's workspace and providers untouched.

## Interfaces and Dependencies


Use the existing Ratatui dependency and workspace crate relationships. `mj_chat::theme` exposes palette constants `BACKGROUND`, `SURFACE`, `SURFACE_RAISED`, `TEXT`, `MUTED`, `BORDER`, `ACCENT`, `SECONDARY`, `SUCCESS`, `WARNING`, `ERROR`; `base()`, `muted()`, `border(bool)`, `title(bool)`, and `selection(bool)` return `Style`; `panel(bool)` and `modal()` return owned `Block` values. No new workspace crate is required.

Revision note (2026-09-07): Created the plan after inspecting production rendering and locating the removed spinner implementation; recorded the user's subsequent authorization to push and create a PR.

Revision note (2026-09-07): Recorded completed implementation, source-level animation gating and configuration decisions, and visual review findings before final validation.

Revision note (2026-09-07): Recorded full passing workspace/lint/live validation and the compact-footer issue caught during final screenshot review.

Revision note (2026-09-07): Recorded the implementation commit, published branch, and requested pull request after successful delivery.
