# Choose projects without learning bundles

This ExecPlan is maintained in accordance with `.agents/PLANS.md`. It is a living document; its progress, discoveries, decisions, and outcomes must stay current.

## Purpose / Big Picture

Starting work in a container currently asks the person to choose or create a “bundle”, then presents an empty repository list and a text field requiring a known GitHub URL or filesystem path. Replace this with “Choose a project”: recognizable saved/recent projects, browsing repositories from the existing GitHub login, and browsing folders. Selecting one repository should continue to the session review without a separate bundle-naming or saving ceremony. Multiple repositories remain available as an optional extension. Existing configuration and automation continue to use bundles internally.

## Progress

- [x] (2026-09-24) Inspected terminal and web creation flows, shared path completion, bundle persistence, and existing tests. Claimed related onboarding issue #1128; this task addresses project selection, not that issue's entire onboarding checklist.
- [x] (2026-09-24) Selected the project-picker design and divided implementation by file ownership.
- [x] (2026-09-24) Implemented bounded repository discovery, folder-name filtering, and authenticated discovery with cancellation and upgrade admission. Eleven focused discovery tests passed.
- [x] (2026-09-24) Implemented terminal project selection, supervised discovery, one-choice continuation, optional multi-repository selection, main-repository selection, and 21 behavior/rendering regressions.
- [x] (2026-09-24) Implemented matching web project discovery and immediate single-repository continuation. The web unit and deterministic browser suites passed; final layout refinements passed the focused browser checks.
- [x] (2026-09-24) Updated human documentation and verified the actual authenticated discovery endpoint against local folders and GitHub in the isolated `project-picker-test` instance, then stopped that instance.
- [x] (2026-09-24) Validated behavior and narrow layouts, exercised the workspace Rust suites, fixed outdated terminal fixture labels, and passed the final TUI/PTY reruns, Clippy, and doctests. Reviewed documentation and prepared the implementation commit on the current branch.

## Surprises & Discoveries

The terminal already supports multiple repositories atomically, while the browser's bundle endpoint takes one source. Existing path completion is asynchronous, but hides discovery behind typing and a completion shortcut. A project picker must therefore offer visible browsing rather than merely rewording the input field. The TUI deliberately has no controller dependency, so shared request/response types live in `mj-core/src/project_picker.rs` and discovery execution remains in the controller. The legacy web helper could reuse a larger group based only on the primary repository; the new picker uses exact source membership.

Small-window mouse tests exposed horizontal button scrolling between mouse-down and mouse-up, and a zero-height selection list at 60×20. Compact tab labels and explicit allocation of the repository list fixed both. On mobile, source cards become compact buttons after a source is chosen, making room for the first folder result within a 390×844 viewport.

## Decision Log

Decision: Translate the internal bundle model at the UI boundary, retaining existing configuration and API identifiers. Rationale: the person wants to choose code, and compatibility does not require exposing persistence terminology. Date: 2026-09-24.

Decision: Use the existing GitHub CLI login for repository discovery, and browse only one filesystem directory at a time. Rationale: no extra authentication setup, unbounded recursive scan, or UI-loop I/O is needed. Directory browsing refers to the controller computer, and remote bare-directory selection must retain its host distinction. Date: 2026-09-24.

Decision: Discovery is cancellable, bounded background work with identity checks on every reply. Rationale: typing, switching source tabs, leaving the wizard, or restarting it must never allow a late response to overwrite the current selection. Date: 2026-09-24.

## Outcomes & Retrospective

The terminal and web now present project discovery instead of bundle creation. A chosen repository proceeds directly to review and is saved for reuse. The terminal also supports optional multi-repository selection and changing the main repository. Existing configuration and legacy API behavior remain compatible; no database changes are needed.

Validation covered the full workspace with `cargo test -q`. The final failing integration fixture expected the old step names; after correcting its terminal-diff markers, all ten PTY tests passed. After reducing the picker state's inline size, the complete TUI suite passed again (775 tests, two existing ignored tests), as did the ten PTY tests, `cargo clippy --all-targets -- -D warnings`, and workspace doctests. The web's 51 unit tests and 103 deterministic browser tests passed, followed by 37 focused passing tests for the final refinements; three lab-dependent cases were skipped. The two Python lab fixtures changed only their expected labels and were syntax-checked.

Mouse and keyboard regressions cover 80×24 and 60×20 terminal layouts, cancellation and stale replies, retries, exact single-repository selection, and multi-repository selection. Mobile screenshots were inspected at 390×844. An authenticated live HTTP smoke check successfully browsed local folders and searched the real GitHub repository in the isolated `project-picker-test` instance; that instance was stopped afterward.

## Context and Orientation

`mj-tui/src/wizards.rs` owns terminal new-session draft state. `mj-tui/src/wizards/dashboard.rs` interprets controls, and `mj-tui/src/wizards/render.rs` renders them. `mj-cli/src/dashboard/actions.rs` dispatches background work and `mj-cli/src/dashboard/io.rs` applies replies. The controller is the process managing configuration and sessions. `mj-controller/src/controller.rs` persists repository groups through `create_bundle_from_sources`; this stays the single persistence mechanism. The web wizard lives in `mj-controller/src/web/viewer.js`. Its authenticated HTTP routes are in `mj-controller/src/server/`; their supervised background tasks are in `mj-controller/src/server_runtime/`.

## Plan of Work

First implement `mj-controller/src/project_picker.rs`, exporting request, entry, and result types and a discovery function using the existing subprocess executor. A GitHub request lists accessible repositories or searches; a directory request returns immediate child folders, a selectable current repository, and a parent location. HTTP `POST /api/projects/discover` runs this through the existing capped preflight task set, participates in daemon upgrade admission, and cancels when the requester leaves.

Next replace the terminal's new-bundle editor with a source picker offering recent projects, GitHub, folders, and an optional URL input. Saved projects remain recognizable by repository names. Choosing a repository submits one source immediately unless the person explicitly chooses several repositories. The CLI runs discovery through its existing cancellable task tracking and returns results only to the requesting draft.

In parallel update the web wizard with the same source choices. Keep focus through snapshot updates, abort replaced searches, and validate request identity on both discovery and creation replies. A selected repository uses the existing bundle endpoint and advances directly. Folder controls must be usable without typing a path. Preserve target-host correctness for bare sessions.

Finally update human documentation for the new workflow, run behavior and rendering tests, inspect narrow layouts, and run the required dev-profile workspace checks before committing only task files to the current branch.

## Concrete Steps

Work from the repository root. Use `rg` to locate existing helpers before adding new ones. Run focused Rust module tests during implementation, then `cargo test` and `cargo clippy --all-targets -- -D warnings`. The execution environment is already unrestricted, so no sandbox override is supplied. Run web unit tests through the existing Node test harness and relevant browser tests where available. Any interactive new binary uses `--instance project-picker-test` and isolated configuration/data directories.

## Validation and Acceptance

A person with no saved bundles sees project choices instead of an empty bundle selector. A GitHub repository can be chosen without knowing its URL. A local repository can be reached by opening folders and using Up without typing its path. Current/recent projects remain immediately available. Selecting one project proceeds to review, and an explicit optional multi-repository action preserves terminal multi-repository behavior. Loading and failures are visible, retry preserves the selection, and rapid switching/back/cancel never applies stale results. Existing bundle config and CLI/API callers work unchanged. Tests must drive actual controls and asynchronous result transitions; do not rely solely on source-text assertions.

## Idempotence and Recovery

Discovery is read-only and safe to repeat. Bundle creation uses existing idempotent source matching and atomic configuration persistence. No schema migration is required. Cancel discovery when its screen is left; an already admitted persistent creation must be allowed to finish and report errors without applying its result to another wizard. Test resources are isolated and cleaned up by their owning process lifecycle.

## Artifacts and Notes

Parent owns terminal/CLI, documentation, integration, and this plan. Separate child sessions own discovery backend and web assets/tests. Browser screenshots are generated under the ignored `tests/e2e/web/test-results/project-picker/` directory. The inspected [mobile folder picker](../docs/project-picker-mobile.png) is retained for PR review. The browser creates single-repository projects and can reuse existing multi-repository projects; creating a new multi-repository group remains available in the terminal. This task does not implement the rest of issue #1128's account and machine onboarding.

## Interfaces and Dependencies

Export `mj_core::project_picker::{ProjectDiscoveryRequest, ProjectDiscovery, ProjectEntry, ProjectEntryKind}`, re-exported by the controller for its callers. Requests serialize with a snake-case `kind` tag: `Github { query: String }` or `Directory { path: String, filter: String }` (filter defaults to empty in JSON). Results contain `entries`, optional `directory` and `parent`, and `truncated`. An entry contains `name`, `source`, `description`, and a kind of `Repository` or `Directory`. `discover(&ProjectDiscoveryRequest, &impl CommandExecutor)` returns `anyhow::Result<ProjectDiscovery>`. Callers apply bounded deadlines and cancellation using existing subprocess infrastructure. Existing dependencies suffice.

Revision note: Created on 2026-09-24 after inspecting both creation surfaces and the background task architecture.

Revision note: Updated after implementation and initial compilation. Shared types follow the existing crate boundary; exact repository membership and folder filtering prevent surprising selections and unreachable truncated results.

Revision note: Final validation on 2026-09-24 recorded the small-window layout fixes, updated terminal fixture labels, boxed picker state, passing checks, and remaining scope boundary with issue #1128.
