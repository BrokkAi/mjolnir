# Add globally enabled agent profiles

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan is maintained in accordance with `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

People with several configured coding-agent accounts need to keep an account definition without allowing Mjolnir to choose or probe it. After this change, every profile has an Enabled checkbox in Setup. Turning it off keeps the definition and existing running sessions, while new work, quota displays, utility-model selection, login, review, and import use enabled profiles only. Setup also calls these entries Agent Profiles and uses title case for its top-level sections.

## Progress

- [x] (2026-09-10 14:20Z) Inspected the profile schema, Setup renderer, session selectors, quota refreshers, utility-model resolver, web projection, and authoritative controller launch paths.
- [x] (2026-09-10 14:31Z) Added the backward-compatible profile field, version-8 migration, and shared enabled-profile accessors.
- [x] (2026-09-10 14:39Z) Added the Setup checkbox, title-cased section copy, reference cleanup, and notification.
- [x] (2026-09-10 14:48Z) Applied enabled-profile filtering and authoritative rejection across new-work entry points, quota, utility selection, review, login, import, the web projection, and diagnostics.
- [x] (2026-09-10 14:55Z) Added behavior tests, configuration documentation, and a regenerated Setup screenshot.
- [x] (2026-09-10 15:06Z) Formatting, the repeated full default-member test suite, and Clippy with warnings denied passed. The validated change is ready for its required current-branch commit.

## Surprises & Discoveries

- Observation: The web viewer uses one `profiles` projection for session selectors and the Quota page.
  Evidence: `ViewerSnapshot::from_config_state` builds `profiles`, and `mj-controller/src/web/viewer.js` consumes the same array in both flows. Filtering the projection therefore enforces global availability without adding a browser-only field.

- Observation: TUI quota selection and profile renaming are indexed against the complete profile map.
  Evidence: `mj-tui/src/render.rs`, `mj-tui/src/lib.rs`, and `mj-tui/src/dialogs.rs` use `config.profiles.len()` and `keys().nth(quota_index)`. All of these must share the enabled ordering to avoid selecting a hidden row.

- Observation: The optional desktop workspace member cannot compile on this host because the GTK/WebKit development packages are absent.
  Evidence: `cargo check --workspace --all-targets` stopped in the `glib-sys`, `gobject-sys`, `gdk-sys`, `javascriptcore-rs-sys`, and related build scripts at missing `pkg-config` entries. The workspace manifest intentionally excludes `mj-desktop` from `default-members`; the required default-member checks pass.

- Observation: The first full test run crossed the one-second wall-clock boundary in `server::tests::a_quota_reads_stale_only_once_its_next_refresh_is_overdue`.
  Evidence: That test failed only its exact-boundary assertion, passed immediately in isolation, and passed in the repeated full suite. It predates and does not exercise the changed enabled-profile code.

## Decision Log

- Decision: A disabled profile is globally unavailable for new work, login, import, review, quota refresh, and utility-model inference, but an already running session may continue using it.
  Rationale: This is the behavior selected by the user after repository exploration.
  Date/Author: 2026-09-10 / Codex

- Decision: Disabling a profile in Setup clears matching startup and review references, turns off automatic review, preserves review model and effort, and displays a notice.
  Rationale: The user requested automatic cleanup with notification; preserving model and effort matches the current review editor when its profile is cleared.
  Date/Author: 2026-09-10 / Codex

- Decision: Configuration version 8 introduces `profiles.<id>.enabled`; omitted values mean enabled and are omitted again when saved.
  Rationale: Versioning prevents older builds from silently misreading a disabled profile while keeping all prior configurations enabled by default.
  Date/Author: 2026-09-10 / Codex

## Outcomes & Retrospective

Agent profiles now default to enabled and can be turned off without deleting their configuration. Setup shows the Enabled checkbox, uses the requested section names, and safely clears Startup and Code Review references when a referenced profile is disabled. Every selector and authoritative launch boundary for new work excludes or rejects disabled profiles; Quota and utility-model workers also remove them, while existing running sessions remain projected and operational.

The version-7 migration, explicit false serialization, reference validation, Setup transition, registration guard, login choice, quota request builder, utility resolver, doctor output, second-opinion picker, and browser snapshot have focused coverage. The regenerated Setup SVG carries the current labels. `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `git diff --check` all pass for the default workspace members.

## Context and Orientation

`src/hel_config.rs` defines the versioned TOML configuration and `HarnessProfile`. `mj-tui/src/setup.rs` and `mj-tui/src/setup/schema.rs` implement the Setup draft and rendering. New, resume, and move selectors live in `mj-tui/src/wizards.rs` and `mj-tui/src/wizards/dashboard.rs`; the controller validates those operations under `mj-controller/src/hel_controller/`. Quota requests are built in `mj-cli/src/pollers.rs`, utility candidates in `mj-controller/src/hel_utility_llm.rs`, and the browser receives profiles from `ViewerSnapshot::from_config_state` in `mj-controller/src/hel_server.rs`.

An enabled profile is one whose new boolean field is true. Configured-profile lookups remain appropriate for an existing session whose durable record already names a profile. Enabled-profile lookups are required wherever Mjolnir is about to choose, probe, authenticate, import through, review with, or provision a profile.

## Plan of Work

Add `enabled` to `HarnessProfile` with Serde's default-true and skip-when-true behavior. Bump `CONFIG_VERSION` to 8, update the legacy upgrade range, and expose iterators/lookups for enabled profiles. Startup and review validation will reject a disabled referenced profile, while Setup will clear such references before producing a configuration.

Extend the Setup schema so new and existing profiles show Enabled as checked by default. Render that boolean as a checkbox-like value on the profile page and hook its transition to false to clear `startup.profile`, clear `review.profile`, disable automatic review, invalidate review discovery state, and show a concise notice. Change top-level section labels and related Setup copy to Agent Profiles and title case.

Replace complete-map iteration with enabled-profile iteration in every new-work selector and request builder. Add controller-side enabled checks for session registration, resume, move, reviewer staging/discovery, import, and login so stale clients cannot bypass the UI. Filter the web projection and TUI quota table, use enabled counts for focus/layout, prune hidden quota state, and filter utility candidates and caches. Keep existing-session worker, credential-sync, checkpoint, and dictation lookups against configured profiles.

Update profile documentation and examples. Add focused behavior tests close to the affected modules, then run the repository's required full checks.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`.

Edit the configuration, Setup, controller, TUI, CLI, and documentation modules described above. Update all `HarnessProfile` literals to set `enabled: true` unless a test intentionally creates a disabled profile.

Run:

    cargo fmt --check
    cargo test
    cargo clippy --all-targets -- -D warnings

The final two commands must run outside the restricted sandbox because the suite uses loopback TCP and Unix sockets. All commands must exit zero. Then inspect `git diff`, stage only task-owned files, and commit directly to the current branch.

## Validation and Acceptance

A version-7 profile without `enabled` loads enabled and saves under version 8 without an explicit true field. A profile saved off contains `enabled = false`. Setup displays Agent Profiles and title-cased top-level sections; a new profile is checked. Disabling a referenced profile clears the startup and review references, disables automatic review, and names those changes in the notice.

Disabled profiles do not appear in terminal/browser new, resume, move, review, import, or quota choices. Direct controller, import, review-discovery, and login requests reject them. Quota and utility workers do not probe them, stale cached state is removed, and doctor reports that their checks were skipped. An existing running session whose durable record names a disabled profile remains valid and continues normal polling and credential synchronization.

## Idempotence and Recovery

All edits are ordinary source changes and can be reapplied safely. The configuration migration is in-memory and takes effect on the next ordinary save; it does not rewrite files merely by loading them. If validation fails, correct the source and rerun the same commands. Do not reset or remove unrelated working-tree files.

## Artifacts and Notes

The worktree already contained `.agents/plans/restore-tui-workspaces-and-status.md` and `mj.sqlite3` before this task. They are unrelated and must remain untracked and uncommitted.

## Interfaces and Dependencies

`HarnessProfile` gains public `enabled: bool`. `HelConfig` gains an enabled-profile iterator returning profile IDs and references, plus an enabled-profile lookup by ID. The TOML interface gains optional `profiles.<id>.enabled`, default true. The browser wire schema does not gain a field; disabled profiles are absent from `ViewerSnapshot.profiles`. No new dependency is required.

Revision note (September 10, implementation): Marked every milestone complete, recorded the chat second-opinion path found during the final audit, added validation evidence, and documented the optional desktop system-package limit and the reproduced clock-edge test flake.
