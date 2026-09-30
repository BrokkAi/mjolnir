# Consistent spacing and wrapping in Create, Open, and Move

This living ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

Create, Open/Sessions, and Move should have one blank terminal cell inside the top and side borders, no blank row below bottom buttons, a blank row between distinct controls or information blocks, and one reserved blank row above bottom buttons. Generic navigation instructions disappear. Long text and repository paths wrap consistently while controls stay reachable on small terminals.

## Progress

- [x] (2026-09-30) Inspect dialog renderers and wrapping implementations.
- [x] (2026-09-30) Apply shared padding and wrapping to wizard and session-picker renderers, including focus-aware scrolling in the project chooser.
- [x] (2026-09-30) Spacing and wrapping coverage passes full dev-profile Cargo tests and Clippy outside the sandbox.
- [x] (2026-09-30) Add underlined CPU and MEM cells, an EC2 instance dropdown, and event-driven editing/submission coverage.
- [x] (2026-09-30) Validate final sizing/capacity behavior: 897 TUI tests passed, 2 ignored; full workspace suite passed; final workspace Clippy and formatting checks passed.
- [x] (2026-09-30) Review task changes and prepare a validated checkpoint on hel3.
- [ ] Choose and implement the newly requested single-choice navigation, preserving configuration access.

## Surprises & Discoveries

The existing styled-line wrapper uses textwrap for single-span lines and a separate grapheme wrapper for multi-span lines. Dialogs additionally use Ratatui paragraph wrapping. Existing rendering tests explicitly expect navigation hints and must be updated to assert their removal while preserving meaningful status and actions. Added whitespace exposes the project chooser’s old fixed-row layout on 60×20 terminals; using one logical body and the existing focus-aware viewport makes every control reachable while the footer stays fixed. An initial wizard run passed 135 of 136 tests; the remaining expectation assumed an off-focus action was always visible and now verifies that Tab reveals it.

## Decision Log

- Decision: Keep the broad declarative dialog layout feature in deferred issue #1205. Use a small padding helper and existing widgets for this task.
  Rationale: The user requested concrete presentation fixes today, with the broader redesign deferred.
  Date/Author: 2026-09-30 / Codex.
- Decision: Use the shared styled-line wrapper for displayed dialog text and unify its plain/styled wrapping path. Preserve separate editable-text cursor layout, which intentionally mirrors the input renderer.
  Rationale: Displayed text needs consistent word breaks and measured heights without changing editing behavior.
  Date/Author: 2026-09-30 / Codex.

## Context and Orientation

`mj-chat/src/components/dialog.rs` contains persistent interaction state and `DialogShell` layout helpers. `mj-chat/src/chat/rendering.rs` exposes `wrap_styled_line`, including source-span mapping used by chat. `mj-tui/src/wizards/render.rs` draws Create and resume/Move, sharing review and attachments. `picker.rs` draws profile/target/bundle pickers, `projects.rs` the Create project chooser, and `move_files.rs` transfer selection. `mj-tui/src/resume.rs` draws the Open/Sessions picker and derives pointer/preview geometry from the same layout functions. Rendering tests live in wizard and resume module test files.

## Milestones

First add the shared padded inner rectangle and displayed-text wrapping helper. Remove the single-span alternate algorithm after adding a regression proving style segmentation does not change word breaks. Then update all named screen renderers, accounting for wrapped text in heights and focus-row positions. Groups of table rows and repository entries remain coherent lists; separate widgets and information blocks gain blank rows.

Next update the existing hint tests and add buffer-based assertions for padding, footer position, separation, wrapping, and keyboard access at constrained sizes. Run from `/home/jonathan/Projects/mjolnir3`: `cargo fmt --all -- --check`, elevated `cargo test`, and elevated `cargo clippy --all-targets -- -D warnings`. Do not change Cargo output directories or bypass mbx. Tests use their existing isolated directories; any manual CLI/daemon invocation must use `--instance dialog-spacing-test`.

Finally inspect `git diff --check` and the complete diff, record validation and outcomes here, and stage explicit task files. Commit on the existing branch without pushing.

## Validation and Acceptance

Rendered buffers must show a blank row under the top border and a blank cell between body text and each side border. Bottom buttons occupy the row immediately above the bottom border, with one reserved blank row above them. Distinct fields and information blocks are separated. Generic select/Tab/Enter boilerplate is absent; errors, warnings, specialized action hints, and action labels remain useful. Wrapping plain and styled versions of identical prose or long paths yields identical rows, preserving every grapheme and style/source mapping. Focused fields remain visible as content grows or the terminal shrinks. Existing interaction, move, and preview-scroll tests continue to pass.

## Outcomes & Retrospective

The requested screen renderers share padding and displayed-text wrapping. The alternate textwrap path and dependency have been removed. Generic navigation hints are removed, including path-completion popup hints. The full workspace suite passed. After the final capacity-update correction, all 897 TUI tests passed (2 ignored), and workspace Clippy plus formatting checks passed. The new field controls support Tab, clicks, paste, exact GiB conversion, validation, and retained drafts. EC2 uses the chosen instance dropdown. Background capacity updates share field validation and preserve typed values. The dialog presentation and resource-editor work forms a validated checkpoint. Single-choice navigation is a separate extension pending the user’s configuration-entry preference.

## Editable sizing extension (2026-09-30)

The user approved replacing SIZE with CPU and MEM (GiB), using separate underlined TextField controls in the selected container row. Both fields must remain visible while focused, accept Tab and clicks, and register cursor geometry from the same layout that draws the table. CPU is a positive integer; MEM is positive decimal GiB with checked conversion to bytes. Invalid or above-host-limit entries remain editable, show an error, and disable Next. Existing allocations must round-trip exactly, including non-integral GiB host limits. Resource edits invalidate remote creation preflight and move preparation. Keep defaults and remembered sizing and retain edits when navigating back.

The user explicitly chose an instance-type dropdown for EC2. CPU and MEM remain read-only for EC2, and each dropdown option identifies the instance type and its resources. Catalog queries stay in existing background tasks. Preserve available selected types during refresh, reject removed selections, and cache stale-target results without changing the active target. Bare targets show dashes with no sizing controls. Remove the old sizing shortcuts and instructions. Reuse TextField, ComboBox, Form and WizardDraft, without protocol or store changes. The broader dialog body redesign remains deferred in issue #1205.

Implementation milestones are shared sizing state and validation in mj-tui/src/wizards/resources.rs, controls and interaction in dashboard.rs/draft.rs, and target table drawing in picker.rs/render.rs. Extend TextField with an opt-in underline style so other editors retain their appearance. Add event-driven editing, submission, EC2 selection and constrained-terminal rendering tests. Repeat cargo fmt --all -- --check, elevated cargo test -- --quiet and cargo clippy --all-targets -- -D warnings, then commit explicit task files on hel3.

Revision note: extend the plan with the approved CPU/MEM and EC2 behavior, and record successful validation of the earlier spacing work.

Review discovery: mj-tui/src/ingest.rs silently clamped wizard allocations whenever a background capacity sample arrived. Replace that path with refresh_wizard_resource_limits, which calls the same field validator used by editing. Retain draft text, invalidate preflight or move preparation if the resolved allocation changes, and return an oversized draft to Target with its error. Defaults are still initialized through existing remembered/default sizing. Also preserve whitespace when truncating descriptive table cells. The focused wizard suite passed 134 tests before this final ingestion regression was added. The final all-TUI run passed 897 tests with 2 ignored; Clippy passed after unused imports from the removed clamp were cleaned up.

Revision note (2026-09-30): record completed sizing behavior and validation. The user has additionally requested skipping single-choice Profile/Target steps and skipping Confirm when both are single-choice. That follow-on navigation change awaits their choice between an optional Configure dialog and an expandable Options section; the currently validated dialog work is a coherent checkpoint before that extension.
