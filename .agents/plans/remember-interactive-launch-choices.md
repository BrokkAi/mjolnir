# Remember choices only from interactive launches

This ExecPlan follows `.agents/PLANS.md` and remains a living document.

## Purpose / Big Picture

Unattended `mj new`, ACP, HTTP API and child sessions must leave saved defaults unchanged. The terminal dashboard, `mj go`, `mj simple`, and web/phone new-session forms may remember choices. A later wizard should preselect the latest interactive choice even after unattended sessions are created.

## Progress

- [x] (2026-10-23) Trace launch registration, viewer/API routing and preference persistence.
- [x] (2026-10-23) Add the interactive request flag and gate default writes.
- [x] (2026-10-23) Follow revised design: store latest interactive choices in go.json rather than session rows; publish them through runtime metadata.
- [x] (2026-10-23) Add isolated create, persisted preference and wizard golden regression coverage.
- [x] (2026-10-23) Workspace clippy passes with warnings denied; formatting and diff checks pass.
- [x] (2026-10-24) Run all five affected suites. Corrected the new isolated regression fixture and reran it successfully; shell/workspace-close failures pass isolated reruns. Two unrelated PTY failures remain documented in the report directory.

## Surprises & Discoveries

The phone and web viewer use the same action handler. Attached-mount history supplies suggestions, but bare-project directory history preselects a path, so new-session provisioning must gate that history write too. Resume retains its existing directory-history behavior. The revised go.json design requires no database migration, session-record field or schema bump.

## Decision Log

The parent revised the design to retain full session recency for project ordering and to persist interactive preselection independently in go.json. An optional `recent` entry contains profile, target, bundle and optional bare-project directory; version 2 remains readable because serde ignores unknown fields and the new entry defaults to absent. The viewer handler sets an internal skipped-serde action field, while the public HTTP create route leaves it false. Daemon protocol advances from 60 to 61 for the request and runtime metadata changes.

The wizard falls back to its existing newest-configured-session behavior while `recent` is absent, preserving upgrade behavior. Once an interactive choice is saved, unattended creates cannot replace it. The daemon caches the saved recent choice in a watch channel. Creation and the existing background configuration refresh serialize their file/cache updates with a separate mutex; feed readers only clone the channel value, never wait for disk I/O. The remote poller carries the daemon host's value to terminal clients.

## Outcomes & Retrospective

Implementation and required validation runs are complete. Workspace clippy, formatting, diff checks, the extended wizard golden and the corrected isolated create regression pass. The full run passes core/client/TUI and CLI unit/startup suites; two unrelated PTY failures reproduce and remain documented for parent acceptance. Migration and session-record edits from the original design have been removed. No commit is made; the parent reviews and commits.

## Context and Orientation

`mj-client/src/daemon.rs` defines the daemon create request. `mj-controller/src/daemon/create.rs` registers it through `SessionLaunchOptions` in `controller.rs`, then updates go preferences only for interactive launches. `database/sessions.rs` gates the stored subagent preference and container-size update in the session transaction. `controller/provisioning.rs` gates bare-project history for new creates. `mj-core/src/go.rs` owns the versioned preference file and read-modify-write helper.

`mj-client/src/runtime_feed.rs` defines `recent_launch` metadata. `daemon/feed.rs` publishes it, `pollers/remote.rs` carries it, and the CLI installs it in `DashboardState`. `mj-tui/src/wizards/dashboard/begin.rs` validates configured choices before preselection. Existing launch recency and `launch_inputs_changed` retain their original project-ordering behavior.

## Plan of Work

Complete isolated regressions, inspect the golden change, then run affected-crate suites and workspace clippy once. Fix and rerun only failing tests. Review the final diff and leave all files uncommitted.

## Concrete Steps

From the repository root, run `cargo fmt --all -- --check`, then elevated `cargo test -p brokk-mj-core -p brokk-mj-client -p brokk-mj-controller -p brokk-mj-tui -p brokk-mjolnir --no-fail-fast` and `cargo clippy --all-targets -- -D warnings`. Save text logs under `.mj/agents/8ba2b78f388ab1faff2cc274ccc80f09`. Use normal mbx Cargo storage. The existing golden was extended with `MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-tui golden_wizard_target_step`.

## Validation and Acceptance

The isolated `only_interactive_create_remembers_default_recent_and_subagents` test must show an unattended create leaves go.json absent or unchanged and preserves the stored subagent policy, while an interactive create saves default and recent choices and publishes metadata. An existing database regression now covers unattended policy/container-size creation. The wizard golden shows interactive choices overriding newer unattended sessions while project order remains history-based. Existing configuration, protocol and upgrade tests must continue to pass.

## Idempotence and Recovery

All test stores and configuration directories are isolated. No live daemon migration, installation or schema change is required. Tests may be rerun; no Cargo output is redirected.

## Artifacts and Notes

Longer evidence and logs belong in the child report directory named above. The wizard fixture is `mj-tui/tests/golden/wizard-target-step.txt`.

## Interfaces and Dependencies

`CreateSessionRequest` adds a false-default, false-omitting `remember_launch_choices` boolean. `SessionLaunchOptions` and server-owned `ControllerAction::New` carry that choice internally. `GoPreferences` adds optional `recent: RecentLaunch`; `RecentLaunch` contains profile/target/bundle identifiers and optional project directory. Runtime metadata and the remote state update carry this optional value independently of session history. No new crate or dependency is needed.

Revision note: replace the original session-column/migration plan with the parent's revised go.json design; add bare-directory history gating after discovering that it preselects values.

## PTY follow-up investigation

The user requested three individual repetitions of the two PTY failures after both passed on clean base. With NO_COLOR unset, theme passed 3/3; empty-workspace passed 2/3. Removing only recent-directory preselection made empty-workspace pass 3/3 with publication and protocol unchanged. Restoring it and printing the second form plus go.json showed that the correct remembered directory was present, and the fixture appended the same directory. The fix clears the second field with Ctrl-U before entering the test's explicit path; all temporary diagnostics and production experiments are restored. Both final cases pass 3/3. The theme's earlier failure remains a likely load-sensitive terminal-query timing issue, not a reproduced regression. Evidence lives in /tmp/mj-pty-regression-10e6a4981. The user's provisioning and docs changes are preserved; no commit is made.
