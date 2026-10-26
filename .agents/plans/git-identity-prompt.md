# Offer to set up the host Git identity instead of failing launches

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

A user whose computer has no `~/.gitconfig` could not start any container session. The launch failed with `read controller Git configuration failed with status exit status: 128: fatal: unable to read config file '/home/<user>/.gitconfig'`, and the TUI told them to resolve the problem without saying how. After this change, a missing global Git configuration counts as "no settings to inherit", so launches succeed. When the host's Git has no commit author (`user.name` and `user.email`), the TUI and the web viewer ask for a name and email the first time the user starts a new session, then write them with `git config --global` on the host. Every session started on the host or in a container then inherits a working commit identity.

To see it working, start Mjolnir in an isolated instance with `HOME` pointed at an empty directory, then press the new-session key. A "Set up Git identity" dialog appears. Save a name and email, and `git config --global --list` in that home shows them. A container session started afterwards can commit.

## Progress

- [x] (2026-10-26) Treat a missing global Git configuration as empty in `read_global_git_settings` (commit 2472c35c5).
- [x] (2026-10-26) Add `GitIdentity`, `controller_git_identity` and `set_controller_git_identity` in `mj-controller/src/controller/provisioning.rs`, tested against real git in an empty home.
- [x] (2026-10-26) TUI: check the identity in the background before the first new session of a run, then offer the dialog (`mj-tui/src/dialogs/git_identity.rs`).
- [x] (2026-10-26) Web viewer: `GET`/`POST /api/git-identity` and the dialog when the new-session form opens.
- [x] (2026-10-26) Full validation and commit.

## Surprises & Discoveries

- Observation: `git config --global --list` exits 128 when neither `~/.gitconfig` nor `$XDG_CONFIG_HOME/git/config` exists, but `git config --global --get-regexp .` exits 1 ("no match") in the same state.
  Evidence: `env -i PATH=$PATH HOME=/tmp/empty git config --global --list` prints `fatal: unable to read config file` and returns 128; the `--get-regexp` form returns 1 with no output.
- Observation: The reader treated "git is not installed" as "no settings". Reused for the identity, that would offer a dialog whose Save can only fail. The first-run PTY test, whose PATH has no git, exposed it.
  Evidence: `first_startup_discovers_both_agents_and_remains_usable_during_background_checks` showed the dialog instead of the wizard. The identity read now reports a missing git as an error, which the TUI shows as a notice before continuing.
- Observation: The terminal test fixtures isolated `HOME` but not Git's global configuration, so the new prompt covered their screens. `AgentIsolation` in `mj-cli/tests/common/mod.rs` now writes a test identity to the fixture home and points `GIT_CONFIG_GLOBAL` at it.

## Decision Log

- Decision: Write the identity to the host's global Git configuration, not to Mjolnir's own config.
  Rationale: The user chose this. One identity then serves the host and every session, and git itself remains the single owner of the fact, so no copy can go stale.
  Date/Author: 2026-10-26 / user and agent.
- Decision: The TUI calls the controller helpers directly from supervised background work instead of through a new daemon protocol action.
  Rationale: The TUI already runs host-side Git work locally (see `start_create_session_preflight` in `mj-cli/src/dashboard/actions.rs`), and it always talks to a daemon on the same host. The web viewer can be remote, so it goes through the daemon HTTP API.
  Date/Author: 2026-10-26 / agent.
- Decision: The web viewer writes through a dedicated `POST /api/git-identity`, not a `set-git-identity` action.
  Rationale: `/api/actions` answers 202 before the work runs, so it cannot return the saved identity or an inline error. The write takes upgrade admission with `enter_unless_draining("viewer Git identity")`.
  Date/Author: 2026-10-26 / agent.
- Decision: Ask when a new session starts, at most once per TUI run or page load, and let the user skip.
  Rationale: Launching without an identity still works and only commits fail, so the prompt must not block a launch. Asking again on every new session after a skip would nag.
  Date/Author: 2026-10-26 / agent.

## Outcomes & Retrospective

Launches no longer fail without `~/.gitconfig`. The TUI and the web viewer offer to set the commit author the first time a new session starts, and they save it with `git config --global`. One gap remains: a check result that arrives while another TUI dialog is open drops the held-back new session, so the user presses new again. The web confirmation goes only to the screen-reader live region, because the viewer has no visible toast.

## Context and Orientation

`mj-controller/src/controller/provisioning.rs` installs a small allow-list of Git settings (`INHERITED_GIT_SETTINGS`, including `user.name` and `user.email`) into container sessions through `install_inherited_git_settings`, which reads them with `controller_git_settings`. Bare sessions on the local host use the host's Git configuration directly. The TUI lives in `mj-tui` (state and dialogs) and `mj-cli/src/dashboard` (background I/O and the dispatch of `DashboardAction`s). The web viewer is plain JavaScript in `mj-controller/src/web/viewer.js`, served by the routes in `mj-controller/src/server/routes.rs`. Its writes go through `POST /api/actions` (`ControllerAction` in `mj-controller/src/server/actions.rs`).

## Plan of Work

For the TUI, `DashboardState::begin_new` in `mj-tui/src/wizards/dashboard/begin.rs` returns an action that checks the identity in the background the first time in a run. The result either continues into the wizard or opens `mj-tui/src/dialogs/git_identity.rs`. Save runs `set_controller_git_identity` in the background. Skip records the choice for the run and continues. For the web viewer, a read route and a `POST` on the same path wrap the same helpers in `spawn_blocking`, and `viewer.js` opens a dialog with the same choices when the new-session form appears.

## Concrete Steps

From the repository root, run `cargo test -p brokk-mj-controller --lib global_git_settings`, the TUI and server tests named in the commit, then `cargo clippy --all-targets -- -D warnings`.

## Validation and Acceptance

The provisioning test `global_git_settings_read_a_missing_global_file_as_no_settings` runs real git with an empty `HOME`. It checks that reading gives no settings, that writing the identity creates the file, and that reading it back returns the saved name and email. To check by hand, run an isolated instance (`--instance git-identity-test`) with `HOME` set to an empty directory, and confirm that the dialog appears, that Save writes `~/.gitconfig`, and that Skip continues to the wizard.

## Idempotence and Recovery

`git config --global --replace-all` is idempotent; saving again overwrites the same two keys. Nothing else in the user's Git configuration is touched.

## Interfaces and Dependencies

In `mj_controller::controller`:

    pub struct GitIdentity { pub name: Option<String>, pub email: Option<String> }
    impl GitIdentity { pub fn is_complete(&self) -> bool }
    pub fn controller_git_identity() -> anyhow::Result<GitIdentity>
    pub fn set_controller_git_identity(name: &str, email: &str) -> anyhow::Result<GitIdentity>

Both functions run git and block, so call them only from background work.
