# Edit subagent models and effort on the main settings page

This ExecPlan follows `.agents/PLANS.md` and records the approved plan through implementation and validation.

## Purpose / Big Picture

Settings > Sub-agents should contain Maximum concurrent, Straightforward model, its conditional effort, Subtle model, and its conditional effort. Model choices start with literal None; effort appears only after choosing that task's model. Existing saved choices remain intact. Saving a Subtle-only draft must fail visibly without closing the dialog or changing the saved configuration.

## Progress

- [x] (2026-10-30) Read the shared settings model and TUI flow; confirm existing None defaults and save validation.
- [x] (2026-10-30) Obtain approval for the inline-controls plan.
- [x] (2026-10-30) Implement shared visible field paths, labels, owning-page lookup, and settings API projection.
- [x] (2026-10-30) Finish TUI rendering, search/navigation, and existing golden updates; focused global golden, search, and mouse navigation tests passed.
- [x] (2026-10-30) Extend the real settings API golden with inline controls and Subtle-only rejection; focused endpoint test passed.
- [x] (2026-10-30) Finish the browser replay and rendered golden against the updated API exchange; focused Chromium update and verification both passed.
- [x] (2026-10-30) Review the integrated UI and persistence boundary; run focused affected cases. Independent source review found no actionable defects.
- [x] (2026-10-30) Run full touched-crate suites: client 43 passed, Controller 2205 passed/18 ignored, TUI 563 passed/2 ignored; doctests passed. Existing core four-combination matrix passed.
- [x] (2026-10-30) Finish workspace Clippy after its three traversal/helper cleanup findings; affected focused goldens/search and the failed check passed, along with formatting/diff checks.
- [ ] Commit on the current master branch and push to origin/master.

## Surprises & Discoveries

`SubagentConfig` already defaults both tasks to None and enforces Subtle requiring Straightforward. `mj_client::settings::save::save_at` validates before writing, while TUI `setup_saved` retains the draft on failure. These remain the validation authority; the event loop must not run full config validation because it may read profile files.

The first shared implementation flattened Subagents into the top-level Settings page by mistake. Integrated review corrected it to flatten only the Sub-agents page, retaining the Settings page's Sub-agents navigation button. Search page ownership must be applied only to scalar fields, so ordinary object/collection pages still open normally.

The browser settings golden replays the Controller exchange strictly in order. Changing that exchange also requires updating `tests/e2e/web/settings.spec.js` and its existing rendered golden, which now exercise the rejected save before the successful combined save. Actual nested storage parents still supply labels and summaries; display-page ownership is only for navigation.

Final Clippy found that the traversals no longer mutate their input paths and that `SetupDialog::keys` became test-only when production adopted full row paths. Replace those walk parameters with immutable slices and compile the old keys helper/import only for tests; no lint suppression or behavior change is needed. Full suite output was successful before this cleanup, so only affected focused tests and Clippy need another run.

## Decision Log

Use one shared `visible_paths(path, value)` projection in `mj-client/src/settings/page.rs` and one `row_page(path)` lookup. The projection keeps actual nested storage paths while displaying task fields together. TUI descriptors/search and the rendered API page consume it, preventing different interpretations of field visibility. Date: 2026-10-30.

Keep configuration shape and save APIs unchanged. A model change clears that task's effort through the existing edit operation. None removes that task choice when projecting the draft into Config. Temporary Subtle-only drafts remain editable; the existing background save rejects them. Date: 2026-10-30.

Defaults apply to unconfigured choices; saved models and efforts are preserved. Unset effort uses the harness default. Discovery remains background work, cannot pick a model implicitly, and displays loading separately from the selected None value. Session delegation overrides are outside this change. Date: 2026-10-30.

Shared client files and TUI files are owned by separate children. The parent retains the bounded existing settings API acceptance case because it verifies both components together, plus integrated review, validation coordination, and Git delivery. Date: 2026-10-30.

## Outcomes & Retrospective

The real settings endpoint accepts inline edits and rejects Subtle-only saves without a file write. Its focused golden test passed with one test and no failures; the captured exchange shows None-first models and conditional effort rows on Sub-agents. The TUI golden captures the retained draft and save error on the main page; focused search and single-click dropdown tests passed. The browser strict replay and rendered golden passed both update and verification runs. Independent source review found no actionable navigation or discovery defects. All 2811 tests in the touched crates passed, with 20 ignored; doctests and the focused existing core matrix passed. Clippy, formatting, and diff checks passed after traversal/helper cleanup. Git delivery remains.

## Context and Orientation

`mj-client/src/settings/page.rs` describes visible rows. `schema.rs` supplies labels/defaults/options and `view.rs` renders settings pages for API consumers. The new helpers preserve paths such as subagents/straightforward/model even though their owning display page is subagents. `mj-tui/src/setup` draws those controls and routes keyboard, mouse, search, discovery, and background save results. The existing `golden_global_subagent_discovery_and_save` case and `mj-tui/tests/golden/global-subagents.txt` pin the whole terminal output. `mj-controller/src/server/tests.rs::settings_routes_hide_literal_secrets_and_save_through_the_shared_model` and its viewer-settings-exchange golden exercise real HTTP edits and saves.

## Plan of Work

First expose shared full row paths and page ownership without flattening the stored JSON. Render inline task labels and literal None, retain the root Sub-agents page, and put discovery feedback in page help. Then update TUI descriptors, search indexing/jumps, and navigation to use those helpers. Preserve stable control identities and existing open-picker selection reconciliation.

Extend the existing terminal golden for default None controls, chosen models, conditional efforts, changing/clearing models, asynchronous discovery, and rejected save feedback. Update the existing API exchange to send task edits from the main Sub-agents page and reject Subtle-only before a valid combined save. Update the browser's strict replay of that exchange and its rendered golden to follow the inline controls. Review the complete UI/API behavior and only then run final suites and Clippy.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir/.mj/clones/55224b4db70fd4f5a2ded170be482077`. Run focused TUI cases with `cargo test -p brokk-mj-tui <filter>` and the settings API case with `cargo test -p brokk-mj-controller settings_routes_hide_literal_secrets_and_save_through_the_shared_model`. Every Cargo test runs elevated outside the restricted sandbox, in the default dev profile, using the existing mbx output configuration. Regenerate only affected golden files through the repository's golden update mechanism.

After the last edit, run full suites for brokk-mj-client, brokk-mj-tui, and brokk-mj-controller, then `cargo clippy --all-targets -- -D warnings`, relevant rustfmt checks, and `git diff --check`. If a test fails, correct it and rerun only that test. Stage explicit owned files, commit on the current branch, and push HEAD to origin/master. A rejected push is resolved by fetching/merging origin/master rather than rebasing.

## Milestones

The shared-field milestone exposes full nested setting paths while presenting models and efforts together on Sub-agents. Its acceptance is the updated real HTTP exchange in `mj-controller/tests/golden/viewer-settings-exchange.txt`, generated by `MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-controller settings_routes_hide_literal_secrets_and_save_through_the_shared_model`. The test passed and records a 400 response for Subtle-only without changing the file, followed by a successful combined save.

The interface milestone makes keyboard, mouse, search, and browser editing reach those same fields. Run `cargo test -p brokk-mj-tui golden_global_subagent_discovery_and_save`, `cargo test -p brokk-mj-tui global_subagent_search_targets_inline_controls_and_hides_unavailable_effort`, and `cargo test -p brokk-mj-tui one_click_opens_a_page_and_a_real_dropdown`. Each focused test passed. From `tests/e2e/web`, run `npm ci` if dependencies are absent and then `./node_modules/.bin/playwright test settings.spec.js --project deterministic`; this also passed against the captured Controller exchange, with no sideways overflow at a 390-pixel viewport.

The delivery milestone runs the full touched-crate suites and workspace Clippy in the dev profile, records results below, commits explicit task files on the current branch, and pushes to origin/master. No worker inputs or harness pins changed, so the path-filtered agent-dev image publication workflow is outside this change.

## Validation and Acceptance

Opening global Sub-agents with an unconfigured draft shows both models as None and no effort fields. Each model dropdown has None first. Choosing a model reveals only its effort row; changing/clearing that model clears its effort and clearing hides the row. Reopening a saved draft retains its choices. Search and single-click/keyboard navigation reach the same controls without task subpages, and ordinary Settings page navigation still works.

Discovery arriving while a picker is open preserves selection and never makes an untouched draft dirty. Save accepts both unset, Straightforward alone, and both set. Save rejects Subtle alone and also rejects clearing Straightforward while Subtle remains configured; the dialog and draft remain available and the file remains unchanged. Existing core matrix tests are reused rather than duplicated as small feature examples.

## Idempotence and Recovery

No migration or public payload type changes are needed. Tests retain existing private config/data directories. Any manual binary/TUI check must use a separate named instance, never the default instance. Do not redirect Cargo output or change mbx configuration. Preserve unrelated working-tree changes and report background errors through existing mechanisms.

## Artifacts and Notes

Initial checkout was clean at f21531f61. No matching issue was identified in the issue search. Detailed child reports/logs live in `.mj/agents/`; only concise results belong in this plan. An initial discovery child failed during Grok startup because that harness rejected additionalDirectories; the failed child was stopped and the read-only map completed through another configured child without changing user settings.

## Interfaces and Dependencies

The new shared helpers are `visible_paths(path: &[String], value: &serde_json::Value) -> Vec<Vec<String>>` and `row_page(path: &[String]) -> &[String]`. Other pages preserve their existing visible keys and object/collection semantics. Reuse the existing schema labels/options, edit operation, asynchronous capability hydration, `save_at`, and TUI save result handling. Add no dependencies, crates, persisted synthetic fields, or independent copies of the Subtle/Straightforward validation rule.

Revision note (2026-10-30): Initial implementation record for the approved inline Subagents plan, including shared ownership, integrated acceptance cases, and required final checks.
