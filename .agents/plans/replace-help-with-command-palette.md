# Replace static TUI help with the command palette

This ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

Users should search for a command and run it from the same place they discover it. Remove the non-interactive terminal help page and make its existing shortcuts open the searchable command palette. Both the default prefix followed by `?` and the prefix followed by `:` must reach the same menu, with no separate Help entry.

## Progress

- [x] (2026-10-27) Confirm the existing distinction: Help opens a static reference while Palette opens the searchable command menu.
- [x] (2026-10-27) Clarify scope from the user's correction: remove the static page entirely rather than preserve it inside the palette.
- [x] (2026-10-27) Independently review input boundaries and identify the existing modal guard, legacy-key alias handling, shared reason formatting, and CLI help-search cleanup requirements.
- [x] (2026-10-27) Remove the help mode, renderer, command entry, CLI help-search coordinator, and obsolete goldens; route shortcuts to the palette.
- [x] (2026-10-27) Preserve saved key configurations as aliases; focused decoded-config, punctuation, simple-mode, modal-state, and palette-golden tests pass.
- [x] (2026-10-27) Review focused routing/render coverage and resolve custom/disabled alias findings, including composer hint/click routing.
- [x] (2026-10-27) Full four-crate validation passes: 1,806 passed, 12 ignored; workspace Clippy, formatting, and diff checks pass.
- [x] (2026-10-27) Commit the implementation as 9836a9c79 on the current branch.
- [x] (2026-10-27) Verify the merged AFK golden passes and deliver the current branch to origin/master; this plan update accompanies the delivery merge.

## Surprises & Discoveries

`KeysConfig` serializes the existing Help and Palette bindings, so changing default values alone leaves saved configurations on the previous behavior. Preserving the old key action as an input alias for the single palette command avoids a config migration. An initial interpretation preserved the static reference as a Help menu item; the user explicitly rejected that scope.

The old Help overlay could preserve an underlying dialog, while `begin_palette` replaces the current mode. Do not inherit Help's unconditional modal allowance: both shortcut aliases must use the existing palette guard. Removing the module also removes a shared availability-reason formatting helper and CLI asynchronous help-search plumbing, which need a real relocation/removal rather than compatibility stubs.

Final review exposed that custom direct aliases must not be advertised beneath a shared prefix heading. Keep binding trigger information through the shared label interpretation; use full labels for unknown-prefix recovery, prefix-only aliases in the pending-prefix banner, and accurate direct/prefix footer grouping. A disabled command must not produce an invented `?` banner shortcut.

## Decision Log

- Decision: Remove the static Help UI and its command rather than maintain a second discovery surface.
  Rationale: The user wants the interactive command palette to replace the non-interactive page completely.
  Date/Author: 2026-10-27, user and Codex.
- Decision: Existing `[keys] help` values become silent compatibility aliases for the palette, including custom old Help chords.
  Rationale: Saved configurations must continue to load and their old entry points must reach the replacement. There is one visible command and no versioned migration for two key fields.
  Date/Author: 2026-10-27, Codex.

## Outcomes & Retrospective

The old static reference, its visible command, and its asynchronous CLI search coordinator have been removed. Both shortcut families now reach the existing palette. Custom legacy aliases route and render accurately, disabled keys produce no invented shortcuts, and both dashboard and composer footers show the same binding data with matching click targets. Independent review confirmed the old UI is absent and modal guards remain intact. The earlier approach that kept shortcut help was discarded without a config migration.

## Context and Orientation

The TUI is the terminal user interface. `mj-core/src/config/keys.rs` declares named key actions and saved key bindings, including the prefix key (default Ctrl+B). `mj-tui/src/keybinds.rs` translates keys into command identifiers. `mj-tui/src/actions.rs` owns command metadata and dispatch, which also drives the searchable menu in `mj-tui/src/palette.rs`. The current static reference lives in `mj-tui/src/help.rs` and the Help mode in `mj-tui/src/lib.rs`, with associated render and event branches elsewhere in the TUI. Search references before removing the mode so modal handling, click ownership, scroll state, and tests remain coherent.

## Plan of Work

Replace Help key dispatch with Palette dispatch and move any relevant pane-local question-mark hint onto the palette command. Retain legacy key configuration as an accepted alias, but remove the separate Help command, mode, renderer, overlay state, and related event handling. Aggregate binding labels through the existing registry/key helpers so footer hints and command search agree with actual routing. Opening the palette must preserve the existing safeguards around dialogs and must not consume ordinary `?` or `:` typed into a composer or terminal.

Remove obsolete help-only render goldens and tests. Extend existing command discovery and shifted-punctuation routing coverage to demonstrate that both entry points run the same menu. Keep config compatibility coverage at the file decode/routing boundary. Update relevant key-binding documentation to describe command search. Do not add broad migration infrastructure, a replacement static page, or unrelated command-menu features.

## Concrete Steps

Work from the repository root on the current branch. Use the existing Cargo shim, which already uses mbx. Do not redirect target storage or change build settings. Run focused tests for changed routing, config decoding, and render goldens while iterating. Once the final diff is reviewed, run the full suites of touched crates and workspace Clippy, using package names such as:

    cargo test -p brokk-mj-core -p brokk-mj-chat -p brokk-mj-tui -p brokk-mjolnir
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

Run full suites once after the final substantive changes. If a test fails, rerun only the failing test after correcting it. Any live binary invocation must use an isolated named instance and its own data. Stage only this task's files, commit on the current branch, and push HEAD to origin/master. If upstream moved, fetch and merge it, resolve conflicts, and push without rebasing.

## Validation and Acceptance

With default keys, Ctrl+B followed by either `?` or `:` opens the same searchable command palette. Existing saved Help bindings also open that palette. The old static Help page and Help menu entry are absent. Existing command search selects and runs actions, including Go AFK. Plain punctuation remains ordinary input in composing and terminal surfaces. Existing modal and draft preservation checks continue to pass. Touched-crate test suites, workspace Clippy with warnings denied, formatting, and the final diff check must pass.

## Idempotence and Recovery

Config loading must not require an eager disk rewrite. Old bindings may be accepted indefinitely as aliases, with no second UI implementation. Keep tests isolated from the running daemon and live store. Preserve unrelated working-tree changes and upstream work. Use normal merges for upstream movement.

## Artifacts and Notes

The implementation child reports under the session's `.mj-agents/602c26d5b00fb0cbb3cb62fffd70f61f` directory. Record decisive review and validation results in this plan before delivery.

The final four-crate test command passed with 1,806 passing tests and 12 ignored, with no failures. Workspace Clippy with warnings denied, formatting, and the diff check also passed. Intermediate full runs exposed stale footer snapshots; all affected Linux/macOS goldens were updated and the final suites passed. Focused coverage includes decoded legacy configurations, punctuation encoding, simple-mode aliases, modal draft preservation, dashboard and composer footer grouping, and direct command click indices.

After implementation commit 9836a9c79, merge upstream d5311066c before delivery. Its independent AFK dialog styling and GitHub-watch changes merged cleanly; only the affected AFK render golden needs a targeted check against the combined styling and footer, with no broad suite repetition.

The integrated-tree `golden_operator_away_dialog` test passed with no edits. This confirms the automatically merged snapshot includes both the concurrent AFK styling and unified palette footer.

## Interfaces and Dependencies

Use existing key configuration, `CommandId::Palette`, command registry metadata, and palette rendering. The legacy Help key action may remain as compatibility input but must map to the same visible palette command. Do not introduce dependencies, database migrations, or protocol changes.

Initial plan records the corrected scope and replaces the earlier proposal to retain static shortcut help.

Updated after independent boundary review: recorded modal-state ownership and cross-crate cleanup requirements.

Updated after implementation: recorded the full removal, focused passes, and all four crates requiring final validation.

Updated after final review: recorded required corrections for disabled bindings and custom direct aliases before the final validation round.

Updated after validation: recorded resolved review findings, passing full suites, updated snapshots, Clippy, and formatting.

Updated after committing: recorded the implementation commit and the targeted validation of concurrent AFK styling before the final push.

Final update: recorded the passing integrated golden and delivery of the reviewed, validated implementation.
