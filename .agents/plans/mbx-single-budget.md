# Upgrade mbx and expose one shared storage budget

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation proceeds.

## Purpose / Big Picture

Upgrade the host and Mjolnir's verified container binary from mbx 1.16.0 to 1.21.0. Users can set one total storage budget in machine setup. Legacy Mjolnir budgets are ignored rather than translated; users must set their desired budget again. The host requested for this task uses `2TB` (2,000,000,000,000 bytes).

## Progress

- [x] (2026-09-30) Read repository instructions and verified latest release metadata, tagged configuration source, and SHA256SUMS.
- [x] (2026-09-30) Upgraded local binary and set the shared 2 TB budget; Cargo integration is current and mbx doctor reports zero failures or warnings.
- [x] (2026-09-30) Updated Mjolnir's pin, configuration, policy writer, and setup UI. Added regression coverage for discarded legacy values and fresh shared defaults.
- [x] (2026-09-30) Validated behavior, reviewed the diff, and passed the full dev-profile `cargo test`, `cargo clippy --all-targets -- -D warnings`, formatting, and whitespace checks.
- [x] (2026-09-30) Prepared the validated change set for commit on `master` and the user-authorized push to `origin/master`.

## Surprises & Discoveries

mbx retains `gc.max_total_size`; version 1.21.0 uses that total in place of implicit action, worktree, incremental, and learned-incremental caps. Explicit component caps still override this behavior. The existing host configuration has three explicit component limits, so those must be removed to get a single budget.

Validation found that setup help is limited to two rendered rows, and that appending a separately serialized TOML table to a parent table misplaces nested values. Shortened the help and serialized the entire configuration fixture together. Both regressions pass in the final full suite.

## Decision Log

- Decision: Name Mjolnir's new field `max_total_size`, matching mbx. Accept and discard old `max_size` and `target_max_size` values even when malformed. Preserve the enabled switch and directory.
  Rationale: The user explicitly requested ignoring old values and having users configure the new budget again. Preserving host placement avoids changing build layouts.
  Date/Author: 2026-09-30, Codex.
- Decision: Keep the existing automatic total calculation for unmanaged hosts, but change its persisted marker so old managed defaults are not reused.
  Rationale: This resets old budgets without migrating them and still provides a safe default until setup is repeated.
  Date/Author: 2026-09-30, Codex.

## Outcomes & Retrospective

The host upgrade and Mjolnir implementation are complete, and all required validation passes. The host's `mbx doctor --json` reports zero failures and warnings, including the accepted 1.8 TiB display of the requested SI 2 TB budget. The full dev-profile suite reports 5,563 passed tests and zero failures, including the legacy TOML adoption regression, setup help layout, and 2,064 controller tests. No store or worker protocol changes were needed. The change set is ready for delivery on `master` to `origin/master`.

## Context and Orientation

`mj-core/src/config/targets.rs` defines `TargetBuildCache`, the optional machine policy. `mj-controller/src/controller/mbx.rs` probes hosts and chooses a policy; its `configuration.rs` submodule publishes the shared TOML file atomically. `mj-core/src/state.rs` defines the preview shown by `mj-tui/src/setup.rs` and its schema. Existing sessions retain the deprecated `SessionBuildCache.max_size` field for persisted-record compatibility, but current policy does not use it. Native mbx configurations are read-only to Mjolnir and continue to be mirrored into containers verbatim.

## Plan of Work

First install the checksum-verified 1.21.0 executable locally using atomic replacement, preserve the existing cache and target paths, remove component budget keys, and set `gc.max_total_size = "2TB"`. Refresh plain Cargo integration with mbx's setup command if necessary.

Then replace machine `max_size` and `target_max_size` with `max_total_size`. The deserializer must explicitly consume and ignore the two old fields without accepting unrelated typos. Simplify managed TOML to `[gc] max_total_size`, remove the independent worktree preview and scaled-budget calculation, and expose a single GB editor in setup. Update the two architecture checksums with the release's published digests. Update behavior tests to exercise the new field and add tests showing ignored old values, preserved placement, and one total without component limits.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir`, run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. All Cargo tests run outside the restricted sandbox, using their existing isolated data directories. Do not change Cargo target placement. Review `git diff`, stage only task-owned files, commit on the current branch, and push to its configured `origin/master` upstream, as requested by the user.

## Validation and Acceptance

`mbx --version` must print `mbx 1.21.0`, setup status must show current Cargo integration, and `mbx doctor --json` must accept the 2 TB configuration. Machine setup must show one total budget row whose value saves as `max_total_size`. Loading a configuration with old budget fields must succeed and leave the new budget unset; setting a new total must survive serialization and write a shared mbx policy without component caps. Cargo test and clippy must pass in the dev profile.

## Idempotence and Recovery

Retain backups of the local executable and configuration in `/tmp/mj-mbx-upgrade` during the task. Atomic binary/config replacement avoids truncating running consumers. Never delete cache contents or relocate build outputs. Existing concurrent working-tree changes remain outside the commit.

## Artifacts and Notes

Verified release: `https://github.com/jdx/mr-boxington/releases/tag/v1.21.0`. The x86_64 musl archive digest is `5225a3b77f90e1cd3d1ef0d1054ccd4593ae19b2c98b815af1b82a4dfed0f97b`; aarch64 musl is `107f86955f8323ea96ca90ca2d06d49b9b1b99789dd8007f71f9c59bb17bb6d7`.

## Interfaces and Dependencies

`TargetBuildCache` retains `enabled: Option<bool>` and `directory: Option<PathBuf>` and gains `max_total_size: Option<String>`. `BuildCachePreview` exposes a single budget of type `Option<BuildCacheLimit>`. Keep the shared subprocess and atomic policy publication helpers. No dependency, store schema, worker protocol, or new crate changes are needed.

Revision note: Created after verifying the upstream total-budget semantics and mapping all affected configuration and UI paths.

Revision note: Recorded the completed host upgrade and implementation, including positive host doctor results; repository checks remain pending.

Revision note: Recorded successful final validation, the repaired validation issues, and the user's explicit authorization to push when complete.
