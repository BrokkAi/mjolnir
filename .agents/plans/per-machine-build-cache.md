# Make build caching a per-machine setting

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture

Users configure Rust build caching in Machines, with one enable policy per host. The machine's Build cache (mbx) row displays its configured enable state and total budget. Settings search finds each machine's cache page through mbx, cache, or build cache. The obsolete global switch and page disappear without enabling caches users previously disabled.

## Progress

- [x] (2026-09-28) Inspected configuration conversion, comment-preserving saves, controller cache resolution, and Settings navigation/search.
- [x] (2026-09-28) Migrated legacy global opt-outs and removed runtime global policy.
- [x] (2026-09-28) Added machine summaries and search aliases; removed the global page and updated user documentation.
- [x] (2026-09-28) Validated behavior, passed required Cargo and documentation checks, and reviewed the changes for the required current-branch commit.

## Surprises & Discoveries

The configuration writer compares canonical loaded and updated values. Migration-only machine opt-outs can therefore be omitted from a save even as the obsolete global section is removed. Persistence must compare those machines against the raw file during this migration.

Search currently returns only label-prefix matches when any exist. Searching cache finds Cache directory and hides Build cache (mbx); page aliases must participate in that preferred tier.

## Decision Log

- Decision: Each machine alone controls enablement; omitted enable means enabled where supported. Reason: Avoid conflicting switches for the small machine collections users typically have. Date: 2026-09-28.
- Decision: Convert legacy global disabled into explicit local/SSH machine opt-outs, including implicit local, after legacy target conversion. Reason: Preserve effective settings including formerly overridden machine enable values. Newly added machines use defaults. Date: 2026-09-28.
- Decision: Show configured budget or automatic in summaries without probing hosts. Reason: Discovery must never block rendering or imply unverified host compatibility. Date: 2026-09-28.

## Outcomes & Retrospective

Implemented per-machine cache ownership and discovery. Configuration revision 14 preserves legacy global opt-outs, including migration-only saves, and removes the obsolete global page and runtime arguments. Machine summaries and all three search aliases are covered by behavior tests. All required checks pass; the live default instance was not used for validation.

## Context and Orientation

`mj-core/src/config.rs` converts stored configuration into runtime configuration and resolves machine settings into targets. `mj-core/src/config/document.rs` edits saved TOML while preserving comments. Machines and their cache overrides live in `mj-core/src/config/machines.rs`. `mj-controller/src/controller/mbx.rs` resolves supported host caches; its service applies configuration in background tasks. `mj-tui/src/setup.rs` owns Settings drafts and navigation, with labels/defaults in `setup/schema.rs` and the flat search index in `setup/search.rs`. The CLI executes preview actions off the UI loop.

## Plan of Work

First advance config revision 13 to 14. Retain a private deserialize-only legacy global setting. After converting old targets to machine records, apply global false to all existing local/SSH machines and implicit local, retaining their other cache settings. Resolve targets afterward. Remove the global field from runtime Config and all preview, provisioning, doctor, and configuration-service interfaces. Save canonical per-machine settings atomically; adjust the in-place writer to materialize migration changes before dropping the global section.

Next remove the global Settings page and add a shared machine cache summary: Enabled by default, Enabled, or Disabled, followed by an explicit total budget in GB or automatic budget. Display these summaries in browsing and search. Add path-specific aliases mbx, cache, and build cache to the search preferred tier. Existing nested pages, previews, mouse/keyboard behavior, draft retention, and unsupported EC2 behavior remain.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir`. Use existing mbx Cargo storage without changing target directories. Format changed Rust with rustfmt. Run `cargo test` outside the restricted sandbox, then `cargo clippy --all-targets -- -D warnings` in the dev profile. Keep automated tests' isolated directories; any manual binary invocation uses `--instance mbx-machine-settings`. Review `git diff --check` and the final diff; stage only this task's files and commit on the current branch without pushing.

## Validation and Acceptance

Migration tests load older configurations with disabled, enabled, or absent global policy, implicit local, multiple SSH machines, conflicting overrides, and legacy target layouts. Saving without user edits and reloading must retain opt-outs and budgets, drop the obsolete section, preserve unrelated comments, and be stable on a second save. Controller tests prove defaults still provision caches and machine opt-outs remain independent. Settings tests navigate with keyboard and mouse, inspect summaries, and search all three aliases across machines including implicit local. Search activation must preserve unsaved drafts. Existing isolated upgrade regressions must remain passing.

## Idempotence and Recovery

Configuration migration occurs in memory and persists through the existing atomic save; no live configuration is edited for testing. Repeated load/save is stable. Existing provisioned-session cache records, database schema, and worker handoff formats are unchanged. Keep unrelated working-tree changes out of the commit.

## Interfaces and Dependencies

Remove runtime `Config.build_cache` and public `BuildCacheConfig`. Retain only a private legacy reader in the stored configuration. Cache resolution and preview APIs receive the machine/target and executor without a global enable argument. No new dependencies are needed.

## Artifacts and Notes

Initial plan recorded on 2026-09-28 from the approved conversational plan. Validation evidence will be recorded here before completion.

Validation update (2026-09-28): `npm run build` in docs passed, including 2,151 internal links over 26 pages. The initial Cargo attempt identified Config test fixtures that still initialized the removed field; these are updated and the full suite is compiling again.

Final validation (2026-09-28): `cargo test` passed in the dev profile outside the sandbox, including all new migration/controller/UI regressions, historical database migrations, and all 11 isolated daemon-startup tests. `cargo clippy --all-targets -- -D warnings` passed. A focused rerun of `every_setting_description_fits_its_two_rows` passed after changing its fixture to the machine page. Rustfmt checks and `git diff --check` passed. Documentation build and internal link validation passed. Runtime configuration no longer retains any global cache policy; legacy input exists only in the stored configuration reader.

Plan completion note: user documentation was updated alongside the interface removal so configuration examples use revision 14 and both cache guides direct users to Machines and search. No scope changes or unresolved implementation issues remain.
