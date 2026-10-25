# Shared form navigation and viewport scrolling

This living plan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Users can traverse ordinary form controls with vertical arrows and scroll a clipped form body with the mouse wheel. History, multiline editors, lists and expanded popups retain their own vertical keys. Focus changes reveal a whole control block, including adjacent validation text.

## Progress

- [x] (2026-10-25) Inspect shared controls and all viewport call sites.
- [x] (2026-10-25) Implement shared navigation and persistent viewport ownership in Form.
- [x] (2026-10-25) Adopt all seven viewport sites and audit consumers; preserve two screen-owned histories through shared metadata.
- [x] (2026-10-25) Add four shared behavior regressions; formatting and both complete suites pass.
- [x] (2026-10-25) Final complete two-crate suites and all-target clippy pass after first-frame wheel/focus ordering refinement.

## Surprises & Discoveries

The new-session project directory and mount source have screen-owned history routing. They must declare vertical key ownership even though TextInput itself has no history configured. First-frame focus repair can occur after viewport layout; a later wheel event must supersede that pending reveal to avoid snapping back.

## Decision Log

- Decision: Form owns one viewport's geometry, offset and last revealed focus; callers supply a half-open content row range. Rationale: event routing and focus have one owner, avoiding caller wheel emulation and redundant offset state. Date: 2026-10-25.
- Decision: TextInput exposes uses_vertical_keys and Form exposes set_field_uses_vertical_keys. Rendering registers the editor's capability; custom history consumers explicitly supply theirs during declaration and rendering. Rationale: preserve existing field and popup behavior without changing ControlKind variants across every declaration. Date: 2026-10-25.

## Outcomes & Retrospective

Shared behavior and all consumers are implemented. Chat: 405 passed, 2 ignored; TUI: 539 passed, 2 ignored (including 69 setup tests). Golden bytes remain unchanged. Final all-target clippy passed with no warnings after the first-frame ordering refinement. No commit is authorized for this delegated task.

## Context and Orientation

`mj-chat/src/components/scope.rs` owns Form event routing and control metadata. `controls.rs` registers TextInput editors. `layout.rs` supplies FormViewport clipping. Existing callers in `mj-tui/src/review_settings.rs`, `dialogs/container.rs` and `wizards/{render,projects,move_files}.rs` supply row locations. Settings files are excluded.

## Plan of Work

Add field vertical-key metadata and route ordinary vertical arrows through focus_sibling after popup capture. Move persistent viewport offset into Form; let unclaimed wheel input scroll the registered body. Reveal focused row ranges only on focus changes, clamp after every operation and show the top of oversized blocks. Replace caller-owned offsets and constructors with Form::viewport. Audit review settings, new/resume/project/move wizards, generic and container dialogs, workspaces, elicitation, config_picker and turn_review.

## Concrete Steps

From the repository root run cargo fmt -p brokk-mj-chat -p brokk-mj-tui. Run elevated cargo test -p brokk-mj-chat --no-fail-fast and cargo test -p brokk-mj-tui --no-fail-fast, then cargo clippy -p brokk-mj-chat -p brokk-mj-tui --all-targets. Retain logs under the assigned report directory. Do not change build storage or run installation.

## Validation and Acceptance

Shared component tests must demonstrate arrow traversal, history and popup capture, wheel scrolling without focus movement, focus reveal after scrolling, inner list precedence, clamping, and fitting/oversized block visibility. Existing TUI golden bytes must stay unchanged. No test binary may touch the default daemon instance.

## Idempotence and Recovery

All edits are reversible source changes; no daemon or store changes are needed. Leave edits uncommitted for parent review.

## Artifacts and Notes

Evidence and audit notes belong in `.mj/agents/de7d2d09c030146d070217563018da03/`.

## Interfaces and Dependencies

Use existing crossterm events and ratatui Rect. Form::viewport returns the existing FormViewport clipping helper and accepts Option<Range<u16>>. Form::set_field_uses_vertical_keys overrides field arrow ownership; TextInput::uses_vertical_keys reports multiline/history capability.

2026-10-25 update: adopted every caller, completed consumer audit and regressions, and documented the first-frame wheel ordering case.
