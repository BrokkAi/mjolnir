# Restore session-owned subagent settings


This living ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective as implementation proceeds.

## Purpose / Big Picture


Profiles supply defaults at session creation. Existing sessions keep their own recorded delegation policy when profile defaults are saved, Move changes destination, or Resume and recovery reload records. Move offers editable Subagents, Model, and Effort controls even when destination pickers are skipped. Retired All models policies remain readable and preserved but cannot be newly selected. Discovery of models and model-specific efforts is shared and cached independently of these defaults.

## Progress


- [x] (2026-09-30) Read repository guidance, pre-cc23c703 reference, and current session, settings, and discovery paths. Current branch is hel4; unrelated scripts/__pycache__/ is preserved.
- [x] (2026-09-30) Restored session ownership and active-profile default editing, with persistence and legacy Resume regressions.
- [x] (2026-09-30) Restored Move controls and review, using shared comboboxes and rendered geometry.
- [x] (2026-09-30) Rendered profile subagent settings as directly clickable comboboxes and removed refresh controls.
- [x] (2026-09-30) Shared discovery inputs and cached model capabilities across live catalog, draft settings, and Move; added counting probes, stale-provider replies, and separate-draft tests.
- [x] (2026-09-30) Updated creation-default documentation. Dev-profile Clippy, formatting, Node syntax and browser behavior checks pass. Final isolated TUI suite passes 914 tests (2 existing ignored tests).
- [x] (2026-09-30) Full dev-profile Cargo suite and doc tests pass outside the sandbox. Implementation and verification are complete; delivery is a commit on hel4 followed by the authorized push of HEAD to origin/master.

## Surprises & Discoveries


The destination-default substitution lives in TUI ResumeWizard::subagent_change and web moveSubagentChange; controller Move already preserves an omitted override. Setup protection compares almost every HarnessProfile field, inadvertently blocking creation-default edits. Persistent capability fingerprints serialize the entire profile, and Setup keys serialize the entire config. ProfileCatalog cached only default capabilities in memory; model lookups bypassed it. Setup and restored Move initially had separate request counters despite sharing a reply type; one monotonic counter now prevents replies crossing dialogs. Provider file changes advance the serialized catalog generation and retire pending replies. A failed shared discovery also exposed a delayed-waiter race: a first attempt could remove a newer retry in the same configuration generation. Publication and removal now require ownership of that exact shared future, proven by a retry regression. The terminal has a minimum width of 60 columns and minimum height of 22 rows; rendered popup tests use 60×24 for narrow coverage. Upstream uses a dismissible × title rather than a Cancel footer on the Move files page.

## Decision Log


Use the pre-cc23c703 Move controls as the behavioral reference, retaining the current creation flow and retirement of new All models selections. No database or protocol migration is needed because session records and Move requests already hold optional policies. ViewerProfile adds a defaulted opaque discovery key derived from the shared projection, allowing browsers to invalidate relevant edits without exposing environments. Discovery-input interpretation will live beside HarnessProfile in mj-core so UI and controller layers share it. Preserve normal mbx Cargo storage (target points into /mnt/optane/mbx-targets). User explicitly authorized pushing to origin/master after completion; remain on hel4 and push HEAD:master. The fetched upstream was two commits ahead, so a fast-forward with autostash incorporated both without switching branches and restored all implementation edits.

## Outcomes & Retrospective


Implementation is complete. The controller suite passes 2081 tests (9 existing ignored tests), including persistence, Resume, shared-cache probe counting, provider invalidation, delayed retry ownership, and Node browser behavior. Core passes 597 tests. The final TUI suite passes 914 tests (2 existing ignored tests), covering rendered mouse press/release, popup commit and dismissal, redraws at 60 columns, recorded unavailable values, and recovery overrides. Required dev-profile Clippy, formatting, JavaScript syntax and diff checks pass. The full Cargo run, including worker, CLI integrations and doc tests, passed with exit code 0. Subsequent TUI regression additions were validated by the final 914-test TUI run and all-target Clippy. Logs are under /mnt/optane/mjolnir4-subagents-{tests,tui,clippy}-final.log. Normal mbx storage and isolated test configurations were retained, with no default instance or live session changes.

## Context and Orientation


mj-core/src/config/harness.rs defines HarnessProfile, the named harness configuration. mj-core/src/subagent.rs defines durable SubagentPolicy and advertised choices. mj-core/src/state.rs protects active session dependencies during settings saves. mj-controller/src/controller/profile_config.rs runs supervised discovery and stores capabilities in the existing profile_config_cache table; mj-controller/src/server_runtime/profile_catalog.rs warms and shares discovery for the daemon. mj-tui/src/setup.rs edits a separate draft configuration. mj-tui/src/wizards/ owns the Move step machine and rendered forms. mj-controller/src/web/viewer.js implements browser Move; Rust viewer tests execute its functions under Node.

## Plan of Work


First let Setup change subagent defaults without changing existing session rows and restore the recorded-policy Move draft. Restore only Move selectors, keeping Create free of selectors. Make explicit changes invalidate the prepared Move and keep untouched legacy policies unvalidated. Then use shared discovery inputs (harness, home, environment) in persistent fingerprints, live catalog identity, and Setup request identity, retaining model-specific efforts and sharing concurrent misses. Render each profile policy field through existing ComboBox controls with actual render rectangles and overlays last. Relevant profile/provider changes must invalidate affected capabilities; draft probing must not adopt unsaved config. Update help and add behavior tests near each module.

## Concrete Steps


Work from /home/jonathan/Projects/mjolnir4. Read files with rg and sed and compare git show cc23c703^ for the reference. Run cargo fmt --all -- --check, cargo test, and cargo clippy --all-targets -- -D warnings in the dev profile using existing mbx, with elevated permissions for every cargo test. Run affected browser tests included in controller tests and any applicable browser harness. Tests use existing isolated directories and named instances; never run a new binary against the default host instance. Review git diff --check and stage only edited files, commit on hel4, then git push origin HEAD:master. If origin advances, incorporate a fast-forward on the current branch when possible, preserving local edits, then validate the combined tree. Do not rebase or switch branches; never force-push.

## Validation and Acceptance


Behavior tests must show two sessions on one profile retaining distinct policies after saving a new default, and a new session inheriting that default. Untouched Move to a different default must omit the override; explicit edits submit it; legacy All models survives Move, Resume and recovery. Mouse press/release tests operate on rendered controls, including popup selection, dismissal, redraw and narrow terminals. Counting probes show effort and unrelated edits do not discover again, cached models are reused, concurrent misses share one discovery, stale replies do not change current choices, and relevant discovery inputs trigger probes. Required Cargo, Clippy and formatting checks must pass before committing and pushing.

## Idempotence and Recovery


All persistence tests use temporary isolated stores and config directories. No schema changes or live data edits are planned. Discovery is restartable background preparation and does not hold daemon upgrade admission. Failed probes report errors and allow subsequent normal population attempts. Preserve unrelated files and do not redirect Cargo storage.

## Artifacts and Notes


Reference commit: cc23c703 removed Move selectors and switched Move to profile defaults. The working branch starts at c0ff7b26. /tmp/subagent-reference.diff is a disposable reference diff.

## Interfaces and Dependencies


Reuse mj_chat::components::{ComboBox, ComboBoxState, Dialog, FormViewport}, existing DashboardAction::DiscoverSubagentOptions and daemon/HTTP discovery routes, ProfileCatalog, and the existing persistent profile capability cache. Introduce one serializable discovery-input projection for HarnessProfile and reuse it in cache keys; preserve existing public wire types.

Revision note: initialized the implementation plan from user intent and inspected code; recorded push authorization and known defects.

Revision note: recorded completed implementation, shared reply ownership, additive browser invalidation key, passing Node behavior regression, and fast-forward incorporation of upstream before final validation.

Revision note: recorded final review corrections, shared-attempt retry ownership, narrow-terminal constraints, final TUI recovery/display evidence, and validation results before commit.

Revision note: the full Cargo suite completed successfully; all required checks pass and the validated tree is ready for commit and authorized push.
