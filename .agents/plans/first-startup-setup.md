# Integrate setup into first startup

This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective through completion.

## Purpose / Big Picture

People who already use Codex or Claude Code should run `mj` and find their accounts ready without reading setup instructions. The first interactive launch automatically discovers installed agents and the current GitHub repository, saves additions, and shows one compact welcome dialog. Doctor errors appear there, or as notices if the dialog has been dismissed. `mj setup` reruns the same discovery without questions. Container image downloads remain the daemon's existing background work. AWS and SSH are configured in Settings.

## Progress

- [x] (2026-09-30) Pull origin/master in the user-selected mjolnir2 worktree; identify and preserve unrelated Move edits.
- [x] (2026-09-30) Implement shared, serialized additive setup and instance completion state.
- [x] (2026-09-30) Integrate supervised background first-run work and a dismissible welcome dialog.
- [x] (2026-09-30) Update CLI behavior, documentation, and behavior tests.
- [x] (2026-09-30) Pass Clippy, Rust formatting, documentation checks and build, and focused controller/TUI/CLI tests.
- [x] (2026-09-30) Pass full workspace tests, including the corrected isolated first-run terminal regression and existing startup/upgrade regressions.
- [x] (2026-09-30) Commit implementation as `0037300c` and push it to origin/master.

## Surprises & Discoveries

The daemon already begins image refresh two seconds after startup, with shared pull coordination and workspace notices. The blocking download belongs to the old setup smoke test. Runtime configuration supplies implicit local targets, so eligibility must inspect stored configuration before adding them. The worktree has unrelated edits in `mj-controller/src/controller/move_session.rs` and `mj-controller/src/daemon/session_move.rs`.

The first terminal regression exposed that ordinary doctor notices could be immediately replaced by routine dashboard notices after welcome dismissal. Late doctor results now use failure notices, and the remediation precedes long diagnostic details so it remains visible in the one-line footer. The original failing assertion waited for `mj login` while the footer displayed profile diagnostic context instead.

## Decision Log

Use installed-agent and repository discovery only for setup; remove remote prompts and container smoke tests. The user selected AWS/SSH in Settings, existing background downloads, and a one-time dialog. Date: 2026-09-30.

Keep setup completion in the instance data directory, separate from user settings. Serialize automatic and explicit setup with one cross-process lock and recheck eligibility while holding it. Record an in-progress state before configuration persistence so interruption between saving configuration and marking completion can recover through additive reconciliation. Date: 2026-09-30.

Automatic setup applies to a bare interactive dashboard launch. Explicit `mj go`, workspace management, and upgrade-resume retain their own flows. Welcome waits behind an already-open modal instead of replacing a Settings draft, and its results follow it through the Help dialog. Date: 2026-09-30.

Doctor checks use the existing cancellable background executor with a thirty-second deadline and no smoke tests. Show only fixable errors; optional runtime warnings and successful checks add no welcome noise. Background configuration writes merge against the latest file under its existing lock, and the daemon's configuration feed remains the dashboard authority. Date: 2026-09-30.

## Outcomes & Retrospective

Implementation, validation, and delivery are complete. The shell prompt wizard and Codex-only startup initializer are removed. Both automatic startup and explicit reruns share durable, additive discovery; terminal input remains responsive while discovery and doctor run. The full workspace suite, Clippy, formatting, and documentation validation passed. The isolated terminal regression proves both agents are saved, welcome can be dismissed during background checks, login remedies remain visible afterward, no session is automatically created, and the creation wizard receives discovered profiles through the daemon feed. Implementation commit `0037300c` was pushed to origin/master. No database or configuration schema migration was required. The unrelated Move edits remain uncommitted and untouched.

## Context and Orientation

`mj-controller/src/setup.rs` retains discovery helpers used by Settings and imports. Its new `setup/startup.rs` owns the shared setup transaction and `setup/startup/tests.rs` proves additive discovery, serialization, and recovery. `mj-cli/src/dashboard/io/spawn.rs` supervises first-run work and its events are applied in `mj-cli/src/dashboard/io.rs`. `mj-tui/src/welcome.rs` owns welcome input, rendering, and late failure notices. The daemon is the background process that owns sessions and refreshes configuration from disk; its runtime feed supplies dashboard configuration. Keep that feed as the authority rather than installing a discovered configuration in the UI directly.

## Plan of Work

First replace the Codex-only initializer and prompt wizard with a shared automatic setup service. Reuse discovered harness homes, authentication probes, GitHub-origin parsing, existing configuration update locks, and additive reconciliation. Keep Settings detection helpers and `mj setup instructions`. A setup report names detected profiles and the repository; doctor checks filter to actionable errors for welcome output. Preserve configured installations and deduplicate reruns under the setup lock.

Next start first-run discovery after the terminal and supervised I/O channel are ready. Announce accepted first-run work immediately, save profiles in the background, and check prerequisites without smoke tests. Add a Welcome modal through the shared dialog component with Continue/Enter/Escape dismissal and responsive global quit. Results update an open welcome or appear in notices afterward. Do not reopen it or automatically create sessions. Keep daemon upgrade/resume behavior intact.

Finally replace obsolete prompt tests with behavior regressions, update README and quickstart/reference descriptions, and validate. All test binaries must use named isolated instances or their existing isolated configuration/data directories. Do not modify Cargo target placement or mbx configuration.

## Concrete Steps

Working directory: `/home/jonathan/Projects/mjolnir2`. Use `cargo test` and `cargo clippy --all-targets -- -D warnings` in the dev profile with elevated permissions as required by AGENTS.md. Use focused crate tests while implementing, then the workspace checks. Run the built `mj` only with `--instance first-startup-test` and isolated configuration/data homes for manual or PTY checks. Stage only task files, commit on hel2, and push `HEAD:master` to origin.

## Validation and Acceptance

Prove Codex-only, Claude-only, both agents, custom homes, missing authentication, repository/no-repository, duplicate-free reruns, existing configuration preservation, simultaneous launches, and interruption recovery. Prove welcome input/rendering, late doctor errors after dismissal, no automatic session, and responsiveness during slow probes. Retain the existing background image download tests and isolated upgrade regressions. A fresh interactive instance shows one welcome and discovered profiles; reopening it shows the dashboard, and `mj setup` adds newly discovered profiles without questions.

## Idempotence and Recovery

The setup lock owns eligibility and persistence. Configuration updates merge into the latest file without overwriting identifiers. Completion is marked only after saving succeeds; in-progress state permits retry after interruption. Existing installations without first-run state are skipped when stored profiles, bundles, or targets exist. No database or configuration schema migration is needed.

## Interfaces and Dependencies

Expose a shared setup runner with automatic/forced modes, cancellable discovery, an accepted-start callback, and a compact report. Use existing subprocess executors and standard file locking; do not add a crate. Dashboard I/O carries startup, configuration completion, and doctor completion events. The TUI receives presentation strings and a welcome modal, without a dependency on controller internals.

## Artifacts and Notes

Initial pull: `git pull origin master` reported Already up to date. Required outcome is a validated commit pushed to origin/master; unrelated Move changes remain unstaged.

Validation evidence: `cargo test` exited zero for the full default workspace, including 2,058 controller tests, 908 TUI tests, 253 CLI unit tests, and 12 terminal regressions. The explicit CLI setup-rerun regression also passed. `cargo clippy --all-targets -- -D warnings` finished successfully in the dev profile, `cargo fmt --all -- --check` and `git diff --check` passed, and the docs checks reported zero errors and warnings. The docs build generated 26 pages and checked 2,152 internal links. Test logs are `/tmp/mj-first-startup-full-tests.log` and `/tmp/mj-first-startup-tests.log`; Cargo uses the existing mbx-managed target location.

Revision note (2026-09-30): Record completed implementation, the terminal-test notice finding and correction, and validation already completed. Leave full workspace tests and publication pending until their results are known.

Revision note (2026-09-30): Record passing full workspace tests and the corrected terminal acceptance check. Keep commit and push pending until Git confirms delivery.

Revision note (2026-09-30): Git confirmed `8119fe8f..0037300c HEAD -> master`. Mark delivery complete and preserve the validation record in this plan.
