# Consume SessionWiki native harness adapters

This ExecPlan follows `.agents/PLANS.md` and remains a living document.

## Purpose / Big Picture

Mjolnir will index Kimi Code, Grok Build, Muse and DSH native histories through SessionWiki's public adapters, including enabled profile homes. Existing Kimi/Grok directory keys and Muse session.jsonl keys stay stable. DSH becomes searchable; importing DSH for native resume remains unsupported.

## Progress

- [x] (2026-11-10) Inspect adapter registration, import boundaries, isolated index helpers and the normal Cargo/mbx shim.
- [x] (2026-11-10) Create isolated `native-sessionwiki-adapters` branch in session f93bd930627476fc706e19d2fd4efef5's clone.
- [x] (2026-11-10) Replace custom adapters with public SessionWiki adapters and add profile isolation, search and continuation coverage. Formatting and subsequent joint compilation passed.
- [x] (2026-11-10) Validate against the parent SessionWiki checkout: 67 focused integration/import tests pass.
- [x] (2026-11-10) Switch to registry brokk-sessionwiki 0.34.24; full controller run passed 2514 tests with 19 ignored and one daemon timing failure, which passed its focused rerun. Workspace Clippy passed.
- [x] (2026-11-10) Validate the Muse XDG root correction: full checkpoint/controller suites pass (171/2516 passed, 2/19 ignored); final workspace Clippy is clean.
- [x] (2026-11-10) Prepare portable validated changes for the required commit/review gate and authorized branch/PR delivery. The parent confirmed publication and joint validation; Mjolnir installation and release remain outside this task.
- [x] (2026-11-10) Commit cff4a06e6 and request review; confirm the Grok/Muse symlink key defect with seeded legacy index rows against 0.34.24.
- [x] (2026-11-10) Validate registry 0.34.25 with configured-path builders and mixed-writer regression: checkpoint171/controller2516 passed,21 existing ignored; workspace Clippy and fmt pass. Compatibility correction is prepared for the required committed-tree review and authorized branch/PR delivery.

## Surprises & Discoveries

The `cargo` command is `/home/jonathan/.local/share/mbx/bin/cargo`, a shim invoking mbx. No Cargo target overrides are needed. The existing internal fork note incorrectly describes an upstream dependency; Cargo actually links brokk-sessionwiki 0.34.22.

SessionWiki review added a backward-compatible `Adapter::modified_at(key)` hook to separate actual activity dates from opaque change tokens. The two delegating Mjolnir wrappers forward it; Mjolnir's own adapter behavior remains unchanged.

The checkpoint/import Muse root resolver used OS-native dirs defaults, which disagree with Muse's XDG paths on macOS and Windows. `HarnessKind::Muse.nested_data_dir` already owns the correct cross-platform interpretation; delegate to it rather than maintaining a second resolver. The one full-suite failure was an unrelated three-second daemon continuation deadline; its isolated rerun passed in 1.28 seconds.

Committed-tree review found that canonicalizing Grok/Muse session roots changed the former mj adapters' lexical keys under symlinked homes. A seeded legacy Grok row produced three search hits (old alias, new canonical path, second home) instead of two. Kimi had already canonicalized keys and must retain that policy. SessionWiki 0.34.25 adds public `.keep_configured_paths()` builders for Grok and Muse so mj preserves its older keys while standalone defaults preserve 0.34.24's canonical keys. Parsing an explicit stored key preserves its spelling in either policy.

Because standalone and mj sync share one database, selecting different policies must still reuse existing physical sessions. SessionWiki 0.34.25 centralizes identity adoption in its indexer using the canonical store root and relative session suffix. Grok/Muse opt into that behavior with `Adapter::path_key_depth`; the index retains the stored key and ID instead of creating another alias row. The mj regression includes a canonical-default writer pass into its seeded lexical index before testing scoped deletion.

## Decision Log

- Decision: Keep a small tool-name mapping module and Mjolnir's native import parsers; remove only the custom SessionWiki adapter implementations.
  Rationale: Indexing belongs in SessionWiki, while import preserves harness resume details and has a different contract.
  Date/Author: 2026-11-10, integration agent.
- Decision: Use the parent's checkout temporarily, then replace it with the published brokk-sessionwiki requirement before the final commit.
  Rationale: The adapters are developed jointly; a committed absolute checkout dependency would not be portable.
  Date/Author: 2026-11-10, user instruction.
- Decision: Resolve Muse import/checkpoint roots through `HarnessKind::Muse.nested_data_dir` and remove checkpoint's unused dirs dependency.
  Rationale: A searchable native Muse session must remain locatable for resume on every platform; one existing owner already implements the required XDG and private-home rules.
  Date/Author: 2026-11-10, integration agent after coordinator review.
- Decision: Select `.keep_configured_paths()` on public GrokBuild and Muse adapters from brokk-sessionwiki 0.34.25.
  Rationale: Existing shared-index IDs and reconciliation depend on configured path spelling; the library owns that policy without an mj wrapper or SQL migration. Its default policy must also preserve the already-published 0.34.24 keys.
  Date/Author: 2026-11-10, coordinator and integration agent after review.

## Outcomes & Retrospective

Implementation and registry validation are complete against brokk-sessionwiki 0.34.25, schema 9 unchanged. Cargo.lock records checksum bd795451e3a68781f9a3d1174675ab025382b73686f948f64b9afe09e7668670. Full dev-profile validation passed: 171 checkpoint tests with 2 ignored, 2516 controller tests with 19 ignored, workspace Clippy with warnings denied, rustfmt and diff checks.

The initial implementation is committed as cff4a06e6. Its review exposed the symlink key issue, now covered by a seeded real-index regression that alternates mj and standalone writers before deleting one home's source. The library owns physical identity adoption, preserving existing keys, IDs, metadata and per-home reconciliation without an mj wrapper or SQL migration. The compatibility correction is ready for the required commit/review gate; delivery commit, PR and CI links are reported to coordinator e3fdc54fb4f90c1f83bfdfe00c516aee.

## Context and Orientation

`mj-controller/src/sessionwiki.rs::native_adapters` registers enabled profile homes. `mj-controller/src/sessionwiki/harness_tools.rs` holds the bidirectional harness/tool mapping. The former custom `harness_adapters.rs` implementations are removed; `sessionwiki/native_tests.rs` verifies the replacement against the real library and native import readers. `MjolnirAdapter` in sessionwiki.rs indexes Mjolnir's own stored conversations and must remain. `mj-controller/src/import/` contains resume readers, which must also remain. `sessionwiki/tags.rs::testing::isolated_index` opens a real schema under temporary SESSIONWIKI_DATA while its shared lock protects environment access. Root Cargo.toml holds the dependency, Cargo.lock resolves it, and `docs/src/content/docs/sessions.md` documents the identical installation version.

## Plan of Work

First replace registration with public KimiCode, GrokBuild, Muse and Dsh constructors for each enabled profile home. Preserve deduplication by harness and home and the existing OpenCode registration. Rename the remaining mapping module to harness_tools. Replace wrapper tests with a real-library integration test that writes native histories in two homes per harness, searches each, deletes one, and verifies reconciliation leaves the other home searchable. Include duplicate and disabled profiles in the registration scenario.

Then temporarily set the workspace dependency path to `/home/jonathan/Projects/sessionwiki/.mj/clones/e3fdc54fb4f90c1f83bfdfe00c516aee`. Once the coordinator supplies the published version, use its registry version and update the lockfile and installation documentation together. Confirm the SessionWiki schema remains unchanged.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir/.mj/clones/f93bd930627476fc706e19d2fd4efef5`. Run focused `cargo test -p brokk-mj-controller sessionwiki::` outside the restricted sandbox while adapters stabilize. Before completion run the full touched-crate suites with `cargo test -p brokk-mj-checkpoint -p brokk-mj-controller` and workspace `cargo clippy --all-targets -- -D warnings` in the dev profile, preserving the normal mbx setup. Review the diff and stage only changed files before committing on the integration branch.

## Validation and Acceptance

Native adapter registration must index and find both homes of each harness. A repeated profile home must produce only one adapter. A disabled home must not be indexed. Removing one home's session must archive only that home's row. Query results must retain kimi-code/grok-build/muse/dsh names and original path keys. Existing import and stored-transcript tests must continue passing. Tests use temporary index storage, never the live SessionWiki database; any manual daemon or CLI test must name an isolated `--instance`.

## Idempotence and Recovery

Tests create disposable histories and indexes and may be rerun. Do not alter Rust target storage or install this build. Coordinate parent changes through mj-agents messages. Commit only portable registry dependencies, never the temporary path dependency.

## Artifacts and Notes

Initial checkout was clean master at task start. Branch creation required normal escalated Git access because .git is protected in the restricted sandbox.

## Interfaces and Dependencies

SessionWiki must export `sessionwiki::adapters::{KimiCode, GrokBuild, Muse, Dsh}`, each with `in_home(home: impl Into<PathBuf>)`. Kimi/Grok/Muse remain shared stores with stable keys. DSH accepts bridge JSON and native directories below the configured home. No index schema change is intended.

The final dependency is brokk-sessionwiki 0.34.25. Select `GrokBuild::in_home(home).keep_configured_paths()` and `Muse::in_home(home).keep_configured_paths()` to preserve the lexical keys of the former mj adapters. Keep Kimi's canonical key behavior. The seeded-index integration case must retain original IDs, produce no duplicate alias/canonical hits and reconcile deleted sources only in their own home.

The released Adapter trait also supplies `modified_at(&self, key: &str) -> Option<chrono::DateTime<chrono::Utc>>` with a default returning None. Forward it from `PreparedFiles` and `SharedMjolnirAdapter` to preserve the wrapped adapter's time semantics.

Version 0.34.25 also supplies `path_key_depth(&self) -> Option<usize>` with a None default. Forward it from the same two delegating wrappers. Grok and Muse specify their fixed relative key depths so the library indexer can adopt existing aliases without a schema change or mj SQL migration.

`mj-checkpoint/src/native.rs::muse_sessions_root` delegates to the existing `mj_core::config::HarnessKind::Muse.nested_data_dir`, which ignores relative XDG overrides and uses home-relative `.config` and `.local/share` defaults on every OS. Existing checkpoint tests and the controller's independent SessionWiki adapter root contract cover the cross-platform agreement.

Plan created 2026-11-10 to track the coordinated extraction and its release dependency.

Updated 2026-11-10 after implementation preparation and the coordinator's registry/version check; validation remains gated on the new public exports.

Updated 2026-11-10 after joint integration/import tests passed. Final validation now waits for the coordinated published package, rather than adapter implementation.

Updated 2026-11-10 after all 118 affected tests passed against the exact release checkout. Forwarded the new time hook and made fixture keys respect the adapters' different Windows path spellings.

Updated 2026-11-10 after registry validation and the coordinator's final Muse root review. Recorded the isolated timing-test rerun and the small shared-root correction still under validation.

Updated 2026-11-10 after final registry validation: both touched-crate suites and workspace Clippy passed. Implementation is complete and ready for the required committed-tree review and authorized branch/PR delivery.

Updated 2026-11-10 after the first committed-tree review exposed symlink key compatibility. Recorded the failing seeded-index reproduction and the public library policy correction in 0.34.25; final validation and review remain pending for that patch.

Updated 2026-11-10 to cover mixed standalone/mj writers sharing one index and forward the new library identity hook. The library owns physical identity adoption; mj only selects the legacy discovery preference and verifies interoperability.

Updated 2026-11-10 after publication and final registry 0.34.25 validation. All touched-crate tests, workspace Clippy and formatting pass; the seeded mixed-writer regression protects the reviewed compatibility defect.
