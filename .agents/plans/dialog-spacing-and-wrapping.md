# Consistent spacing and wrapping in Create, Open, and Move

This living ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

Create, Open/Sessions, and Move should have one blank terminal cell inside the top and side borders, no blank row below bottom buttons, a blank row between distinct controls or information blocks, and one reserved blank row above bottom buttons. Generic navigation instructions disappear. Long text and repository paths wrap consistently while controls stay reachable on small terminals. CPU and MEM become editable fields, and EC2 uses an instance dropdown. Subagent policy moves into profile setup with Native as the default and a single model as the other choice. Single-profile and single usable raw-target selectors are skipped, with no Confirm when both were skipped.

## Progress

- [x] (2026-09-30) Inspect dialog renderers and wrapping implementations.
- [x] (2026-09-30) Apply shared padding and wrapping to wizard and session-picker renderers, including focus-aware scrolling in the project chooser.
- [x] (2026-09-30) Spacing and wrapping coverage passes full dev-profile Cargo tests and Clippy outside the sandbox.
- [x] (2026-09-30) Add underlined CPU and MEM cells, an EC2 instance dropdown, and event-driven editing/submission coverage.
- [x] (2026-09-30) Validate final sizing/capacity behavior: 897 TUI tests passed, 2 ignored; full workspace suite passed; final workspace Clippy and formatting checks passed.
- [x] (2026-09-30) Review task changes and prepare a validated checkpoint on hel3.
- [x] (2026-09-30) Move subagent policy into profile setup, with Native as the default and Single model as the only alternative.
- [x] (2026-09-30) Skip single-profile selection and single usable raw-target selection; bypass Confirm when both selectors were skipped.
- [x] (2026-09-30) Validate and commit the profile settings and navigation extension on hel3; full dev-profile Cargo tests, Clippy, formatting, and diff checks passed.

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

The requested screen renderers share padding and displayed-text wrapping. The alternate textwrap path and dependency have been removed. Generic navigation hints are removed, including path-completion popup hints. The full workspace suite passed. After the final capacity-update correction, all 897 TUI tests passed (2 ignored), and workspace Clippy plus formatting checks passed. The new field controls support Tab, clicks, paste, exact GiB conversion, validation, and retained drafts. EC2 uses the chosen instance dropdown. Background capacity updates share field validation and preserve typed values. The dialog presentation and resource-editor work forms a validated checkpoint. The follow-on navigation extension uses the user’s final decision: profile-owned subagent defaults, with Native as the default.

## Editable sizing extension (2026-09-30)

The user approved replacing SIZE with CPU and MEM (GiB), using separate underlined TextField controls in the selected container row. Both fields must remain visible while focused, accept Tab and clicks, and register cursor geometry from the same layout that draws the table. CPU is a positive integer; MEM is positive decimal GiB with checked conversion to bytes. Invalid or above-host-limit entries remain editable, show an error, and disable Next. Existing allocations must round-trip exactly, including non-integral GiB host limits. Resource edits invalidate remote creation preflight and move preparation. Keep defaults and remembered sizing and retain edits when navigating back.

The user explicitly chose an instance-type dropdown for EC2. CPU and MEM remain read-only for EC2, and each dropdown option identifies the instance type and its resources. Catalog queries stay in existing background tasks. Preserve available selected types during refresh, reject removed selections, and cache stale-target results without changing the active target. Bare targets show dashes with no sizing controls. Remove the old sizing shortcuts and instructions. Reuse TextField, ComboBox, Form and WizardDraft, without protocol or store changes. The broader dialog body redesign remains deferred in issue #1205.

Implementation milestones are shared sizing state and validation in mj-tui/src/wizards/resources.rs, controls and interaction in dashboard.rs/draft.rs, and target table drawing in picker.rs/render.rs. Extend TextField with an opt-in underline style so other editors retain their appearance. Add event-driven editing, submission, EC2 selection and constrained-terminal rendering tests. Repeat cargo fmt --all -- --check, elevated cargo test -- --quiet and cargo clippy --all-targets -- -D warnings, then commit explicit task files on hel3.

Revision note: extend the plan with the approved CPU/MEM and EC2 behavior, and record successful validation of the earlier spacing work.

Review discovery: mj-tui/src/ingest.rs silently clamped wizard allocations whenever a background capacity sample arrived. Replace that path with refresh_wizard_resource_limits, which calls the same field validator used by editing. Retain draft text, invalidate preflight or move preparation if the resolved allocation changes, and return an oversized draft to Target with its error. Defaults are still initialized through existing remembered/default sizing. Also preserve whitespace when truncating descriptive table cells. The focused wizard suite passed 134 tests before this final ingestion regression was added. The final all-TUI run passed 897 tests with 2 ignored; Clippy passed after unused imports from the removed clamp were cleaned up.

Revision note (2026-09-30): record completed sizing behavior and validation. The user has additionally requested skipping single-choice Profile/Target steps and skipping Confirm when both are single-choice. The currently validated dialog work is a coherent checkpoint before that extension. The latest profile-setup requirement below supersedes the earlier optional Configure/Options proposals.

## Profile setup and navigation extension (2026-09-30)

The latest user clarification supersedes the earlier Configure/Options proposals and the suggested harness-specific defaults. Each HarnessProfile will own a subagents setting, defaulting to Native. Settings → Agent Profiles → a profile → Sub-agents offers only Native and Mjolnir single model, with model and optional effort fields. Legacy AllModels and None session policies stay readable for running sessions and child-agent suppression; they are not profile setup choices. New sessions and Moves use the selected profile's configured policy instead of the global remembered session policy. Resume retains its recorded policy. Remove the subagent controls from Confirm. Configuration validation rejects empty models, empty efforts, unsupported harnesses, and removed profile policies.

A single enabled (or resume-compatible) profile skips Profile. The existing lone_target predicate owns the target decision: only one compatible, usable raw target can skip Target. Containers and EC2 always show Target for sizing. Confirm is skipped only when both Profile and Target were skipped. Create retains project directory selection, with its final action labeled Create. Open/Move start their existing supervised prerequisite work immediately; necessary transfer choices and failures remain reachable. Introduce a Launching wizard state for asynchronous preparation so the fast path never renders Confirm, retries explicitly after failures, and submits exactly once after readiness. Back and step numbers omit skipped pages.

Implementation runs through mj-core/src/config/harness.rs and subagent.rs for profile policy, mj-tui/src/setup/schema.rs and setup.rs for editing, controller.rs and daemon/create.rs for default resolution, and mj-tui/src/wizards for navigation and removal of session policy editing. Add behavioral tests for settings persistence, independent defaults, raw versus sized routes, skipped-page Back behavior, async completion, and retry. Run elevated dev-profile cargo test and cargo clippy --all-targets -- -D warnings plus formatting and diff checks, then commit explicit files on hel3.

Decision: use explicit profile policy, never infer it from harness kind or the last session. Rationale: the user wants to configure Claude and OpenAI differently during profile setup while Native remains the default.

Revision note: record the corrected profile configuration requirement and the raw-only target skip rule; the pending Configure/Options question no longer applies.

Milestone outcome (2026-09-30): profile policy is persisted and used by CLI, TUI, and web creation and TUI/web Moves. The review controls and AllModels/None UI choices are removed; recorded legacy session policies remain readable. Settings model and effort selectors use supervised draft discovery, show loading/errors, reject retired replies, and provide retry. The navigation state has an explicit Launching step, retains the sizing page for containers/EC2, and submits completed Move preparation once. Failed resume checks expose an explicit Retry. Necessary large-transfer file selection remains. Initial validation exposed PTY expectations for the now-skipped Profile screen and a settings test fixture missing the second model-specific catalog reply; both fixtures were corrected. The final full dev-profile Cargo suite passed, including 2065 controller, 579 core, 900 TUI, 698 worker, 253 CLI, and 11 PTY tests. Clippy with warnings denied, formatting, and diff checks passed. This completes the profile settings and navigation checkpoint on hel3 without pushing.

Revision note: record completed profile settings and navigation, validation-driven fixes for the PTY route, correct modal border measurement, asynchronous failure retries, and passing final validation.
