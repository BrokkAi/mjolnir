# Make New immediate and preserve the startup workspace

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture


Pressing New must create a session with saved defaults without a dialog, task entry, click, or confirmation. Once ready, its normal composer receives focus by default; Setup can disable that focus. The full wizard remains on Shift-N or Alt-W. Opening a workspace must select or create a session in that workspace while keeping every workspace's sessions visible for explicit switching.

## Progress


- [x] (2026-09-08) Identify the unnecessary Quick New modal and the global-session startup selection regression.
- [x] Remove the modal and connect direct creation to ordinary composer focus.
- [x] Scope initial selection and automatic creation to the attached workspace without filtering the global list.
- [x] Add the user's subsequent request: a remembered Show stopped checkbox in the Sessions panel and an h shortcut, enabled by default.
- [x] Add behavior regressions, update documentation and captures, and pass full tests, Clippy, all-workspace/all-target compilation, formatting, and documentation validation.
- [x] Review the completed correction and prepare its commit on master.

## Surprises & Discoveries


The previous change interpreted “prompt by default” as a new task-entry dialog. The user clarified that creation must be immediate and the existing prompt should receive focus. Removing workspace filters from the session data also broadened startup's live-session checks and most-recent-session choice. New-session preparation already receives the terminal process's launch directory and creation already carries its attached workspace ID; preserve both.

## Decision Log


Remove the task modal and its TUI-only initial-task plumbing. Keep the existing daemon protocol fields for compatibility. Retain `startup.prompt` as the persisted setting, now meaning focus the normal composer after creation. Put attached-workspace interpretation in DashboardState so initial selection, startup hydration, startup choice, and automatic creation share it. Explicit selection of another workspace's session remains valid.

## Outcomes & Retrospective


All three requested corrections are implemented and validated: direct New with ordinary composer focus, startup scoped to the opened workspace, and a remembered stopped-session toggle in the Sessions panel. The correction does not move or delete existing sessions and does not create another release tag. The stopped-session toggle filters the panel immediately and saves in supervised background work; a failed save restores the previous preference and reports the error.

## Context and Orientation


`mj-tui/src/actions.rs` owns New's shortcuts, and `quick_new.rs` currently owns the unwanted modal. `mj-tui/src/lib.rs` owns selection and focus. `wizards/dashboard.rs` resolves saved launch defaults. `mj-cli/src/dashboard.rs` chooses the startup conversation and captures the launch directory. `dashboard/actions.rs` dispatches supervised creation, while `dashboard/io.rs` applies registration and provisioning results. The daemon remains the sole owner of durable session writes.

## Plan of Work


First replace the modal command with immediate creation and remove all modal rendering/input routes. Keep filesystem probes and provisioning in the existing supervised background work. Select the registered session and focus its normal composer when creation finishes, honoring the saved focus setting.

Next record the attached workspace in dashboard state. Use one workspace-scoped iterator for automatic startup decisions, and choose a default selection from that workspace. Preserve an explicit selection from another workspace during refreshes. Creation must continue using the captured launch directory and attached workspace ID.

Finally update shortcut/default documentation and remove the obsolete task-box capture. Add tests exercising one-key creation, configured focus, global visibility with workspace-local startup, and preserved launch context. Run the required checks and commit explicit changed files on master.

## Concrete Steps


From `/home/ryan/code/mjolnir`, run focused TUI and CLI behavior tests, then `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and `git diff --check`. All Cargo tests must run outside the restricted sandbox. Run documentation check/build and regenerate affected real-UI captures. Keep logs under `target/`. Stage only this correction's files and commit on the current branch.

## Validation and Acceptance


One New key emits creation immediately and opens no modal. Startup defaults determine profile/runtime; the launch directory is preserved. Successful creation selects the new session and focuses the regular composer unless disabled in Setup. A newer session in another workspace remains visible but cannot become the automatic startup conversation or prevent creation in an empty attached workspace. Explicit cross-workspace selection survives state refresh.

## Idempotence and Recovery


Use disposable test state. Preserve existing sessions, drafts, configuration, and published tags. Report provisioning failures through existing notices. Do not change registry publisher settings or perform a new release for this correction without instruction.

## Artifacts and Notes


The reported regression shipped in v2.2.0 at `08244de9`. `target/direct-new-tests.log` records the passing complete default-member suite, including all 360 TUI tests and all five real-PTY tests. The strengthened `first_launch_and_new_key_create_in_the_launch_context_without_task_input` test sends only Alt-N and verifies the second session is durably created with the original project directory and workspace, without a task. Its deliberately unavailable provider limits this test to registration; ordinary composer focus is tested through the actual completion-state method.

`target/direct-new-clippy.log` records warnings-denied Clippy success, and `target/direct-new-workspace.log` records successful compilation of every target in every workspace package, including desktop and voice. `target/direct-new-checkbox.log` records the final checkbox rendering test after its width adjustment. Documentation check reports zero errors/warnings; build produces 24 pages and validates 1,700 internal links. Four real Ratatui captures were regenerated, the obsolete quick-new capture was removed, and dashboard/Setup PNG renders were visually inspected.

## Interfaces and Dependencies


Use the existing Rust workspace, saved StartupConfig, session lifecycle, and regular chat composer. No new dependency is required. The added top-level show_stopped_sessions preference advances the configuration schema to 4 and the daemon protocol to 15, preventing an older daemon from discarding the new setting. Versions 1–3 upgrade in memory and on the next ordinary save; defaults continue showing stopped sessions.

Revision 2026-09-08: record the user's correction and the actual startup regression.

Revision 2026-09-08: include the user's stopped-session visibility toggle, persistence behavior, and required configuration/protocol compatibility changes.

Revision 2026-09-08: record completed implementation, terminal behavior verification, passing checks, and visual inspection before committing.
