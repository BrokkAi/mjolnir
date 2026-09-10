# Rebuild terminal dialogs around predictable shared behavior

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture

Terminal users should be able to predict how every menu, editor, and wizard responds. Selection lists select on one click and activate exactly like Enter on double-click. Command menus run on one click. Shared dialog components provide consistent focus, actions, layout, dismissal, and draft protection. Existing workflow steps and controller operations remain intact; browser dialogs are outside this change.

## Progress

- [x] (2026-09-10) Inspect existing forms, setup, workspace management, and wizard routing; agree on terminal scope, unchanged workflow steps, single-click commands, and protected drafts.
- [x] (2026-09-10) Implement shared list activation and gesture isolation with deterministic behavior tests.
- [x] (2026-09-10) Implement shared dialog state, layout, action declarations, and dismissal behavior; migrate terminal dialogs.
- [x] (2026-09-10) Replace wizard legacy focus and synthetic Enter routing with semantic actions.
- [x] (2026-09-10) Validate unit, rendering, and terminal acceptance behavior, run required checks, and commit on the current branch; document the unrelated controller fixture failure.

## Surprises & Discoveries

The repository already has persistent `Form` controls in `mj-chat/src/components/scope.rs`, but list mouse release only emits Select while Enter emits Activate. The wizards maintain duplicate legacy focus enums and synthesize Enter to invoke domain operations. Setup already preserves its draft inside a dismissal confirmation, providing a basis for generalized protection. Geometry is reset on every render, so gesture invalidation must distinguish ordinary redraws from actual changes in geometry or content.

## Decision Log

Decision: Extend the existing component library without a new crate. Rationale: focus, hitboxes, controls, and scrolling already have shared ownership. Date: 2026-09-10.

Decision: Preserve workflow steps and terminal theme; standardize interaction and layout. Command results run on one click, while workspace/setup/wizard lists select first. Rationale: user choices during planning. Date: 2026-09-10.

Decision: Preserve the existing typed `Interaction::Activate(K)` API and update Form selection atomically before emitting it. Command consumers read the selected index from Form. Rationale: Enter and double-click then use precisely the existing action route without a breaking interaction variant across unrelated chat controls. Stable domain identities and content revisions guard gestures. Date: 2026-09-10.

Decision: Escape dismisses the innermost layer; changed editable values require Keep editing or Discard, with Keep editing focused. Navigation and loading do not dirty drafts. Rationale: user selected draft protection. Date: 2026-09-10.

## Outcomes & Retrospective

The shared dialog component and terminal migrations are implemented. Focused validation passed 512 chat/component tests and 423 terminal tests before final workspace declaration and end-to-end additions. Final terminal validation passes 428 tests (2 ignored); shared chat/component validation passes 514 tests (1 ignored). Clippy is clean. Live acceptance identified and fixed missing target-editor parent restoration and stale harness assumptions. Full-suite reruns encountered intermittent executable-busy errors in unchanged controller subprocess fixtures; an isolated upgrade-test rerun passed, the reduced-concurrency full run passed 766 controller tests but hit the same npm-upgrade fixture error. All remaining workspace packages passed separately, including PTY shutdown checks.

## Context and Orientation

`mj-chat/src/components/` owns reusable terminal controls, event results, focus, and layout. `mj-chat/src/hel_modal.rs` provides modal geometry. `mj-tui/src/component_events.rs` routes active modal events; `dialogs.rs`, `setup.rs`, `workspaces.rs`, `palette.rs`, `resume.rs`, and `review_settings.rs` own domain drafts and render individual dialogs. `mj-tui/src/wizards/dashboard.rs` contains creation and resume/move transitions. These screens emit `DashboardAction` requests; controller background tasks execute filesystem, network, and process operations. Keep that separation intact.

## Plan of Work

First extend Form with explicit list activation policy and atomic selected-index updates before activation. Reuse the existing 500 ms double-click interval, require matching completed clicks, and invalidate gestures on changed targets, navigation, scrolling, resizing, or layer transitions. Route keyboard and mouse through the same domain action. Test with an injected event timestamp rather than sleeps.

Next add reusable dialog state and shell to the components module, composing Form and FormViewport. Declare initial focus, submit mappings, action roles, and dismissal behavior. Maintain visible focus separately from the default action. Modal forms absorb outside clicks; command menus dismiss. Keep actions visible on small terminals and errors near content. Migrate palettes/workspaces, setup and nested editors, wizards, and remaining dashboard dialogs. Use existing typed controller actions and preserve busy-state guards.

Replace legacy wizard focus and synthetic key dispatch with semantic transition functions. Back preserves entered values. Escape closes child popups/editors before their parents; close and Cancel use the same protection path. Generalize the existing draft-preserving confirmation, including restoration of prior focus and values. Do not count search or highlighted rows as unsaved edits. Destructive confirmation initially selects the safe action.

## Concrete Steps

Work from `/home/jonathan/Projects/hel3`. Inspect callers before changing exported interaction variants. Add colocated behavior tests and update live probes in `tests/e2e/tui_components_dialogs.py` and the existing terminal harness. Run `cargo fmt --check`, `cargo test` with elevated permissions, and `cargo clippy --all-targets -- -D warnings`. Discover the existing harness invocation and run relevant terminal scenarios. Commit only changed files on the current branch after validated milestones; do not push.

## Validation and Acceptance

Prove single clicks select ordinary lists, double-clicks produce the same action as Enter, command clicks activate the clicked row, disabled rows do nothing, drags cancel, click intervals and identities are respected, and a second click cannot operate a newly opened dialog. Verify nested Escape, draft keep/discard, focus restoration, preserved wizard values, disabled default actions, duplicate submission prevention, and normal/small terminal rendering. Full tests and clippy must pass; record exact results below.

## Idempotence and Recovery

Changes are local source refactors with no data migration. Retain existing controller action types, asynchronous cancellation, and stale-result protection. Retry failed validation after fixing its source. Do not clear build storage or change mounts to work around failures. Stage only task-owned files.

## Artifacts and Notes

Initial component tests: 45 passed. Expanded chat/component tests: 512 passed, 1 ignored. Terminal regression tests after updating deliberate behavior changes: 423 passed, 2 ignored. Final results supersede these intermediate counts. Logs are under `target/dialog-tests.log`, `target/dialog-full-tests.log`, and `target/dialog-clippy.log`.

## Interfaces and Dependencies

Keep crossterm, ratatui, rat-focus, and existing Form controls. Add `ListActivation`, `Form::handle_at`, content revisions, and explicit domain item identities. Selection changes atomically before the existing typed activation is emitted. `Dialog<K>` composes Form with action roles, field-submit mappings, draft baselines, scoped dismissal, confirmation rendering, and pending submission state. `DialogShell` provides fixed-footer layout and interaction hints. Dialog presentation/state composes Form and existing modal geometry, returning typed events without I/O. Screens supply draft comparisons and domain actions. No controller protocol changes are required.

Revision: 2026-09-10 initial implementation plan recorded from the approved conversational plan.

Revision: 2026-09-10 implementation milestones completed; recorded compatible activation API, successful focused checks, and remaining full/live validation.

Revision: 2026-09-10 final edge fixes preserve wizard resources on revisits and restore the target ID editor parent; live probes now use current dropdowns, dismissal controls, and a focused dialog mode. The Python ACP fixture supplies bounded Node/npm preflight stubs and rejects package execution.

Final live acceptance: `python3 tests/e2e/tui_components_tmux.py --dialogs-only --seed 7109 --hel target/debug/mj` passed, recording 59 checks in `target/reliability-artifacts/tui-components-seed-7109-108783/live-evidence.json`. This mode excludes unrelated chat/sidebar and provider-discovery probes; the general harness still exposes those probes. It covers creation and resumption, setup selection/double-click and parent restoration, nested target editor focus, review keep/discard and disabled Save, palette sizing, persistent settings, container interaction/resizing/drag cancellation, Help, and safe deletion. `cargo fmt --check`, Python compilation, `git diff --check`, and final Clippy passed.

Final package validation: `cargo test -p brokk-mj-core -p brokk-mj-client -p brokk-mj-worker -p brokk-mjolnir` passed (including 934 core tests and all 6 PTY tests). Together with 514 chat tests, 428 terminal tests, 766 passing controller tests, and the isolated passing npm-upgrade test, all test cases were observed passing; a single final all-in-one run is not claimed because of intermittent executable-busy failures.
