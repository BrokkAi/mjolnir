# Add a minimal `mj simple` terminal mode

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan is maintained in accordance with `.agents/PLANS.md` from the repository root. It is self-contained: a contributor should be able to implement and validate the work using only this plan, the current working tree, and `.agents/PLANS.md`.

## Purpose / Big Picture

People using Mjolnir inside tmux currently have to navigate the normal workspace dashboard, create a session through a multi-step wizard, choose a target, and avoid Ctrl+B because tmux consumes it first. After this change, a person can run `mj simple` inside a repository and immediately attach to, or start, one straightforward local session. The session uses the default enabled agent profile, always uses the local `localhost` target, creates an isolated checkout, disables automatic review for that session, and uses direct Ctrl-key commands with no prefix state at all. Ctrl+B is never claimed by Mjolnir.

The observable result is:

    cd /path/to/repository
    mj --instance simple-check simple

No profile, target, project, or review wizard appears. The dashboard opens the last eligible simple session in this folder or starts a new one on `localhost` with an isolated checkout. Pressing Ctrl+B is left for tmux. Pressing Ctrl+F1 opens Mjolnir help, and Ctrl+Q detaches, without arming a second key.

## Progress

- [x] (2026-10-20T07:03:18Z) Traced the existing `mj go` startup flow, workspace binding, session creation action, review selection, and prefix-key router.
- [x] (2026-10-20T07:03:18Z) Confirmed the user wants a separate `mj simple` command rather than changing `mj go`.
- [x] (2026-10-20T07:19:47Z) Added `mj simple [<folder>]`, default-profile selection, strict local-bare target validation, and fixed local isolated-checkout recipe construction.
- [x] (2026-10-20T07:19:47Z) Added Simple startup invariants in `mj-tui`: direct launch without wizards, eligible-session reuse, and no writes to the normal Go recipe.
- [x] (2026-10-20T07:19:47Z) Carried a per-session review choice through dashboard creation and forced `SessionReview::Off` for Simple mode.
- [x] (2026-10-20T07:19:47Z) Replaced prefix behavior only in Simple mode with direct Ctrl chords and kept configuration reloads from restoring the normal map.
- [x] (2026-10-20T07:19:47Z) Added router/state tests and a real-PTY regression proving `Ctrl+B q` does not detach while direct `Ctrl+Q` does.
- [x] (2026-10-20T07:19:47Z) Passed formatting, the full touched-crate suites, workspace Clippy with warnings denied, and the Simple-mode PTY regression.
- [ ] Commit the validated change on the current branch.

## Surprises & Discoveries

- Observation: The existing dashboard session creation path always sends `review: None`, so global `[review] enabled = true` applies to sessions created from the TUI. Merely suppressing reviewer discovery in the UI would not disable automatic turn review.
  Evidence: `mj-cli/src/dashboard/io/spawn.rs` constructs `daemon::CreateSessionRequest { review: None, ... }`.

- Observation: `Config::with_local_targets` supplies `localhost` as a `LocalBare` target when the host can run workers, but explicit user configuration can replace it.
  Evidence: `mj-core/src/config.rs` inserts the built-in target only through a vacant map entry.

- Observation: Dashboard configuration reloads rebuild `DashboardState::keybinds` from the reloaded `Config`. A one-time prefix replacement would be lost on the next daemon config refresh.
  Evidence: `mj-tui/src/ingest.rs` assigns `self.keybinds = config.keybinds()` in `set_config`.

- Observation: Adding the session review choice to `DashboardAction::CreateSession` changes a debug-formatted wizard golden even though rendered UI text is unchanged.
  Evidence: `golden_create_delegation_availability_and_submission` differed only by `review: None` in the printed action.

- Observation: A real terminal preserves the distinction needed for this feature: the old `Ctrl+B q` bytes leave a Simple dashboard running, while the direct detach byte `Ctrl+Q` exits it.
  Evidence: `simple_mode_detaches_on_direct_ctrl_q_and_never_claims_ctrl_b` passes in `mj-cli/tests/termination_pty.rs`.

## Decision Log

- Decision: Add a separate `mj simple` subcommand and leave `mj go` behavior unchanged.
  Rationale: The user explicitly allowed either hijacking `go` or adding `simple`, then stated that a new command is acceptable. A separate command keeps the established fast-start workflow and its saved target choices intact.
  Date/Author: 2026-10-20 / Codex

- Decision: Reuse the internal Go startup machinery with an explicit `simple` marker rather than build a second dashboard startup path.
  Rationale: Workspace binding, remembered-session lookup, launch preparation, standby composers, and lifecycle ownership already work. Duplicating them would create two owners for the same startup state and risk divergent behavior.
  Date/Author: 2026-10-20 / Codex

- Decision: A Simple session always uses the target id `localhost`, the current folder as its project directory, and `create_managed_worktree: Some(true)`.
  Rationale: These are the requested invariants. If `localhost` was overwritten with a non-local target, `mj simple` fails visibly instead of silently choosing a remote or container target.
  Date/Author: 2026-10-20 / Codex

- Decision: Resolve the default profile from the saved fast-start recipe when that profile is enabled, otherwise use the first enabled profile. Fail with setup guidance when no enabled profile exists.
  Rationale: This preserves the account a person chose as their default while remaining usable after that saved profile was disabled. Asking for an agent in the terminal would violate the no-wizard requirement.
  Date/Author: 2026-10-20 / Codex

- Decision: Reuse only a remembered Simple session when it is on `localhost` with `create_managed_worktree == Some(true)`. Otherwise create a new session.
  Rationale: A folder can already contain normal Mjolnir sessions on other targets. Attaching one of those would violate Simple mode's local isolated-checkout guarantee.
  Date/Author: 2026-10-20 / Codex

- Decision: Disable automatic review by setting the new session's own `SessionReview::Off` value.
  Rationale: Per-session review choice overrides global `[review]` without mutating the user's configuration or affecting other clients and sessions.
  Date/Author: 2026-10-20 / Codex

- Decision: Replace prefix behavior completely in Simple mode with direct Ctrl-key bindings.
  Rationale: The user clarified that no two-step prefix state should remain. Direct bindings make every command one chord and leave Ctrl+B, on its own, for tmux. Configuration reloads will reapply the direct map.

- Decision: Use familiar direct keys for common commands and Ctrl+Alt variants for pane and workspace commands that would otherwise replace essential composer editing keys.
  Rationale: Assigning Ctrl+H, Ctrl+J, Ctrl+K, and Ctrl+M directly would intercept backspace, newline, line deletion, and Enter in many terminals. The Ctrl+Alt forms remain direct one-chord commands without taking those editing keys.
  Date/Author: 2026-10-20 / Codex

## Outcomes & Retrospective

The implementation achieves the planned user-visible behavior. `mj simple` resolves a workspace from its folder, reuses only a running local isolated Simple session, or creates one directly with the saved/default enabled profile. It never opens the session wizard, never changes the folder's normal Go recipe, creates no automatic review override other than `SessionReview::Off`, and has no prefix state. The focused key, state, full crate, Clippy, and real-PTY validations pass. The only remaining step is the required commit.

## Context and Orientation

Mjolnir has one binary, `mj`, built from `mj-cli`. Its terminal dashboard lives in `mj-tui` and the persistent controller ("daemon") lives in `mj-controller`. A workspace groups sessions shown as tabs. A session is one agent conversation plus its worker and checkout. A target names where that worker runs; the built-in `localhost` target runs it on this machine. A managed worktree, now implemented as an isolated clone for new sessions, is a session-owned checkout that does not modify the user's original checkout.

`mj go` is the existing fast-start command. `mj-cli/src/go.rs` reads `go.json`, binds a folder to a workspace, and builds a `mj_tui::GoMode`. `mj-tui/src/go.rs` uses that mode to open an existing session or launch a recipe. `mj-cli/src/dashboard.rs` resolves the workspace and drives the startup action through the same dashboard event loop as ordinary Mjolnir.

The general session dashboard action is `DashboardAction::CreateSession` in `mj-tui/src/lib.rs`. The CLI executor in `mj-cli/src/dashboard/actions.rs` starts the launch, and `mj-cli/src/dashboard/io/spawn.rs` registers the session with the daemon. The daemon request type is `mj_client::daemon::CreateSessionRequest`; `mj-controller/src/daemon/create.rs` passes its `review` field into the session record.

The key configuration is implemented in `mj-core/src/config/keys.rs`. A prefix key arms a pending state; the next key selects an action. `mj-tui/src/keybinds.rs` routes key presses and `mj-tui/src/keybinds.rs` labels commands for footers and help. `DashboardState` stores the resolved `Keybinds`.

## Plan of Work

First, add a public `simple: bool` marker to `mj_tui::GoMode`. Default constructors for ordinary Go mode set it to false. Add a small CLI module at `mj-cli/src/simple.rs` that parses an optional folder, loads `Config` and `GoPreferences`, chooses the default enabled profile, requires `localhost` to exist and be `TargetTemplate::LocalBare`, and builds a `GoMode` with a recipe fixed to the current folder, `localhost`, `create_managed_worktree: Some(true)`, and `simple: true`. Register `Command::Simple(simple::SimpleArgs)` in `mj-cli/src/main.rs`, classify it as a TUI process, route it to `run_workspace_dashboard`, and add parser/help expectations.

Second, teach `DashboardState::begin_go` about the Simple marker. Record the marker on the dashboard, install Simple key bindings, and filter a proposed startup session so only local isolated sessions are reused. When no eligible session exists, return `DashboardAction::GoLaunch` directly for the fixed recipe. Never call `change_go_setup` or `begin_new_wizard` for a Simple mode launch. Add a `simple_mode()` accessor and make `set_config` rebuild either normal or Simple bindings according to that marker.

Third, add `review: Option<mj_core::config::SessionReview>` to `DashboardAction::CreateSession` and carry it through every constructor and destructure. Ordinary wizard and Go launches pass `None`; Simple launches pass `Some(SessionReview::Off)`. Pass the value into `daemon::CreateSessionRequest.review` instead of the current hardcoded `None`.

Fourth, add Simple key binding construction in `mj-core/src/config/keys.rs`. Start from `KeysConfig::default()`, change only `prefix` to `"ctrl+space"`, resolve it, and expose it through a clearly named helper such as `simple_keybinds()`. Add a focused test proving the prefix is not Ctrl+B. In `mj-tui`, add tests that entering Simple mode changes the live prefix, a later config reload keeps Ctrl+Space, and Ctrl+B is not the prefix.

Finally, add a state-machine test in `mj-tui/src/go.rs` proving a Simple launch with no eligible remembered session returns `GoLaunch` immediately and never opens `Mode::New`; then prove the corresponding created action carries `SessionReview::Off`. Extend existing test constructors for the new `GoMode` and `CreateSession` fields. Update README or CLI documentation only where the command list or quick-start examples are generated or maintained, if such a location exists.

## Concrete Steps

Run all commands from `/home/ryansvihla/code/mjolnir`.

Inspect the relevant startup and key paths before editing:

    sed -n '1,240p' mj-cli/src/go.rs
    sed -n '1,230p' mj-tui/src/go.rs
    sed -n '470,720p' mj-core/src/config/keys.rs
    sed -n '1380,1510p' mj-cli/src/dashboard/io/spawn.rs

After implementation, format and run focused tests:

    cargo fmt --all -- --check
    cargo test -p brokk-mj-tui simple
    cargo test -p brokk-mj-core simple_keybinds
    cargo test -p brokk-mjolnir simple

The package names and crate names are visible in the workspace manifests. If a focused test filter runs zero tests, use the exact module path printed by `cargo test -- --list` rather than treating it as success. Cargo tests must be run outside the restricted sandbox with elevated permissions because this repository's tests exercise loopback sockets and PTYs.

After focused tests pass, run the required full validation:

    cargo test
    cargo clippy --all-targets -- -D warnings

Then inspect and commit only the changed files:

    git status --short
    git diff --check
    git diff --stat
    git add <each changed file>
    git commit -m "Add minimal simple mode"

## Validation and Acceptance

With an enabled profile and the built-in local target, running `mj --instance simple-check simple` in a Git repository must open either a remembered eligible Simple session or a newly launched session. It must not render the New session profile, target, project, or review steps. The new session's target is `localhost`, `create_managed_worktree` is true, and its stored review override is off; automatic review therefore does not start after a turn even when global `[review] enabled = true`.

From a dashboard launched by `mj simple`, help must list direct Ctrl commands and no prefix banner may arm. Pressing Ctrl+F1 must open help. Pressing Ctrl+Q must detach. Pressing Ctrl+B must not be consumed as a Mjolnir prefix. A configuration reload must not restore the normal prefix map within the Simple dashboard.

If `localhost` is absent or is not `LocalBare`, `mj simple` must exit with an error that names the `localhost` requirement. If no enabled profile exists, it must exit with guidance to configure or enable one rather than opening the session wizard.

## Idempotence and Recovery

`mj simple` is safe to run repeatedly in the same folder: it reuses the remembered workspace and an eligible simple session, or creates a new isolated session without changing the source checkout. It does not rewrite target or review configuration. If a launch fails before registration, the existing dashboard failure notice and retry path remain responsible; running `mj simple` again is safe. No database migration is required.

For local development, always use a named instance such as `--instance simple-check`. Do not point a test build at the default live instance or its data. Temporary test directories created by the existing test harness remain isolated under its own `MJ_CONFIG_DIR` and `MJ_DATA_DIR`.

## Artifacts and Notes

The key invariants are visible in code:

    GoMode {
        simple: true,
        recipe: Some(GoRecipe {
            target_id: "localhost",
            project_directory: Some(current_directory),
            create_managed_worktree: Some(true),
            ..
        }),
        ..
    }

    DashboardAction::CreateSession {
        review: Some(SessionReview::Off),
        ..
    }

    simple_keybinds().labels(KeyAction::Detach) == ["ctrl+q"]
    simple_keybinds().resolve_direct(ctrl_b) == None

The implementation should keep those values centralized rather than rechecking and mutating independent flags at multiple call sites.

Final evidence:

    config::keys::tests::simple_mode_commands_are_direct_and_never_bind_ctrl_b ... ok
    keybinds::tests::simple_mode_has_no_prefix_and_leaves_ctrl_b_to_the_focused_surface ... ok
    go::tests::simple_mode_launches_the_fixed_local_recipe_with_review_off_and_no_wizard ... ok
    simple_mode_detaches_on_direct_ctrl_q_and_never_claims_ctrl_b ... ok

    cargo test -p brokk-mj-core -p brokk-mj-tui -p brokk-mjolnir
    all suites passed

    cargo clippy --all-targets -- -D warnings
    Finished dev profile

## Interfaces and Dependencies

In `mj-cli/src/simple.rs`, define:

    #[derive(Debug, clap::Args)]
    pub(crate) struct SimpleArgs {
        pub folder: Option<std::path::PathBuf>,
    }

    pub(crate) fn prepare(args: SimpleArgs) -> anyhow::Result<mj_tui::GoMode>;

In `mj-tui/src/go.rs`, `GoMode` gains:

    pub simple: bool

`DashboardState` gains a private `simple_mode: bool`, a public accessor:

    pub fn simple_mode(&self) -> bool;

and its `begin_go` method treats a marked mode as the fixed Simple startup workflow.

In `mj-tui/src/lib.rs`, `DashboardAction::CreateSession` gains:

    review: Option<mj_core::config::SessionReview>

In `mj-core/src/config/keys.rs`, expose:

    pub fn simple_keybinds() -> Keybinds;

The function builds a default `KeysConfig`, replaces every default-bound action with a direct Ctrl chord, leaves the now-unused prefix value outside the command map, and resolves it. `DashboardState::route_bound_key` bypasses prefix arming entirely while Simple mode is active. No new external dependency is needed.

Plan revision 2026-10-20T07:19:47Z: Updated progress and outcomes from design to completed validation after the user clarified that Simple mode must remove prefix behavior completely, and after adding the real-PTY regression and final full validation.
