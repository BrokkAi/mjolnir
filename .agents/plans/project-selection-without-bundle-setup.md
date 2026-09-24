# Start sessions without bundle setup

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Starting work must not require learning what a bundle is or saving one before a session can begin. A bundle is the existing internal configuration describing repositories to clone together. Keep that storage format, but prepare it automatically when a person chooses a project. The terminal should use the repository it was launched in automatically, and both terminal and browser should offer known projects and folder browsing. Pasting a repository link and composing several repositories remain optional.

## Progress

- [x] (2026-09-24) Traced terminal and browser creation, existing automatic preparation in `mj go`, recent directory history, and asynchronous path completion.
- [x] (2026-09-24) Implemented automatic use of the terminal repository, direct known/recent project selection, a first-project editor without a configuration step, and folder browsing.
- [x] (2026-09-24) Implemented browser project selection, folder browsing, and preparation on Next; all 22 creation interaction tests passed.
- [x] (2026-09-24) Added regressions for automatic preparation, recents, folder navigation, retries, duplicate requests, and replacement drafts.
- [x] (2026-09-24) Full Rust suite, Clippy, formatting, 45 browser unit tests, and 94 browser interaction tests passed. Final review completed; the implementation and this plan form one commit at the existing HEAD.

## Surprises & Discoveries

`mj-cli/src/go.rs::resolve_recipe` already prepares an internal bundle without asking the user to create one. The normal wizard needlessly exposes that preparation as an extra form. Directory completion already executes outside the UI loops and supports the machine that owns the path. Local sources for isolated sessions resolve a network remote; they do not copy unpublished changes. Keep that distinction visible at review.

## Decision Log

- Decision: Keep existing configuration, API identifiers, and creation jobs; replace the user interaction rather than migrate storage.
  Rationale: Existing sessions and multi-repository configurations remain usable, with no database or wire migration.
  Date/Author: 2026-09-24, Codex.
- Decision: Reuse known local projects and the current terminal repository, and expose the existing asynchronous directory listing through explicit browse actions.
  Rationale: Users can start local projects without typing paths or configuring repositories. The browser must browse the controller or selected remote host, never assume its own device's folders belong to that host.
  Date/Author: 2026-09-24, Codex.

## Outcomes & Retrospective

Both interfaces now prepare internal repository configuration automatically. Terminal users in a repository bypass project setup, and other users choose a known project, browse folders, or paste a link. Full validation passes. Existing multi-repository configurations remain usable and unpublished local changes are still explained at review. No implementation work remains.

## Context and Orientation

`mj-tui/src/wizards.rs` holds terminal wizard state; `wizards/dashboard/new_session.rs` advances creation and applies background preparation results; `wizards/draft.rs` declares and handles controls; `wizards/render.rs` draws them. `mj-cli/src/dashboard/io/spawn.rs::spawn_create_bundle` already performs the filesystem/configuration work in a tracked background job. `mj-controller/src/web/viewer.js` implements the browser wizard, calling the authenticated `/api/bundles` endpoint for the same preparation. `mj-controller/src/server/viewer_types.rs` projects configured repositories and recent host directory history. Browser and terminal path fields already use asynchronous directory completion. None of the new selection/rendering logic should perform filesystem or network I/O directly.

## Plan of Work

### Milestone 1: terminal project selection

First replace the terminal's configured-bundle-only choice with project choices that include the current repository, saved configurations, and recent local directories. Selecting a source issues the existing preparation action and then enters review. Starting from a repository should prepare that project automatically on the first visit; Back permits choosing another. Rename the optional source editor in user-facing text, remove its separate save/create language, and make the ordinary continuation prepare the project. Preserve optional multiple repository composition.

### Milestone 2: browser project selection

Then replace the browser's create/save panel with direct project choices and an optional repository field. Next prepares a selected source and proceeds to preflight in the same interaction. Show controller-local recent directories for isolated targets, and add explicit folder browsing that starts at home and lets users choose directories without typing. Retain draft identity guards, errors, busy state and prevention of duplicate submissions. Keep explicit review of network sources and excluded local changes.

## Concrete Steps

Work in `/home/ryan/.codex/worktrees/dc4c/mjolnir`. Run focused Rust wizard regressions and browser interaction tests during implementation. Run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` before committing. Every `cargo test` must run with elevated permissions, as required by AGENTS.md. Browser deterministic tests run from `tests/e2e/web` with `npm test`. Any actual new-build daemon or CLI invocation must use `--instance project-picker-test` and isolated configuration/data. Do not use the host's live default instance.

## Validation and Acceptance

With no configured bundles, a terminal launched inside a repository can advance from target selection to review without entering any source or creating configuration explicitly. Outside a repository, picking a recent project prepares it directly. Existing multi-repository projects remain selectable. Folder browsing populates the project source without keyboard path entry. A pasted repository link advances directly to review. Preparation failure leaves the chosen source available for correction and retry. Browser refreshes and delayed responses cannot reset or mutate a different draft, and repeated Next does not submit duplicate preparation. Tests must drive the UI transitions and resulting creation/preflight actions.

## Idempotence and Recovery

Preparation uses the existing exact-source reuse behavior, so repeated selection reuses configured projects. Errors retain the draft. No migrations or destructive operations are planned. Commit only changed files at the existing HEAD and do not push. This worktree began detached; preserve that state rather than create or switch branches.

## Artifacts and Notes

Validation completed successfully: `cargo test --quiet --no-fail-fast`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. Browser `npm test` passed 45 unit tests and 94 interaction tests with 3 existing skips. The terminal source editor also passed its mouse-interaction regression at the minimum 60-column width.

The first full Cargo run timed out in the existing eight-second concurrent-start test while Clippy compiled. The final sequential run passed that test and the whole suite. Hand-written browser input fakes in both JavaScript and Rust needed their DOM dataset field added, and the draft fake needed its source field; real browser interaction tests already passed. Chromium and its Linux libraries were initially absent and were installed with approved elevated commands.

## Interfaces and Dependencies

Reuse `DashboardAction::CreateBundle`, `DashboardAction::CompletePath`, `PathInput`, and the browser's existing `/api/bundles`, `/api/paths/complete`, and `/api/preflight/new` contracts. Internal names may remain compatible while all new-session product text uses project/repository terminology. No new dependency or crate is needed.

Initial plan recorded 2026-09-24 after tracing the existing implementation.

Updated 2026-09-24 after implementation and focused interaction validation; first-time terminal users now skip the empty configured-project list as well.

Final update 2026-09-24: completed validation, preserved legacy documentation anchors, and verified folder controls at the terminal minimum width. The final commit includes only the files changed for this task.
