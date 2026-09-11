# Preserve working sessions during setup and configuration repair

This ExecPlan follows `.agents/PLANS.md` and must remain current as implementation proceeds.

## Purpose / Big Picture

Running setup must discover new software without removing working configuration. A missing bundle, profile, or target must affect only sessions that reference it. Users must still open Mjolnir and read a diagnostic explaining how to restore the configuration.

## Progress

- [x] (2026-09-11) Created isolated worktree `fix/missing-session-bundle`; restored the local muse-acp bundle from its live container Git origin, backed up config, and verified dashboard startup.
- [x] (2026-09-11) Identified full-config replacement in setup and global foreign-reference validation in controller loading.
- [x] (2026-09-11) Implemented shared additive discovery, explicit CLI target conflict choices, TUI draft alternatives, atomic merging, and save-time protection of active dependencies.
- [x] (2026-09-11) Implemented computed diagnostics, reconnect/launch/checkpoint guards, terminal repair dialog, and web repair guidance.
- [x] (2026-09-11) Added scenario regressions and updated human documentation. Full cargo test passed (3,015 tests); latest setup/doctor checks, clippy, formatting, JavaScript syntax, and diff checks passed.
- [x] (2026-09-11) Finalized the validated change for delivery on `fix/missing-session-bundle`; the implementation commit includes this plan.

## Surprises & Discoveries

Setup explicitly advertised replacing existing configuration. Controller loading then rejected every operation if any active session referenced a removed entry. The local live container still held the original repository, so restoring its bundle required no session or database deletion. Dashboard startup succeeded after repair; the existing worker also reported relay connectivity errors, a separate runtime issue.

## Decision Log

Preserve existing entries and global settings. Discoveries with identical identities reuse them; new repositories and harness homes receive their own identifiers. For conflicting target configurations offer keep (default) or add separately, never silently replace a session dependency. This avoids needing a new persisted configuration snapshot format. Date: 2026-09-11.

Compute configuration diagnostics from the current configuration rather than persisting them as lifecycle errors. Restoring the missing entry clears the diagnostic without rewriting session state. Keep structural validation strict. Date: 2026-09-11.

## Outcomes & Retrospective

Local missing-bundle startup failure is repaired and dashboard startup was verified with the installed CLI. Both setup paths now preserve working entries and discover additions, including harness commands installed before first login. Active session dependencies are protected during settings saves. Configuration drift is isolated to the affected session, with terminal transcript/setup actions and web repair guidance. Full tests and latest focused checks pass. The implementation is finalized on the isolated branch. Malformed config files still fail structural validation, and arbitrary deleted custom entries require restoration rather than guessed substitutes.

## Context and Orientation

`mj-controller/src/hel_setup.rs` discovers installations, asks setup questions, and writes TOML. `src/hel_config.rs` provides `HelConfig::update_to`, which locks the file and reloads it before editing. `src/hel_state.rs` owns persisted session records and reference validation. `mj-controller/src/hel_controller.rs` currently validates all session references during load. `mj-controller/src/hel_controller/backend.rs` prepares session reconnect commands. `mj-tui/src/lib.rs` and `render.rs` own terminal interaction and presentation. `mj-controller/src/hel_server.rs` projects configuration into safe web data; `mj-controller/src/web/viewer.js` displays it.

## Plan of Work

First replace setup's whole-config save with a reviewed list of additions applied through `HelConfig::update_to`. Reuse existing identities, allocate repository names instead of overwriting current-repository, and ask about target conflicts. Detect conflicting concurrent changes instead of overwriting them. Preserve global preferences and disabled profiles.

Then factor per-session configuration validation into `SessionRecord`. Controller loading validates file/state structure but no longer refuses unrelated sessions. Reconnect and worker launch paths reject incomplete configuration with the same actionable diagnostic. Terminal and web views compute that diagnostic without mutating stored lifecycle state. Existing setup controls and instructions to restore the named TOML entry provide repair guidance.

## Concrete Steps

Work only in `/Users/ryansvihla/code/mjolnir/.worktrees/bundle-fix`. Run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Every cargo test runs with elevated permissions because the suite needs sockets. Keep build artifacts in the worktree target directory, not temporary storage. Stage only changed files and commit to `fix/missing-session-bundle`; do not push.

## Validation and Acceptance

Tests must show rerun setup preserves existing bundle/profile/target settings, newly discovered software is added, repeated discovery does not duplicate installations, another repository is added, conflict choices preserve old entries, cancellation writes nothing, and concurrent edits survive or fail explicitly. Tests must show an unrelated active session still loads when another references missing configuration, affected operations explain repair, and restoring configuration removes the diagnostic. Public web diagnostics contain identifiers only, never profile homes or secrets.

## Idempotence and Recovery

All setup changes are additive and atomic. Aborted dialogs leave the file untouched. Conflicting writes fail without partial changes. Local repair backup is `config.toml.before-bundle-repair-20260911` beside the user's config. Never delete sessions or stop their containers to repair configuration.

## Artifacts and Notes

The repaired session is c6af007fd95cc1a9ba121b4a66bc337d and references bundle muse-acp, repository BrokkAi/muse-acp. This private diagnostic context belongs here, not in product documentation.

## Interfaces and Dependencies

Use existing Rust types and config locks; introduce no crate or dependency. A per-session configuration issue helper must be shared by strict validation, operation guards, and UI projections. Keep filesystem work in existing background/setup execution paths; rendering computes only in-memory diagnostics.

Initial plan records agreed behavior and local repair evidence.

The terminal settings editor was discovered to have an independent detection path that skipped name collisions. Reconciliation now lives in `HelConfig::setup_additions` in `src/hel_config.rs`, shared with the CLI. `mj-cli/src/dashboard/io.rs` validates changes through `HelState::validate_setup_update` before saving, preserving active dependencies while allowing additions, restoration of missing entries, global defaults, and profile enablement changes. The repair dialog uses the existing scrollable confirmation framework and offers transcript/setup actions. One move relay test fixture needed its raw checkout path supplied after introducing session-specific reconnect validation. Initial controller/core suites passed; terminal rendering checks exposed and corrected a clipped repair label. Updated 2026-09-11.

Final scenario review found that a fresh CLI installation without its first-login home was invisible to directory-only discovery. `discover_current` now supplements existing homes with bounded version probes using the program names already owned by `hel_credentials::login_command`. Missing-home diagnostics point to login, and discovery never creates directories. Regression coverage proves a fresh Muse installation with an explicit home override is added once without filesystem mutation. Updated 2026-09-11.

Validation completed on 2026-09-11: `cargo test --quiet` passed 3,015 tests with existing opt-in tests skipped; after the final first-login discovery change, `cargo test -p brokk-mj-controller hel_setup --quiet` passed 40 tests and `cargo test -p brokk-mj-controller hel_doctor --quiet` passed 47 tests. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `node --check mj-controller/src/web/viewer.js`, and `git diff --check` passed. CLI poller, gesture, and presentation fixtures now supply their referenced bundles instead of relying on invalid setup; their original behavior assertions pass. No release or push was requested.
