# Add saved terminal UI themes

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Users can choose Midnight (the current appearance), Light, or Dracula in F7 Setup → Theme and save with Ctrl-S. The dashboard, conversation, menus, and workspace picker use the saved colors without restarting. Existing configuration retains Midnight. Cancelling Setup or failing to save preserves the active appearance.

## Progress

- [x] (2026-09-08) Inspected shared terminal styling, Setup editing and supervised persistence, configuration compatibility, and transcript caching.
- [x] (2026-09-08) Added the saved theme type, configuration version 5 migration, and Setup choices.
- [x] (2026-09-08) Replaced fixed color reads with a shared selectable palette, scoped dashboard/workspace-picker rendering, and invalidated conversation and reviewer rows on theme changes.
- [x] (2026-09-08) Verified save/cancel/failure behavior, persistence, rendering, contrast, and cache refresh. Full Cargo tests, Clippy with warnings denied, formatting, and diff checks pass.
- [x] (2026-09-08) Reviewed and staged the validated implementation for the required commit on the current branch, `master`.
- [x] (2026-09-08) Validated the integration with origin's newer mouse controls: the full Cargo suite and Clippy pass on the combined tree.
- [x] (2026-09-08) Validated integration with GitHub master for the user-requested `ui-themes` branch and PR: the full Cargo suite and Clippy pass again.

## Surprises & Discoveries

The web viewer has no Setup modal. The requested control exists in `mj-tui/src/setup.rs`, so this feature applies to the terminal UI. Chat stores colored transcript rows in `TranscriptRenderCache`; changing chrome alone would leave existing messages in the old colors.

Reviewer conversations maintain a separate styled-row cache in `mj-chat/src/hel_chat/second_opinion.rs::ReviewerPane`. Both caches now include the theme. Shared control styles were also constants and must be evaluated at render time. Dracula's red was brightened so status text maintains at least a 4.5 contrast ratio on every painted panel background.

The full-height Setup modal covers all underlying panels at the 100-column test size. Rendering assertions must inspect modal text against its raised surface while Setup is open, and panel text against its normal surface after dismissal. The first suite run exposed incorrect assumptions in the new assertions; correcting them made all seven Setup tests pass without changing production rendering.

The requested push encountered newer mouse-control work on origin/master. Integration preserves the structured clickable footer and the extracted workspace controls, wraps their complete rendering in the selected theme, and converts the added sidebar/workspace control colors to palette reads. The merge required conflicts to be resolved in the shared theme helpers, dashboard footer, and workspace selector.

The combined tree passed `cargo test` and `cargo clippy --all-targets -- -D warnings`, including all theme tests and the mouse-control behavior tests. The integration preserves both features without rebasing or changing branches.

In this environment, origin is a workspace repository proxy and its commits can differ from GitHub master. Origin rejected updating master while its working directory contained staged changes. The user then explicitly requested a branch and PR, authorizing creation of `ui-themes`. Publishing that branch to GitHub required pushing to the repository's HTTPS URL as well as origin. GitHub master also needed merging; its multi-repository bundle wizard was preserved and its added colors converted to palette reads. The PR description identifies inherited local UI commits that GitHub master did not contain.

The integration with GitHub master passed the full Cargo suite and Clippy with all targets and warnings denied. The feature and its inherited UI updates are reviewed in PR #978.

## Decision Log

- Decision: Provide three built-in palettes: Midnight, Light, and Dracula, with Midnight as the default.
  Rationale: Preserve the current appearance while offering both a light option and another distinct dark option without introducing theme files or dependencies.
  Date/Author: 2026-09-08 / Codex.
- Decision: Apply changes after successful Setup save using the existing background persistence path.
  Rationale: Match the modal's existing draft, cancel, conflict, and failure semantics.
  Date/Author: 2026-09-08 / Codex.
- Decision: Select colors within synchronous render scopes and include the theme in transcript cache validity.
  Rationale: Avoid global mutable preferences leaking between clients or concurrent tests, while keeping shared rendering helpers consistent.
  Date/Author: 2026-09-08 / Codex.
- Decision: Advance the configuration schema to version 5 and salvage recognized themes in newer read-only configurations.
  Rationale: Older builds deny unknown configuration fields; their existing future-version recovery must activate when a theme is saved.
  Date/Author: 2026-09-08 / Codex.

## Outcomes & Retrospective

The feature is complete: Setup offers Midnight, Light, and Dracula; saving updates the terminal palette immediately and persists it across restarts. Dashboard chrome, controls, chat, reviewer conversations, animations, and the workspace picker share the selected colors. Cancelling or failing to save preserves the active theme. Existing configurations keep Midnight and migrate without being rewritten on load. Full Cargo tests and Clippy pass. The web viewer remains outside this terminal Setup feature. No dependencies were added.

## Context and Orientation

`src/hel_config.rs` defines `HelConfig`, persisted as TOML, with a schema version and best-effort read-only loading for future versions. `mj-tui/src/setup.rs` edits a JSON draft of that configuration; `setup/schema.rs` supplies labels, help, and choices. `mj-cli/src/dashboard/io.rs` saves the draft in a supervised worker and refreshes dashboard and chat context on success. `mj-chat/src/theme.rs` currently owns twelve fixed color roles used by both `mj-chat` and `mj-tui`. `mj-tui/src/combined.rs::render_combined` paints the main terminal frame. `mj-cli/src/workspace_selector.rs` owns a separate workspace picker. `mj-chat/src/hel_chat/transcript.rs` caches styled lines for conversation rendering.

## Plan of Work

First add a serializable `UiTheme` enum with a canonical choice list and labels, a defaulted `HelConfig.theme` field, and schema-version migration. Include the field when recovering newer configurations and in explicit configuration test fixtures. Add a Theme choice in Setup with help that explains when changes apply.

Then define a `Palette` containing the existing semantic roles in `mj-chat/src/theme.rs`. Add palette lookup and a synchronous `with_theme` scope that restores the previous thread-local selection even on panic. Convert color references across terminal renderers to palette fields. Select the configured theme at main-frame and workspace-picker boundaries. Include the selected theme in transcript cache validity so existing messages redraw correctly.

Finally add behavior tests that change themes through Setup input events and verify rendering after save, cancellation, and failure; cover TOML persistence and old configuration defaults; verify existing cached conversation lines receive new colors. Run the required checks, review the complete diff, and commit only this task's files.

## Concrete Steps

Work in `/workspace/mjolnir`. Use `rg` to locate remaining fixed theme constants and explicit configuration constructors. Run:

    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings

The environment has unrestricted filesystem and socket access, so no sandbox override is needed. Tests must finish successfully and Clippy must report no warnings. Record actual results below after running them.

## Validation and Acceptance

An older TOML file without a theme loads as Midnight and changes only when saved. Every supported theme survives a save/load cycle without replacing other settings. Opening F7 Setup and choosing Light leaves the applied configuration unchanged until Ctrl-S succeeds. Success changes rendered backgrounds and text; cancel and save failure retain the previous theme. Cached conversation content changes colors on the next draw. The workspace picker follows the saved theme. Manually, run `cargo run -p brokk-mjolnir --`, open F7, choose Theme, select a theme, Enter, then Ctrl-S and observe the updated palette.

## Idempotence and Recovery

Configuration loading remains non-mutating. Tests use temporary directories and synthetic terminals. Re-run checks after fixes; do not alter user configuration for validation. Stage only files changed for this feature, commit to the current branch, and do not push.

## Artifacts and Notes

The focused Setup validation completed with:

    cargo test -p brokk-mj-tui setup::tests::
    test result: ok. 7 passed; 0 failed; 0 ignored

Final validation succeeded with:

    cargo test
    All default workspace member unit, integration, and documentation suites passed.
    Existing ignored tests remained ignored.

    cargo clippy --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.21s

    cargo fmt --all -- --check
    git diff --cached --check
    Both checks completed successfully without output.

## Interfaces and Dependencies

Use existing serde, ratatui, and standard-library facilities. Define `hel::hel_config::UiTheme` and `HelConfig.theme`. Expose `mj_chat::theme::palette()` for the current `Palette`, `palette_for(UiTheme)` for explicit lookup, and `with_theme(UiTheme, impl FnOnce() -> R)` for synchronous rendering. Do not hold a theme scope across asynchronous work. No new crates or dependencies are needed.

Revision note: Final update records the completed behavior, passing full-suite validation, and reviewed changes staged for the required current-branch commit.

Revision note: Delivery integration records the newer upstream mouse controls and successful full validation of the combined tree before the authorized push.

Revision note: Records the user-authorized branch/PR workflow, the distinction between workspace origin and GitHub master, and successful validation of their integration for PR #978.
