# Add an opt-in, remembered fast-start workflow

## Purpose / Big Picture


`mj go [folder]` selects the workspace bound to that directory (or creates a new one) and opens its conversation without changing plain `mj`. The full dashboard remains available; switching directory-linked workspaces changes the source folder and saved New recipe together. First use chooses an account and execution target using the existing launch UI. Later invocations and the New button reuse those choices. A visible context banner identifies the source folder, actual session location, and account/target. Launch failures stay in the existing retry UI. This plan follows `.agents/PLANS.md`.

## Progress


- [x] (2026-09-16) Restore the full dashboard and workspace navigation while retaining deterministic directory entry and scoped New behavior.
- [x] (2026-09-16) Validate workspace switching, background launch ownership, and dashboard rendering; build the client and prepare the updated PR description.
- [x] (2026-09-16) Inspect startup, session creation, background jobs, configuration persistence, and TUI controls.
- [x] (2026-09-16) Implement saved global defaults and per-folder recipes, the go command, and initial setup.
- [x] (2026-09-16) Connect New and Change setup, visible context, and in-app launch failures.
- [x] (2026-09-16) Add behavior tests and pass the full dev-profile test suite, Clippy, formatting, CLI help, and diff checks.
- [x] (2026-09-16) Commit implementation as `526ffd93`, push `codex/mj-go-fast-start`, and open PR https://github.com/BrokkAi/mjolnir/pull/1046.
- [x] (2026-09-16) Correct the reviewed UX: bind folders to friendly workspace names, reopen the last conversation, and remove management/support panes from fast mode.
- [x] (2026-09-16) Verify the actual rendered fast screen, pass full dev-profile tests, Clippy, formatting, and CLI help checks.
- [x] (2026-09-16) Prepare the validated correction and revised description for delivery through existing PR #1046.

## Surprises & Discoveries


Plain `mj` opens the most recently used organizational workspace. A workspace is a grouping of sessions, not a filesystem directory. Session creation already runs in supervised background jobs and supports draft input while launching. Bare targets require a directory on the execution host; isolated targets require a repository bundle (a named list of repositories). These source semantics must be preserved, including the existing explicit treatment of remote clones.

## Decision Log


User feedback on 2026-09-16 supersedes the restricted-screen decision: the dashboard information and workspace navigation are useful. Go is a directory-aware entry and remembered-launch workflow, not a separate restricted interface. Restore the shared dashboard, commands, and focus traversal. Keep the readable names and current-context banner. Load saved workspace recipes before opening the UI, switch the active recipe with the workspace, and use the launch's originating workspace when a background preparation completes. Unbound workspaces use ordinary setup instead of inheriting another folder's recipe.

Use the existing session creation and failure/retry machinery. Keep go preferences in a separately versioned file beside config, using the shared lock and atomic-write helpers, so old config parsers and the database need no migration. Remember full recipes per canonical source folder and reusable account/target defaults globally. Never infer a remote bare path from a local path. Keep commits on the current branch as instructed, and publish a dedicated remote PR head without pushing master.

## Outcomes & Retrospective


Implemented the additive workflow. Go preferences are stored under the instance config directory in `go.json`. First setup establishes global defaults, subsequent setups remember per-project choices, and `--global-default` explicitly changes defaults for new projects. New and Alt-N reuse the recipe; plain mj retains the original wizard. The selected session's checkout and branch are read through the shared target command helper on a background job with a three-second process timeout and a five-second refresh interval. A failed observation is displayed as unavailable instead of retaining the previous path.

Cold target provisioning still takes the runtime's normal startup time; this change removes repeat setup navigation, not container boot or network latency. Live provider/runtimes are not exercised during development. The compact navigator was an overcorrection: the latest user feedback restores the ordinary dashboard and workspace navigation, retaining friendly conversation titles and remembered launch behavior. Launch errors offer retry and settings; manual host repair can still be needed, but prerequisite errors no longer require a doctor command.

The correction stores workspace IDs and last-selected conversation IDs alongside recipes in version 2 of go.json, retaining version 1 read support. Legacy hashed names are renamed through the daemon API, while directory identity is persisted separately from display names. Existing user-renamed bound workspaces keep their names. Reopening prefers the remembered eligible conversation, excluding other workspaces, archived sessions, and subagents; without a remembered choice it uses latest recorded update time. Stopped conversations still use the existing recovery wizard. Plain mj remains unchanged.

## Context and Orientation


`mj-cli/src/main.rs` routes CLI commands and attaches the terminal to the persistent controller process (daemon). `mj-cli/src/dashboard.rs` drives the terminal event loop. `mj-cli/src/dashboard/io.rs` runs supervised jobs and applies their replies. `mj-tui/src/wizards/dashboard.rs` owns launch choices and `mj-tui/src/combined.rs` renders the conversation. `mj-core/src/config.rs` supplies atomic file writes and a cross-process lock. The controller's existing `create_bundle_from_sources` interprets repository inputs and must be reused.

## Plan of Work


First add a shared recipe and preferences store, then add CLI arguments and deterministic folder/workspace selection. Extend the dashboard with optional go state; ordinary dashboards have none. Configure the first recipe through the existing wizard, preselecting the explicit folder and preparing repository bundles off the event loop. Save recipes before launching and surface persistence errors. Reuse a saved recipe on New, with an explicit Change setup action. Render actual selected-session context rather than merely repeating startup defaults. Preserve target failures in the application with retry/settings actions and remove doctor detours from prerequisite errors used by this workflow.

## Concrete Steps


Work in `/Users/ryansvihla/code/mjolnir`. Use `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Run every test command with sandbox escalation. Keep normal build output in the repository target directory. Review `git diff --check` and the final diff. Commit only changed files, push HEAD to a dedicated feature ref, and create a PR with concrete behavior and validation evidence.

## Validation and Acceptance


Tests must prove folder identity, saved recipe precedence, explicit setup changes, concurrent New without a wizard, and unchanged ordinary dashboard behavior. Verify the rendered banner includes selected-session identity and does not claim a remote clone is the source folder. Launch and persistence failures must offer retry without losing the draft or choosing a different target. CLI help must describe go and its settings scope. Live provider authentication is not assumed; tests use isolated files and existing test fakes.

## Idempotence and Recovery


Preference updates use a stable lock plus atomic replacement. Failures are reported rather than silently defaulted. Session creation uses existing cancellation and cleanup. New creates another session and does not stop existing work. No schema migration, live store upgrade, or provider login is performed during development.

## Artifacts and Notes


Focused fast-mode behavior tests pass, including mouse activation of New and unchanged ordinary wizard behavior. `cargo run -q -p brokk-mjolnir -- go --help` shows the new command and settings options. `cargo test --quiet`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` all pass. The full suite includes the existing worker fault-injection tests. No real provider authentication or live Docker/SSH/EC2 provisioning was performed.

## Interfaces and Dependencies


Use existing Path/PathBuf, serde, existing Config lock/atomic writes, TUI DashboardAction and wizard state, and supervised dashboard IO. Add no crates or new runtime dependencies. Expose only the minimum shared recipe/store and go-state interfaces needed by CLI and TUI.

Plan created 2026-09-16 to capture the authorized additive workflow and PR delivery.

Updated 2026-09-16 with implemented behavior, validation progress, and explicit runtime limitations.

Completed 2026-09-16: implementation and validation delivered in PR #1046. Remote master was not changed.

Reopened 2026-09-16 following the user's screenshot: minimized dashboard panels, hashed workspace names, and automatically creating a fresh session still violate the intended simple workflow. Add durable folder-to-workspace bindings and last-conversation selection in version 2 of go.json (read version 1, refuse unknown newer versions). Reuse and rename legacy hashed workspaces through existing daemon APIs. Render a small conversation navigator, New and Menu, and the conversation/composer; exclude workspace, quota, and target panels and their focus stops. Show conversation names instead of session IDs in fast-mode headers. Regression tests must assert both required controls and the absence of the unwanted panels, plus selection persistence and workspace reuse. Keep normal mj unchanged.

Correction validated 2026-09-16: all 14 focused go tests pass, the full dev-profile suite passes, and Clippy, formatting, diff checks, and CLI help pass. A terminal-buffer SVG captured by the rendered-context test was rasterized with Quick Look and visually inspected. The tested client was built with `cargo build -p brokk-mjolnir`; existing worker binaries and the daemon protocol are unchanged. No live provider session was started for validation.

The local client at `/Users/ryansvihla/.local/bin/mj` was replaced by atomic rename after retaining the previous executable at `/private/tmp/mj-go-review.R3b6z4/mj.previous`. The next terminal attachment uses the correction; already-running terminal UIs and sessions are left untouched. Detach with Alt-Q, then run `mj go` from the project folder.

Revised 2026-09-16 after the user approved the direction but requested useful dashboard information and workspace switching back. The new acceptance scenario is: run go in A, switch to an existing directory-linked B and use B's New recipe, then run go in A again and return to A regardless of the most recently visited workspace. A different unbound directory must get a new bound workspace, never borrow the active workspace merely because it is open. The first milestone restores shared rendering/navigation; the second scopes cached project recipes and delayed launch completions to workspace IDs; the third runs focused and full validation, installs the client without stopping sessions, and updates PR #1046.

Dashboard restoration validated 2026-09-16: the full dev-profile test suite, Clippy, changed-crate formatting checks, CLI help, and diff checks pass. The renderer capture includes workspace navigation, session information, target/quota panels, friendly titles, and the source/working-directory banner. Workspace-wide formatting reports an unrelated existing difference in `mj-controller/src/review_host.rs`; that file is deliberately left unchanged. No live provider provisioning was tested. The previous client is backed up at `/private/tmp/mj-go-dashboard.BTNpgF/mj.previous`; the validated replacement is installed by atomic rename without restarting live sessions.

A concurrent git pull rebased the earlier three feature commits onto updated master. `git range-diff` confirms all three patches are unchanged. PR delivery therefore uses an exact-old-head lease on the feature ref only; remote master remains untouched.
