# Open empty workspaces at a ready prompt

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Running `mj` in a project directory should create the first workspace without a picker, start a Codex session using that directory when the workspace has no live sessions, and focus the prompt. Automatic targets prefer usable Podman, then Docker, then a local directory session. Existing workspace selection and existing-session startup remain unchanged. Users can choose another configured profile and target for automatic sessions. The explicit `mj workspaces` and `mj setup` commands retain their management flows.

## Progress

- [x] (2026-09-07) Inspect workspace selection, configuration, session creation, and prompt attachment.
- [x] (2026-09-07) Implement first-workspace creation, configurable automatic session selection, minimal first-run configuration, and runtime-based target defaults.
- [x] (2026-09-07) Add tests for empty/populated startup, defaults, configuration preservation, plain directories, launch transitions, and startup without terminal input; update human documentation.
- [x] (2026-09-07) Complete `cargo test`; all default-workspace tests pass, including the first-launch PTY test. Existing ignored tests remain ignored.
- [x] (2026-09-07) Pass formatting, `cargo clippy --all-targets -- -D warnings`, docs check/build, and internal link validation; review the diff.
- [x] (2026-09-07) Commit the validated implementation on the existing `master` branch.

## Surprises & Discoveries

Existing live sessions already auto-open by recent activity in `mj-cli/src/dashboard.rs`. Empty workspaces explicitly focus Sessions instead. New session creation already runs in background tasks, reports progress and failure, and supports cancellation. Reuse that lifecycle rather than adding a second launch implementation.

## Decision Log

The user's clarification limits the change to empty-workspace onboarding. Keep the rules for choosing among existing workspaces, including attached workspaces, unchanged. Automatic local sessions use the launching client's current directory; a workspace itself is a named group of sessions, not a filesystem directory. Prefer Codex, then an existing configured profile if Codex is absent. An explicit startup profile takes precedence. Following the user's additional instruction, automatically probe Podman, then Docker, and fall back to a local directory. Reuse configured targets of the chosen kind and their settings; add the shared standard target template when none exists. Explicit target choices bypass automatic selection, including its fallbacks. Never choose a remote host implicitly. Containers use the existing quick-bundle helper to persist the current repository as a local source and include its uncommitted contents. A directory outside Git uses local execution without inventing a repository. Existing local Git worktree behavior is preserved. Preserve interactive setup on platforms without local bare execution.

## Outcomes & Retrospective

The terminal now creates its first workspace automatically, launches a default session in an empty workspace, and preserves prompt focus through registration and launch completion. Existing populated workspace selection is unchanged. Startup profile and target overrides are validated and survive profile/target renames. Automatic target selection checks Podman, then Docker, then uses local execution; it preserves configured target settings and supports plain directories. All required Rust checks pass, as do the documentation check/build and 1,696 internal-link checks. The real-terminal test proves first-workspace and session creation without keyboard input and responsive quitting. Tests use an intentionally unavailable harness; no paid model prompt or production container was launched. Windows retains its existing setup path.

## Context and Orientation

`mj-cli/src/main.rs::run_workspace_dashboard` selects a named workspace through the persistent background controller (the daemon). `mj-cli/src/dashboard.rs::DashboardContext::open` loads the terminal surface and starts its background feeds. `hydrate_stored_session_summaries` chooses existing live sessions. `mj-tui/src/wizards/dashboard.rs` builds session launch actions, and its parent module owns the existing raw-project identity helper. `mj-cli/src/dashboard/io.rs` submits launch actions to the daemon and applies registration and completion events. `src/hel_config.rs` defines versioned TOML configuration. `mj-controller/src/hel_setup.rs` owns configuration initialization.

## Plan of Work

First add a defaulted `[startup]` configuration section with optional profile and target IDs and an opt-out for automatic sessions. Add an empty-workspace action builder beside the existing new-session wizard, reusing project identity handling. Test that active sessions suppress creation, a stopped session does not, the current directory reaches the launch action, and prompt focus is immediate.

Then automatically create the first workspace during ordinary launch, preserving explicit picker behavior. For an empty configuration on Unix, initialize a Codex profile using its existing home environment convention and a local bare target without the setup questionnaire. Run configuration preparation off the UI loop. An empty workspace emits `DashboardAction::CreateStartupSession` once on opening. The existing creation worker calls `mj-cli/src/dashboard/startup.rs::prepare_session_launch` to probe runtimes with bounded cancellable subprocesses, prepare the project bundle, and convert the request into `CreateSession`. Startup profile/target references participate in configuration validation, target migration, and profile/target renames. An empty workspace invokes the existing background launch lifecycle once on opening; errors remain visible and do not retry every frame.

Finally update the configuration reference, quickstart, and README. Validate with repository checks and commit the coherent implementation.

## Concrete Steps

From `/workspace/mjolnir`, run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Use colocated Rust behavior tests, including startup action input and focus checks, configuration round trips, and first-workspace selection decisions. Review `git diff --check` and the final diff, then stage only changed files and commit directly to the current branch.

## Validation and Acceptance

On a Unix host with Codex credentials, launch `mj` from a project with an isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`: observe automatic workspace creation, visible session launch progress, and a focused prompt when ready without wizard input. Existing multiple-workspace installations must still show their picker. An existing live session must never trigger a new startup session. A configured alternate profile must be selected for an empty workspace. Missing prerequisites must produce an actionable failure while leaving the dashboard responsive. No test should submit a paid agent prompt.

## Idempotence and Recovery

Minimal initialization must only write a completely unconfigured installation, preserving configured profiles, targets, and settings. Automatic launch happens once per dashboard opening, not on state refreshes or after session deletion. Failed launches remain inspectable through the normal lifecycle. Explicit setup and new-session commands remain recovery paths.

## Artifacts and Notes

Validation logs are stored in the ignored `target/startup-*.log` files. The first full run identified old PTY startup input that must be removed now that the workspace picker is skipped. Subsequent testing identified an exact subprocess-count assertion in bare-directory validation; it now tests actual repository, plain-directory, and missing-directory behavior. Final results will be recorded below.

## Interfaces and Dependencies

Extend existing configuration and dashboard modules; add no crate or library dependencies. Use `PathBuf` for the project directory, the existing raw-project context helper moved to `src/hel_config.rs` for reuse at the identifier boundary, and `DashboardAction::CreateSession` for supervised background work.

Initial plan recorded after inspecting the existing startup and lifecycle code and incorporating the user's scope clarification.

Plan revised to include the user's configurable target preference, runtime detection, current-repository preparation, plain-directory support, and the resulting validation work.

Final validation: `cargo test` exited 0; `cargo clippy --all-targets -- -D warnings` exited 0; `cargo fmt --all -- --check` and `git diff --check` passed. In `docs`, `npm run check` reported zero errors/warnings and `npm run build` checked 1,696 links across 24 pages. The final source was unchanged during these successful checks.
