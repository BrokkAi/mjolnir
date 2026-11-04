# Remember delegation separately for each profile

## Purpose

Creating a session with Mjolnir delegation under one profile must not change the remembered delegation choice for another profile. The terminal and browser new-session wizards should initialize from the selected profile and refresh when that profile changes. Existing Move and Resume behavior continues to use the session's own policy.

## Progress

- [x] Trace the global singleton, state projection, and wizard consumers.
- [x] Choose the storage and compatibility design and divide implementation ownership.
- [x] Implement storage migration, atomic writes, rename handling, and controller projections.
- [x] Implement shared state, wire metadata, and terminal client propagation.
- [x] Implement terminal and browser profile-specific selection and regression coverage.
- [x] Review the integrated diff and complete required validation.
- [x] Prepare the validated change for commit on the current branch and publication to origin/master.
- [x] Validate the upstream merge with migration 101 and protocol 68 for publication.

## Context and orientation

The controller is the background service owning SQLite storage and session registration. `mj-controller/src/database/sessions.rs::save_new_session_to` currently writes one `subagent_preference(singleton, policy)` row in the transaction that creates an interactive session. `state_io.rs` loads that row, while `committed.rs` maintains the published state after subsequent writes. `mj-core/src/state.rs::State` currently exposes one `last_subagent_policy`, propagated through `mj-client/src/runtime_feed.rs::RuntimeMetadata`, controller feed and viewer projections, and the CLI dashboard. The terminal wizard reads it in `mj-tui/src/wizards/dashboard/begin.rs` and `wizards/subagents.rs`; the browser reads it in `mj-controller/src/web/viewer.js`.

Profiles use persistent string IDs, also stored as `SessionRecord.last_profile`. Configuration rename already has a journal and database reference-migration transaction; remembered choice keys belong in that same rename path.

## Decision Log

On 2026-10-27, use `last_subagent_policies: BTreeMap<String, SubagentPolicy>` throughout state and projections. Provide `State::last_subagent_policy_for(profile)` to resolve the profile entry, then the legacy empty-string entry, then the existing Native default. The empty key is reserved for the pre-upgrade global value because that value has no reliable owning profile. Never update it during new session creation.

Replace the singleton table in migration 101 with a table keyed by `profile_id`, copying the old row to the empty key. Classify this migration as breaking and raise the minimum compatible revision because old writers address the removed singleton column. Preserve all shipped migrations, including upstream's structured-prompt migration 100. Keep the session and remembered choice in one transaction. Update incremental database notifications and profile renames with the same ownership.

Accept old scalar snapshot fields through a serde alias and shared policy-map deserializer so saved State data remains readable. Terminal upgrade handoff carries drafts and layouts, not State; its format needs no change. Use daemon protocol 68 because the published preference shape differs from upstream's protocol 67. Remember DSH None and Native now that the profile boundary prevents them from affecting Claude or Codex.

Implementation is delegated in disjoint slices: controller Rust files; core/client/CLI; and terminal UI plus browser JavaScript. The parent owns integration, this plan, validation coordination, and delivery. No branch changes or Cargo target changes are authorized or needed.

The initial implementation used the next available schema/protocol revisions (100/66) and was committed as bf70a80a7. Publication encountered newer upstream work using schema 100 and protocol 67. Preserve those upstream changes and append preference migration 101, composing protocol 68. Session 386351293e1410b9a9d089053453c3b1 owned and published shared Muse fixture/formatting repairs in 4ec10a068; discard this session's duplicate uncommitted repairs and merge their published ancestor normally. Bounded migration, metadata, startup, and wizard checks validate the composed build; prior unrelated successful suites remain valid evidence.

## Milestones and concrete steps

First update persistent ownership. Add migration 101 in `mj-controller/src/database/schema.rs`, increase `SCHEMA_VERSION` in `database.rs`, and make `sessions.rs`, `state_io.rs`, and `committed.rs` write and project a map. Extend database integration tests to create different interactive choices for two profile IDs, reload them, and prove subsequent writes and profile renames preserve both independently. Migration tests must start from older storage in isolated directories and preserve legacy policies.

Next update shared state and the metadata path. Replace scalar fields with maps in core, runtime metadata, controller feed/viewer projections, and CLI dashboard drains. Retain old snapshot decoding through one shared deserializer. Update existing serialization and protocol fixtures. Rename handling must update both durable rows and the in-memory map through the existing reference-refresh mechanism.

Finally update terminal and browser creation flows. Resolve the chosen profile before resolving the initial policy. When the selected profile changes, use the destination's remembered choice while preserving existing within-profile explicit-choice behavior and unavailable-policy handling. Keep Move and Resume on their session-specific policies. Extend existing terminal goldens and profile-selection transition tests, and browser tests covering the same behavior.

## Validation and acceptance

Run commands from the repository root using existing mbx-managed Cargo storage and the dev profile. During implementation, use only focused filters for affected regressions. After final fixes, run full suites for packages `brokk-mj-core`, `brokk-mj-client`, `brokk-mjolnir` (the CLI), `brokk-mj-controller`, and `brokk-mj-tui`, and `cargo clippy --all-targets -- -D warnings` once, with logs outside build storage if needed. They can be split across commands and owners. Run applicable browser tests and formatting checks. On any failure rerun only the failing tests after repair; do not repeat successful full suites. All live binary tests must use a named isolated instance or existing isolated test directories; never upgrade the live/default store.

Acceptance means different choices survive storage reload for two profiles; selecting each profile in either new-session UI restores its own choice; unsupported or unavailable choices retain the established display behavior; old snapshots and migrations remain readable; and profile rename retains its saved preference. Review the diff, stage only changed files, commit on the existing branch, and push `HEAD:master` to origin. If upstream has moved, fetch and merge origin/master, resolve conflicts, and push without rebasing.

## Surprises & Discoveries

The remembered policy is also maintained by the incremental committed-state projection, so changing the loader alone would leave active dashboards stale. DSH's None was deliberately excluded while storage was global; the new ownership makes that exclusion incorrect.

An independent review of the first patches found that legacy scalar policy objects and profile maps must be distinguished by their value shapes, because a real profile can be named `mode`. It also found that profile rename must update the current Controller immediately, beyond updating durable database rows; regression coverage must observe that same instance before reload. Both findings were sent to the owning implementation agents before final validation.

Full suites exposed historical fixtures that lowered the revision of a freshly migrated database without undoing its new preference-table layout. The correct repair is to restore the historical singleton layout in those test fixtures. Production preference migration 101 remains unconditional, since its transaction makes a pre-101 revision with a post-101 table impossible during normal upgrades. Controller migration and CLI startup fixture owners applied that correction and reran only affected regressions.

Final independent review found another browser profile transition: when a live snapshot leaves only one profile, `settleSkippedNewSteps` selects it without a dropdown event. Route that transition through `selectNewProfile` too. The existing browser creation golden now proves both explicit switching and a live alpha/Mjolnir to sole beta/None transition, including the submitted launch action. Both the extended creation golden and existing skipped-steps golden passed against their expected files. Full browser unit tests passed 57 cases and exposed one source-extraction fixture missing the new helper; loading the real helper corrected that fixture, and its focused rerun passed.

## Recovery and idempotence

Use isolated temporary database directories for migration tests. Migration steps run transactionally and use the revision marker to avoid repetition. Never migrate the host's live store for this feature. Preserve unrelated working-tree changes. If validation is interrupted, resume the affected command using existing build storage; do not relocate or delete build outputs.

## Outcomes & Retrospective

Implementation and review are complete across storage, metadata, and both UIs. Each profile owns its remembered policy, renames retain it, and the old singleton remains a fixed starting default for profiles without an entry. Both explicit and automatic browser profile changes use the same selection path. No production migration workaround remains.

Pre-merge validation passed: client 43 unit tests and one process-probe test; core 551 library tests with three ignored, plus three integration tests and one existing ignored doc example; TUI 565 tests with two ignored; CLI 223 unit tests and all applicable integration targets, including the three corrected startup regressions. Controller passed 2,147 tests in its full run plus all 13 corrected historical-fixture regressions (2,160 total), with 17 ignored; the preference migration regression also passed its focused final rerun. Full browser unit validation passed 57 tests initially and the single corrected source-extraction fixture on its focused rerun (58 total). Both the extended creation golden and existing skipped-steps golden passed against checked-in expectations.

Workspace Clippy passed. Because the schema file changed during that compilation, a final scoped Clippy pass for controller and CLI also ran and passed with `--all-targets -- -D warnings` on stable source. Formatting and diff checks passed. Independent final review approved the completed compatibility, migration, and browser-transition changes without remaining findings. Delivery uses the existing branch and the repository's required push to origin/master; no installation or live-store migration was performed.

The final upstream merge preserves peer-owned fixture commit 4ec10a068 and structured-startup migration 100, appending preference migration 101 with protocol 68. Its validation passed: 31 schema regressions, the client policy-map compatibility case, committed preference projections, accepted-policy persistence, same-controller profile rename, and both browser wire integration cases; all 16 isolated daemon-startup tests; the focused TUI profile transition and creation golden. Browser code/tests were unchanged by the additional upstream merge, so the already-passing merged 58-unit and two-golden results remain applicable. A fresh final workspace `cargo clippy --all-targets -- -D warnings`, full formatting check, and independent bounded merge review all passed on the composed source. No shared Muse fixture repair is duplicated in this feature's diff against origin/master.
