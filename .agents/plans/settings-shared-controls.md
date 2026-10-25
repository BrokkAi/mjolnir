# Convert Settings pages to shared controls

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Settings will use the same buttons, checkboxes, dropdowns and inline fields as other forms. A click operates the control directly. Text remains in the draft while browsing pages; Enter applies it and Save validates every pending field. Invalid text remains visible with an error and prevents saving. Search finds a control without changing its value.

## Progress

- [x] (2026-10-25, archive-notice finding) Keep inline-field Enter/Save validation only under its field, preserving unrelated page notices. Add correction/estimate/Enter/Save/background-notice coverage and refresh the build-cache snapshot to remove duplicated bottom errors.
- [x] (2026-10-25, archive-notice finding) Full TUI: 553 passed, two ignored. Both-crate clippy with warnings denied, formatting and diff checks pass; leave changes uncommitted.

- [x] (2026-10-25, live search finding) Confirm that full-row search hitboxes retained default two-click activation. Set Results to shared SingleClick and synchronize the clicked selection before navigation; all 82 Settings tests pass.
- [x] (2026-10-25, live search finding) Full TUI: 552 passed, two ignored. Both-crate clippy with warnings denied, formatting and diff checks pass; leave changes uncommitted.

- [x] (2026-10-25, live finding) Derive inline-editor focus from the current descriptor kind, including path completion ownership. Add the real environment collection Add/edit/Back/Add regression with search and Backspace checks; all 81 Settings tests pass.
- [x] (2026-10-25, live finding) Full TUI: 551 passed, two ignored. Clippy with warnings denied, formatting and diff checks pass; leave changes uncommitted.

- [x] (2026-10-25, review round 2) Refresh or invalidate install previews by the operation machine's current draft key, independently of navigation. Add a regression covering success, refresh failure and newer draft keys; all 80 Settings tests pass.
- [x] (2026-10-25, review round 2) Full TUI: 550 passed, two ignored. Both-crate all-targets clippy with warnings denied, formatting and diff checks pass. Changes remain uncommitted.

- [x] (2026-10-25) Read the investigation and split modules; inspect shared navigation and viewport support.
- [x] (2026-10-25) Replace page-list state with descriptors, persistent text buffers and path-based edits.
- [x] (2026-10-25) Render and route shared controls; retain async guards and detail-page actions.
- [x] (2026-10-25) Rewrite input helpers and affected tests; update the five owned Settings goldens.
- [x] (2026-10-25) Run formatting, both crate suites and clippy; prepare the uncommitted result.
- [x] Parent updated the three embedded Settings snapshots: review-settings, command-palette and support-pane-title-menu.
- [x] Independent review reproduced viewport/cursor geometry, nullable-default, mbx ownership and nested-review focus defects.
- [x] (2026-10-25) Fix the four confirmed defects; add five adapted regressions and strengthen the reveal test. Full TUI: 549 passed, two ignored. Chat: 405 passed and one timeout, with that test passing alone; two ignored. Clippy and formatting passed.

## Surprises & Discoveries

The live search report initially looked like a label/value hitbox mismatch. The ChoiceList registered the whole row and retained DoubleClick activation; the subsequent value click was the second click. A regression first fails on a fresh label click, then passes after Results explicitly opts into SingleClick. On activation, Settings reads the control's selected metadata, as the command palette does, so clicking an unselected result opens that row. The test uses independent fresh dialogs at five horizontal positions for boolean, dropdown, page and non-first results, asserting no draft mutation or dropdown expansion.

Live testing found that a scalar collection entry has the same display path when shown as a parent Page button and as a detail-page Field. The pending buffer correctly survives Back, so its existence cannot identify a focused editor. `focused_field_path` now asks the current descriptor kind and is shared by text shortcut gating, path popup gating, completion context, focused completion input, and completion dismissal. Explicit path-based Save/apply operations still use pending buffers independently of focus.

Round 2 found that install completion still selected the visible machine page before replacing a cached preview. With per-machine retention that left the pre-install answer cached indefinitely after navigation. The callback now guards by the operation machine's current draft key, retains its fresh answer, and removes its cache entry on refresh failure. The other result callbacks do not consult the visible page to select a machine's cache. The regression also proves older operation keys cannot overwrite or invalidate a newer resolution.

Independent review found four real defects despite passing initial tests. The old reveal regression sent wheel events without asserting movement. Its strengthened version scrolls the field out of view before triggering Enter validation. A separate root-page wheel regression uses the reported 100x20 surface, and a pointer test inserts text at an interior character. The original nullable-permissions probe used a local-bare fixture where the Permissions setting is intentionally hidden; the adopted regression uses SSH bare and exercises the actual dropdown and configuration reader.

The full TUI suite also embeds Settings in `review-settings.txt`, `command-palette.txt` and `support-pane-title-menu.txt`. Those golden files are outside this assignment's explicit ownership; their mismatches are limited to the new Settings controls, summaries, hints and contextual Remove placement. The main test log contains their exact diffs.

Scalar environment-map entries need their own detail page to give Remove a page-path target. Array removal must shift pending field paths for surviving entries, rather than dropping their unsaved text. Save can await target-machine path resolution, so a small Settings state machine resumes validation through the existing prerequisite poll and rejects stale generation/draft/text replies.

The old text editor already owns normalization and validation. Its apply logic must become a path-based operation rather than duplicating those rules for inline controls. Archive preview and path-resolution guards explicitly refer to that editor and must instead refer to persistent buffers.

## Decision Log

- Decision: inline text/path field validation does not write a bottom notice on Enter or pending-field Save failure. Rationale: the field owns its error and already clears it on a changed edit or successful apply; duplicating it requires a second lifetime and risks overwriting unrelated background notices. General save failures, background results and host-ownership notices retain their existing notice ownership. Date: 2026-10-25.

- Decision: retain cache resolutions per machine; resolve field ownership from its machine and request key, independently of the current page. Drop pending text for fields becoming host-owned and show an explanatory notice. Rationale: a now-read-only control cannot be used to repair a rejected Save, and the host owns those values. Date: 2026-10-25.
- Decision: metadata preparation uses `begin_update`; only rendering uses `begin_frame`. Rationale: input must retain the latest cursor maps and viewport geometry. Date: 2026-10-25.
- Decision: derive dropdown nullability from schema defaults, including after concrete selection, and give null an explicit Default label. Restore review focus by its display path after Back. Date: 2026-10-25.

- Decision: use the existing Settings prerequisite polling method to resume a Save after path resolution; no CLI callback contract changes. Rationale: keep network work outside the UI loop and retain the existing action/result transport. Date: 2026-10-25.
- Decision: retain empty strings for required text and environment values; only existing schema-aware nullable/defaulted fields clear to defaults. Rationale: an environment variable explicitly set to an empty value has distinct meaning. Date: 2026-10-25.
- Decision: stop at the three out-of-scope golden updates after finishing all authorized implementation and validation. Rationale: explicit ownership requires parent approval before touching those files. Date: 2026-10-25.

- Decision: retain text buffers by display path across page navigation; allocate monotonically increasing screen-local IDs and reconcile descriptors by path and kind. Rationale: control identity must not be a row ordinal, and fields are part of the draft. Date: 2026-10-25.
- Decision: keep the small Add-name prompt; remove per-field discard prompts. Rationale: collection creation is still an explicit operation while existing values edit inline. Date: 2026-10-25.
- Decision: do not commit. Rationale: the delegating agent explicitly owns review and commit. Date: 2026-10-25.

## Outcomes & Retrospective

The conversion and four independent-review fixes are implemented, uncommitted. The parent applied the three additional embedded Settings snapshots. Final TUI validation after inline-only field feedback passes: 553 tests, two ignored (+9 regressions versus the reviewed 544). The chat suite had 405 passing tests and one unchanged image-transport test timing out; that single test passed on its isolated rerun, validating all 406 tests, with two ignored. Formatting, diff checks and clippy with warnings denied pass. No shared component behavior changed during review fixes. The newly explicit Default labels updated settings-root and global-subagents snapshots; all snapshot producers pass in the full TUI run. Evidence is recorded in `.mj/agents/8fff757b7257da74ca1c67a405420b21/review-fixes-report.md`.

## Context and Orientation

`mj-tui/src/setup.rs` owns Settings state. Modules under `mj-tui/src/setup/` separate draft transformations, declaration, rendering, input, async results, paths and previews. `schema.rs` defines config interpretation and remains unchanged. `search.rs` indexes display paths. `mj-chat/src/components/scope.rs` owns shared Form focus and viewport scrolling. A descriptor maps a Copy control ID to a display path and a control kind. A pending buffer is text entered in a field but not yet applied to JSON. Display paths pass through the existing storage-path helper before accessing JSON, since Interface is synthetic.

## Plan of Work

First add explicit focused-block reveal to Form with a component regression. Replace Settings list selection with descriptors that drive declaration, layout and event lookup. Preserve schema choice and path interpretation. Move scalar validation into a path-based edit function; Save visits pending buffers in deterministic order and focuses the first invalid one. Move Remove and Sign in to detail pages. Render each descriptor using shared controls and a viewport block including errors, warnings and preview lines. Keep search as ChoiceList. Reconcile live choices by value and leave async generation/request-key checks intact. Finally rewrite test helpers to use real key and pointer events and update intentional render changes.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir`, edit owned Settings modules and the small shared reveal addition. Run `cargo fmt -p brokk-mj-chat -p brokk-mj-tui`. Run focused tests during fixes, then `cargo test -p brokk-mj-chat --no-fail-fast` and `cargo test -p brokk-mj-tui --no-fail-fast` outside the sandbox. Run `cargo clippy -p brokk-mj-chat -p brokk-mj-tui --all-targets` with no warnings. Keep logs under `.mj/agents/8fff757b7257da74ca1c67a405420b21/`. Do not change Cargo build paths or invoke the CLI, daemon or install script.

## Validation and Acceptance

Tests must demonstrate one-click checkbox, dropdown and page actions; search landing without mutation; text surviving Back and dirty checks; invalid Save focusing the field; Enter revealing errors; detail-page actions; nullable text defaults and the three build-cache enabled states. Existing discovery races, preview guards and validation assertions remain. Whole crate suites and clippy must pass. Golden updates must explain the resulting real controls.

## Idempotence and Recovery

Edits affect only UI state, not persisted config contracts. Build and test commands use existing build storage. No live instance is started. Re-run only failing tests while iterating. Preserve unrelated working-tree edits and leave final changes uncommitted.

## Artifacts and Notes

The initial design evidence is `.mj/agents/99fc020cf9b62aa701cfdc5f10a6a9ab/design.md`. Its source line numbers precede the module split; module names above are authoritative.

## Interfaces and Dependencies

Use existing `Dialog<SetupControl>`, `Button`, `Checkbox`, `ComboBox`, `TextField`, `PathField` and `Form::viewport`. Add `Form::reveal_focused()` solely to request visibility after validation changes an already-focused block. Do not introduce config or wire changes or new crates.

Revision note (2026-10-25): implementation and validation completed within the ownership boundary; record the three embedded golden updates requiring parent approval.

Revision note (2026-10-25, review fixes): preserve input geometry, add schema-nullable defaults, own cache previews per machine and discard newly host-owned pending text, and restore review launch focus. Final validation completed; full TUI passes and the single unrelated chat timeout passes on an isolated rerun.

Revision note (2026-10-25, review round 2): install completion now refreshes its own machine off-page and invalidates the old cache on refresh failure. All 80 Settings tests and the full TUI suite (550 passed, two ignored) pass. Both-crate clippy with warnings denied, formatting and diff checks pass.

Revision note (2026-10-25, live shortcut finding): identify focused editors from their current descriptor, not cached buffers. All 81 Settings tests and the full TUI suite (551 passed, two ignored) pass. Clippy with warnings denied, formatting and diff checks pass.

Revision note (2026-10-25, live search finding): use shared SingleClick activation without changing geometry. All 82 Settings tests and the full TUI suite (552 passed, two ignored) pass. Both-crate clippy with warnings denied, formatting and diff checks pass.

Revision note (2026-10-25, archive notice): field validation now has a single inline owner, and the build-cache snapshot preserves background resolution status instead of replacing it with duplicate validation text. Full TUI validation passes (553 passed, two ignored), as do both-crate clippy with warnings denied, formatting and diff checks.
