# Make session management direct and put configuration in Setup


This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture


Users must be able to see and switch every session across workspaces in a full-height sidebar. Stopping and restarting take one key each without confirmation. Deletion asks only Yes or No. New session opens a fresh task prompt before launching with saved defaults; the existing detailed wizard remains on a separate key. A Setup modal makes configuration editable without leaving the terminal UI or editing a configuration file.

## Progress


- [x] (2026-09-08) Read repository instructions and traced session filtering, creation, configuration, and rendering.
- [x] (2026-09-08) Implemented global session visibility and direct lifecycle keys.
- [x] (2026-09-08) Implemented isolated quick task creation, durable task handoff, and configurable sidebar placement.
- [x] (2026-09-08) Implemented the complete Setup editor, background discovery, concurrent-edit-safe persistence, and stale-result protection.
- [x] (2026-09-08) Updated behavioral tests, guides, and five real-UI captures; full Cargo tests, formatting, Clippy, and documentation validation pass.
- [x] (2026-09-08) Reviewed the final diff and included the validated implementation in the required commit on master.

## Surprises & Discoveries


Focusing the old composer during asynchronous creation could send a task to the previous conversation. A separate quick prompt avoids that. Registration previously omitted `draft_input` on insertion; the new durability test exposed this, and the insert now initializes it without overwriting later edits on updates. Only the daemon owns database writes, so accepted task consumption also runs there. New and Setup dialogs carry generation identifiers so late background results cannot modify newer dialogs. Existing Alt-T and Alt-R belong to transcript rendering and history search; stop/restart therefore use the unmodified session-pane keys s/r.

Session filtering happens both when loading a controller and when the daemon publishes runtime snapshots. Removing just the UI filter would reintroduce the bug on the next refresh. Existing startup preparation already probes usable Podman and Docker off the UI loop and falls back to local execution; quick creation can reuse it.

## Decision Log


- Decision: Quick New opens a fresh task prompt by default, with a saved option to skip it. The prompt is committed with the new session before provisioning and submitted only to that session; daemon-owned consumption clears only the matching saved draft. Keep the detailed creation wizard under a distinct command. Reuse startup profile and target defaults, with Codex preferred and automatic local runtime selection.
  Rationale: This directly removes the repeated selection steps while preserving custom launches.
  Date/Author: 2026-09-08, Codex.
- Decision: Keep session ownership in its original workspace when viewing or restarting it from the global list.
  Rationale: Switching sessions is navigation, not a request to move the session.
  Date/Author: 2026-09-08, Codex.

## Outcomes & Retrospective


All seven requested outcomes are implemented. Sessions remain visible across workspaces and when configuration entries are removed. Deletion requires only Yes/No; stop and restart run from one key; quick New collects a separate task with saved defaults, while the original wizard stays available. The sidebar supports either side, and Setup exposes every user configuration field. All required validations pass. The daemon protocol advances from 13 to 14 for the global snapshot and initial-task handoff fields. No release tag or push is part of this work.

## Context and Orientation


`mj-tui/src/lib.rs` owns terminal state and returns `DashboardAction` values without doing I/O. `actions.rs` defines commands and key bindings; `dialogs.rs` handles confirmation; `wizards/dashboard.rs` handles creation and resume. `combined.rs` allocates screen space. `mj-cli/src/dashboard/actions.rs` dispatches background work, and `io.rs` applies results. `mj-cli/src/daemon.rs` publishes runtime updates. `src/hel_config.rs` defines validated, serialized settings and locked updates. The daemon is the persistent process managing sessions independently of the terminal.

## Plan of Work


First remove workspace and lifecycle visibility restrictions from the dashboard data path, while preserving workspace ownership and read receipts. Bind direct stop/restart/delete commands and replace typed destruction confirmation with Yes/No. Add quick creation using existing supervised startup preparation, leaving the old wizard separately accessible.

Next allocate a left or right session column spanning the conversation, composer, targets, and quota. Preserve pane size controls as sidebar width controls. Add persisted placement and prompt defaults.

Finally add a modal editor for configuration, including profiles, targets, bundles, startup, review, display, and web settings. Reuse typed configuration validation and supervised background save operations. Keep external discovery and writes away from the event loop, and show failures in the modal.

## Milestones


The first milestone removed both dashboard and daemon workspace filters for the global session list. Workspaces retain ownership of their sessions; resume, restart, draft persistence, and read receipts use the session's own workspace. The second milestone replaced destructive typing with Yes/No and added direct stop/restart keys, a full-height side column, and quick task creation. The final milestone added Setup for every persisted user setting, updated the guides, and rendered documentation captures using the real UI.

## Concrete Steps


Work in `/home/ryan/code/mjolnir`. Inspect the named modules with `rg` and targeted reads, make changes, and add colocated behavioral tests. Run `cargo fmt --all -- --check`, `cargo test` outside the restricted sandbox, and `cargo clippy --all-targets -- -D warnings`. Review `git diff --check` and the final diff. Stage only changed files and commit on the existing branch without pushing.

## Validation and Acceptance


Tests must demonstrate sessions from two workspaces and stopped sessions remain visible after reloads; stop and restart emit lifecycle actions on one key without opening a confirmation; delete opens a Yes/No choice; quick New skips the wizard while the detailed key opens it; the sidebar occupies the left/right edge of the entire content; configuration edits persist, validate, and report save failures. Existing creation, lifecycle, modal, and input tests must pass with updated expectations where requested behavior changed.

## Idempotence and Recovery


Configuration writes use the existing locked update APIs. Session lifecycle work uses the existing supervised operations and cancellation. Test data must use temporary fixtures; do not delete user sessions, branches, or workspaces as part of implementation. Preserve unrelated changes and commit only this task's files.

## Artifacts and Notes


Final validation in `/home/ryan/code/mjolnir`:

    cargo test --no-fail-fast
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check
    cargo test -p brokk-mj-tui generate_documentation_screenshots -- --ignored --nocapture

All returned exit status 0. Cargo tests ran outside the restricted sandbox. The TUI suite passed 358 tests, and all other workspace suites and terminal termination tests passed. Logs are in `target/session-ui-tests.log`, `target/session-ui-clippy.log`, and `target/session-ui-screenshots.log`.

In `docs/`, `npm run check` reported no errors or warnings; `npm run build` built 24 pages and verified 1,701 internal links. The dashboard, quick prompt, wizard, palette, and Setup captures were rendered through Ratatui and visually inspected. Geometry tests cover both sidebar positions at 140 by 32, 72 by 20, and 32 by 16. A 70 KB multiline task verifies isolated prompt handling and durable initialization; matching-draft consumption preserves newer edits. Setup tests cover edits, validation, saving, stale results, and concurrent external changes.

## Interfaces and Dependencies


Use existing Rust workspace crates, Ratatui rendering, Crossterm input, shared form/text controls, `HelConfig` serialization and validation, and dashboard background workers. No new crate or dependency boundary is required.

Revision note: Initial plan captures all seven requested outcomes and the two-layer workspace filtering discovered during inspection.

Revision note: All seven outcomes and final validation are complete. A final regression also keeps existing sessions visible when all accounts or targets are removed in Setup.
