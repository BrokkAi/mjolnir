# Preserve the configured review trigger with session reviewer overrides

This ExecPlan follows `.agents/PLANS.md` and remains current through implementation and validation.

## Purpose / Big Picture

Automatic review defaults to off. Users can select end-of-turn or before-push review globally and can override reviewer settings for a session. Currently, supplying only `--review-count`, `--review-model`, `--review-effort`, or deprecated `--review-tier` implicitly selects end-of-turn review. This silently replaces a configured before-push trigger.

After this change, reviewer options without `--review` inherit the configured trigger. A user who selected before-push review can request three reviewers without changing when they run. An explicit `--review off|turn|push` still overrides the trigger. Global off remains off until explicitly enabled, and existing stored session choices keep their meaning.

## Progress

- [x] (2026-11-04) Audit canonical configuration, CLI options, UI, documentation, daemon lifecycle, and worker launch. Confirm the CLI implicit trigger as the only code defect found.
- [x] (2026-11-04) Choose an inheriting persisted session choice and retain `ReviewConfig::for_session` as the single effective-settings resolver.
- [x] (2026-11-04) Correct the README's claim that automatic review always checks every turn. Parent owns this small edit after the delegated editor failed to restart.
- [x] (2026-11-04) Implement the core inheriting choice and persisted-format coverage.
- [x] (2026-11-04) Update CLI and import flag handling, existing boundary tests, and product/configuration documentation. Parent corrected the incomplete CLI handback after its follow-up failed to restart.
- [x] (2026-11-04) Resolve database and protocol compatibility: breaking schema revision/floor 107 and client protocol 72; retain previous migrations unchanged.
- [x] (2026-11-04) Add database/API boundary coverage for inherited count/model/effort. Existing exhaustive migration/interruption coverage includes revision 106 to 107 and rejects revision 106 readers.
- [x] (2026-11-04) Run touched-crate suites, affected-test reruns, independent source review, and workspace Clippy. All passed after the documented fixes.
- [x] (2026-11-04) Implement the user's additional terminal/web subagent-list default. Terminal scopes move their active filter to/from inactive saved scope slots and default to Not idle; uppercase I selects it without colliding with Create session on n. Web has a persisted Show/Hide idle control and hidden counts.
- [x] (2026-11-04) Validate subagent filter transitions and rendering, preserve top-level filters, and complete final Clippy on the combined tree.
- [x] (2026-11-04) Commit the validated review-inheritance checkpoint as `71850aa7d`.
- [x] (2026-11-04) Commit subagent-list defaults as `6eb3146b8` and request the required before-push review.
- [x] (2026-11-04) Address all first-review findings and pass focused native navigation, suspend restoration, and web review-visibility regressions; commit as `2f0d27ea1`.
- [x] (2026-11-04) Validate and commit review corrections, obtain a clean before-push review, and push to `origin/master`.
- [x] (2026-11-04) Resolve integration with upstream `5238411cd`, preserve migration history, use client protocol 82, and make child filters kind-aware. All-target TUI/client check and Worker/Reviewer isolation regression passed.
- [x] (2026-11-04) Validate merged browser behavior: 58 Node checks and all 19 dashboard Playwright cases passed.
- [x] (2026-11-04) Complete merged-tree Rust suites and Clippy, commit the merge, obtain a clean before-push review, and push.
- [x] (2026-11-04) Pass merged core/client/controller/CLI-library suites, workspace formatting and Clippy; resolve both TUI compact/history failures and pass six focused TUI regressions plus crate Clippy.
- [x] (2026-11-04) Resolve merged CLI integration failures without weakening assertions: strict incompatible-store exit and all nine PTY failure reruns pass.
- [x] (2026-11-04) Move the mandatory compatibility check to an independently supervised task and cancel optional target refresh on shutdown. All three store-divergence integration cases passed; strict breaking-revision exit passed in under one second.
- [x] (2026-11-04) Repair isolated PTY profile/tool fixtures and implement completion of unnecessary first-run setup without leaving Welcome open or replacing another dialog.
- [x] (2026-11-04) Pass nine PTY cases, all four Welcome dialog cases, final workspace Clippy, and formatting. The merged implementation is ready for the merge commit and renewed review.
- [x] (2026-11-04) Commit the integration as `491e7a15f` and request renewed review against upstream `5238411cd`.
- [x] (2026-11-04) Address the second review's lifecycle finding in canonical attention and extend the existing child-scope workflow for all four lifecycle states; the focused regression passed.
- [x] (2026-11-04) Pass final terminal suite (570 passed, two ignored), workspace Clippy, and formatting for the lifecycle correction.
- [x] (2026-11-04) Commit the lifecycle correction as `25d1e6db0`, obtain clean review for tree `4526b87`, and push successfully to `origin/master`.

## Surprises & Discoveries

The user's host configuration already selects push. The global built-in default is off, and the user explicitly confirmed that it must remain off. No host configuration edit is needed.

The CLI helper deliberately documented an implicit turn choice for reviewer-only flags. `SessionReview` currently has only `Off`, `Turn`, and `Push`, so it cannot store reviewer overrides while inheriting the trigger. Fixing only the CLI's string literal would replace one implicit trigger with another.

The worker and daemon already distinguish the triggers: turn completion only arms turn review, push review uses committed changes and a mailbox result, and launch uses the effective session configuration. The audit found no additional lifecycle conversion from push to turn. These conclusions are static-source findings until validation completes.

## Decision Log

- Decision: Preserve the global off default, explicit trigger choices, and the legacy `enabled` mappings unchanged.
  Rationale: The user explicitly confirmed off as the correct default; compatibility with old explicit session settings is part of the intended behavior.
  Date/Author: 2026-11-04, parent agent following user correction.
- Decision: Add `SessionReview::Inherit` with reviewer overrides, serialized with `mode = inherit`, and resolve its trigger only through `ReviewConfig::for_session`.
  Rationale: A durable inheriting choice avoids fetching a configuration snapshot in the CLI and keeps the daemon's effective configuration authoritative after resume or configuration changes.
  Date/Author: 2026-11-04, parent agent.
- Decision: Deprecated `--review-tier` remains accepted and ignored; used alone it inherits the trigger like other reviewer-only flags.
  Rationale: A deprecated, ignored value should not silently change review timing.
  Date/Author: 2026-11-04, parent agent.
- Decision: All subagent scope transitions, including suspend exits and native-parent shortcuts, must use the same filter owner; only an editing filter claims native navigation keys.
  Rationale: The first committed-tree review found overlooked direct transitions and an old no-filter keyboard gate. Centralizing all transitions prevents saved top-level filters from being replaced by child state.
  Date/Author: 2026-11-04, parent agent after independent review.
- Decision: A web child with an open `turn_review` remains visible regardless of harness idleness.
  Rationale: Running review or a pending review decision is work or attention, and existing UI already treats review presence as authoritative.
  Date/Author: 2026-11-04, parent agent after independent review.
- Decision: Scope filter identity uses upstream's complete `ChildWorkspace` identity (parent plus kind), and the default hides idle only for Worker scopes.
  Rationale: Upstream added distinct Reviewer workspaces. Sharing only a parent ID would leak filters between them and unexpectedly hide reviewer history.
  Date/Author: 2026-11-04, parent agent during upstream integration.
- Decision: Mandatory store compatibility enforcement runs independently of optional provider, target, and credential preparation; optional preparation is cancellable during daemon shutdown.
  Rationale: The existing real migration regression showed schema mismatch rejected by the writer while the daemon remained alive. Its only periodic guard was behind optional refresh work; moving that single owner removes the dependency instead of extending the exit timeout.
  Date/Author: 2026-11-04, parent agent accepting isolated daemon regression evidence.
- Decision: A no-setup completion clears pending Welcome and closes only an active Welcome dialog.
  Rationale: Startup now requests setup asynchronously even for configured installations. The old no-op completion left Welcome permanently discovering; an unrelated Settings draft must remain intact.
  Date/Author: 2026-11-04, parent agent after PTY diagnosis.

## Outcomes & Retrospective

Both requested changes are delivered to `origin/master` at `25d1e6db0`. Automatic review still defaults to off; reviewer-only options inherit the configured trigger. Ordinary terminal and web subagent views default to hiding idle children, while reviewer history, active lifecycle work, failures, and review activity remain visible. Final review was clean for tree `4526b87`, and Git confirmed the push from `5238411cd` to `25d1e6db0`. The final terminal suite passed 570 tests with two existing ignores; workspace Clippy and formatting passed.

The integrated implementation is validated. Merged browser checks passed 58 Node checks and all 19 Playwright dashboard cases. Merged core (561 passed, four ignored), client (44 passed), controller (2,267 passed, 19 ignored), and CLI-library (236 passed) suites passed. Both initially failing TUI compact/history workflows and four directly affected guards passed after preserving unknown relationships and explicitly selecting All for history. The live store-divergence cases passed (three total), including strict incompatible-revision exit in under one second after making its guard independent of optional preparation. All nine previously failing PTY cases and all four Welcome cases passed after completing the no-setup state transition and correcting isolated fixture prerequisites. Final workspace Clippy and formatting passed. Evidence for the final startup checks is `.mj/agents/45af5a7af38c6f18cf3f14cfce887b54/summary.txt`; implementation acceptance, committed-tree review, and Git delivery are complete.

Review inheritance implementation and validation passed: core 553 passed/4 ignored, client 43 passed, controller 2,255 passed/19 ignored, and CLI library 231 plus its integration targets. The focused inherited-review HTTP test also passed. Logs are in `.mj/agents/96db413dbd2386a48ffbb29510d32ce0`.

The additional subagent-list defaults and transition/browser regressions are complete. All eight web Node modules passed across the main run and focused elevated projects-module retry; that module's hidden failure was sandbox spawnSync EPERM. All 19 elevated Playwright dashboard cases passed across the 18 passing cases and the focused summary-child history retry. Terminal validation initially caught a Create-session shortcut collision, missing native activity classification, and history/scroll fixtures that needed All. After corrections, all 37 failed TUI cases passed focused reruns; the other 527 cases had already passed (two ignored). The directly affected retained-native-child regression also passed again. Final workspace Clippy passed after the last test edits. Evidence is in `.mj/agents/348efb3b6f173b3da784ee6fb5345811` and `.mj/agents/59107cb89cda02cf226505cad1c8d162/final-validation.txt`. Implementation and acceptance are complete; the prepared commits proceed through the required review gate and Git delivery.

## Context and Orientation

`mj-core/src/config.rs` defines `ReviewConfig`, `ReviewTrigger`, and `SessionReview`. The global trigger is off, turn, or push. A session with no override follows all global settings. The new inheriting choice follows the trigger while overriding only supplied reviewer settings. `ReviewConfig::for_session` produces the effective configuration used by the daemon and worker launch.

`mj-cli/src/api_commands.rs::session_review` constructs the session choice for both `mj new` and `mj import`; `mj-cli/src/import.rs` shares the helper. `mj-core/src/config/tests.rs` covers the JSON shape stored in the session record. CLI tests already parse real command arguments and examine the shared choice.

The database stores session choices as JSON. A new enum value is incompatible with older readers, even without an SQL column change. Migration 107 raises the reader/writer floor to 107 transactionally with its migration marker and revision. The integrated client/daemon protocol is 82: upstream supplies revisions through 81, and this task adds the enum boundary. Workers receive the resolved three-value trigger, so this task adds no relay or checkpoint format change; upstream relay 49 remains intact.

Human documentation is `docs/src/content/docs/turn-review.md`; the bundled agent configuration reference is `mj-core/assets/skills/mj/references/configuration.md`. Both currently state that reviewer-only flags select turn review and must describe inheritance instead.

## Plan of Work

The core implementation adds the inheriting choice with the same optional count, model, effort, and deprecated tier fields as explicit enabled choices. The trigger accessor returns no explicit trigger for inheritance. The effective-settings resolver leaves its global trigger intact for inheritance and applies count, model, and effort overrides in the same path as explicit choices. Existing JSON encodings retain their meanings.

The CLI implementation emits inheritance for reviewer-only flags, emits no choice for no review flags, and retains explicit choices. Extend the existing CLI/import boundary tests so parsed settings-only flags run through the effective-settings resolver for all three global triggers. Update documentation and help in the same slice.

Classify and implement the persisted compatibility change at a new revision. Never rewrite a shipped migration. Bump a protocol version only where required by the actual transport of the new enum. Keep migration and API regressions in their existing isolated test environments.

After integration, inspect the diff and decisive tests. Run focused tests first if needed to resolve failures, then full touched-crate suites and `cargo clippy --all-targets -- -D warnings` once after final changes. Use the existing mbx Cargo setup without redirecting output. All `cargo test` commands run outside the restricted sandbox. Any new executable invocation uses a separate named instance.

## Concrete Steps

Run from the repository root. Exact Cargo package selectors must be checked against each touched crate's `Cargo.toml`; do not infer package IDs from directory names. Focused filters include `a_session_review_choice_is_stored_in_a_stable_shape`, `new_review_flags_choose_the_sessions_own_review`, and `an_import_can_choose_the_sessions_review`. The existing `review_trigger_loads_legacy_settings_and_saves_only_the_trigger` test must continue passing without changing its expected off/turn compatibility behavior.

After the final validation, stage only files changed for this task and commit on the current branch. Call `request_review` against the committed tree and address or explicitly evaluate all findings before `git push origin HEAD:master`. If upstream moved, fetch and merge `origin/master` without rebasing, review the merged commit, and push again.

## Validation and Acceptance

Given global push and `mj new --review-count 3`, the serialized session choice inherits the trigger, and the resolved configuration uses push with count three. The analogous import path behaves identically. The same settings-only flags preserve global turn and global off. Explicit trigger arguments override each global mode. Legacy JSON choices, including the legacy on alias, remain readable and keep their behavior. Saving global configuration still defaults to off and writes current trigger keys.

Old daemons and incompatible stores must be refused or upgraded by the project's established compatibility mechanism rather than parsing an unknown stored enum or dropping it. Existing isolated upgrade regressions provide the appropriate evidence. Required Rust checks and before-push review must pass before delivery.

## Idempotence and Recovery

File edits and focused tests are repeatable. Do not change or test the host's live store. Keep all configuration, data, and daemon activity in existing isolated test directories or a named instance. No Cargo target redirection or build-cache alteration is authorized. If a child edit fails, inspect the shared diff and take over only that child's owned files after confirming ownership.

## Artifacts and Notes

Audit handbacks live under `.mj/agents/ee18e6282b5b03702ea6e3e4edbabaa4`, `.mj/agents/356516081e6442ae033f823bb3bb4e60`, and `.mj/agents/649173e1966f84f5c9b695431c15b529`. The core author owns config code/tests; the CLI author owns CLI code/tests and the two documentation references; the parent owns this plan, README, design, integrated acceptance, and Git delivery. Compatibility ownership is assigned after its focused investigation.

## Interfaces and Dependencies

This task adds no crate or external dependency. `SessionReview::Inherit` is the new stored/shared choice. Its optional fields retain existing types. `SessionReview::trigger` returns `Option<ReviewTrigger>`; `ReviewConfig::for_session` remains the sole place combining global and session settings. `mj-controller/src/database.rs` and `database/schema.rs` define schema revision and floor 107, and integrated `mj-client/src/daemon.rs` defines protocol 82. Server action decoding accepts inherit and validates its model/effort text through the same path as explicit choices. Terminal filter scope identity includes both parent and child kind from `ChildWorkspace`, so reviewer history and worker filters remain independent.

Revision note (2026-11-04): Created after the user confirmed the off default and requested an audit for remaining end-of-turn assumptions.

Revision note (2026-11-04): Recorded implemented inheritance, compatibility boundaries, independent review, and the parent takeover of incomplete CLI edits after the delegated correction could not restart.

Revision note (2026-11-04): Recorded final database/API regression coverage and validation start. The historical migration/interruption matrix covers the new revision without a redundant migration-specific test.

Revision note (2026-11-04): Recorded the API fixture compile correction and deferral of final Clippy while the additional subagent-list request is investigated.

Revision note (2026-11-04): The user confirmed the idle-subagent default should apply in both terminal and web. Terminal implementation reuses scoped session-filter state with a Not idle choice; web uses a subagent-only Show idle control and existing published activity facts. Global/top-level session defaults remain unchanged, and working, blocked, failed, and unread completions remain visible.

Revision note (2026-11-04): Recorded passing review suites, browser validation and sandbox diagnosis, scope restoration, uppercase I shortcut, and native attention classification. Native running/failed/disconnected states refine the canonical SessionFacts attention result without altering synthetic lifecycle records; known waiting states remain waiting. Existing history/scroll fixtures opt into All explicitly.

Revision note (2026-11-04): The review-inheritance checkpoint is committed as 71850aa7d. Final TUI reruns passed 34 of 37 formerly failing cases; the remaining historical menu/escape workflows now explicitly reveal idle rows and await their focused reruns. Browser regression and final workspace Clippy passed.

Revision note (2026-11-04): Recorded successful final three history/menu reruns, the retained-native-child check, all browser/Node results, and final workspace Clippy. Source acceptance is complete; remaining work is the required committed-tree review and push.

Revision note (2026-11-04): The first before-push review found three unique valid gaps (one reported twice): native navigation under filters, automatic scope exits bypassing filter restoration, and web review activity being hidden. Acceptance is reopened for these corrections and their regressions.

Revision note (2026-11-04): Upstream advanced to 5238411cd with significant concurrent changes. Integration is required before final delivery; a focused investigation is mapping compatibility counters and shared reviewer-workspace changes while review corrections receive focused validation.

Revision note (2026-11-04): Focused review-fix tests passed and were committed as 2f0d27ea1. Merge of 5238411cd is in progress with client protocol 82 (upstream 81 plus this task), unchanged relay 49, and retained local migration 107/floor 107 after upstream's unchanged migration history through 106. Filter ownership is being ported to kind-aware ChildWorkspace scopes.

Revision note (2026-11-04): Merge conflicts and kind-aware filter ownership are resolved. The merged all-target TUI/client check, Worker/Reviewer isolation regression, 58 Node checks, and all 19 browser dashboard cases passed. Full affected Rust suites and workspace Clippy are running before the merge commit and renewed review.

Revision note (2026-11-04): Merged core/client/controller/CLI-library suites and workspace Clippy passed. Fixed compact-child loading lost during the semantic merge and explicitly enabled All in the reviewer-kind history fixture; six TUI regressions passed. Remaining CLI integration failures are isolated to startup test environment/readiness and mandatory incompatible-store exit; these are being diagnosed without weakening assertions or changing mbx storage.

Revision note (2026-11-04): Fixed mandatory schema-check scheduling and optional refresh cancellation; the strict exit test and all store-divergence cases pass. PTY fixtures now provide actual profile homes and required host commands. A separate production bug opened Welcome before the setup RPC and never retired it on a no-setup result; the completion transition and dialog-ordering regression are implemented. The nine PTY failures and final lint are being rerun before merge delivery.

Revision note (2026-11-04): All remaining PTY, Welcome, and store-divergence regressions passed, and final workspace Clippy/formatting are clean. The integration is ready to commit and undergo renewed before-push review.

Revision note (2026-11-04): Integration was committed as 491e7a15f. Renewed review found an active lifecycle child could inherit Inactive attention without a local operation and be hidden. The correction belongs in canonical attention classification, preserving a single owner rather than adding a filter-specific busy predicate.

Revision note (2026-11-04): Provisioning, checkpointing, closing, and destroying now carry Working attention from their lifecycle state; stopped stays Inactive. The existing child-scope workflow covers all four without local overlays and passed. Full terminal validation and lint are running before the correction commit and renewed review.

Revision note (2026-11-04): All 570 terminal tests passed (two ignored), as did final workspace Clippy and formatting. Parent reviewed the final diff; the only edit during lint was moving the hard-won comment immediately above its test attribute, with no behavioral change. The correction is ready to commit and review.

Revision note (2026-11-04): The final review is clean and the functional changes were pushed successfully as 25d1e6db0. Recorded completed progress and delivery evidence.
