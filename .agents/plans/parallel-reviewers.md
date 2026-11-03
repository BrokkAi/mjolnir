# Run configurable parallel reviewers and combine their verdicts

This ExecPlan is a living document maintained under `.agents/PLANS.md`. Keep `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` current. It extends `.agents/plans/plain-text-review-verdicts-and-override.md`, whose initial implementation is in progress in this same checkout.

## Purpose / Big Picture

The user wants independent reviewer sessions to inspect the same captured change in parallel. A saved review setting defaults to two reviewers. Explain in settings and human documentation that one to three reviewers are reasonable and that more reviewers have diminishing returns. More than three is advice, not by itself an invalid choice.

The primary agent must receive every reviewer's findings, labeled with the separate reviewer session that produced them, and be told that findings may overlap. Do not deduplicate or synthesize away an independent finding. When every reviewer says `No findings.`, one combined clean verdict is enough. The pre-push gate may treat a group as clean only when every requested reviewer completed cleanly; a missing, failed, cancelled, or still-running reviewer cannot count as clean.

The current plaintext contract remains: the exact trimmed ASCII-case-insensitive `No findings.` sentinel is clean, an empty answer is a failure, and other text is findings. An explicit audited override remains a separate user-visible decision from a clean review.

## Progress

- [x] (2026-11-03) User requested parallel reviews during the plaintext/override work; parent stated the assumption that the saved count applies to both turn and push reviews.
- [x] (2026-11-03) Mapped driver, host, durable registration, configuration/session/CLI, terminal/web presentation, and restart assumptions; approved the typed snapshot and per-role registration design below.
- [x] Saved-count core/CLI/docs and numeric TUI/presentation implementation complete; focused state tests and inspected golden updates pass.
- [x] Pure driver fan-in, durable emitted-work replay, and host group recovery implementation complete; integrated compilation and focused host/driver regressions pass after corrections.
- [x] Cover group ordering, partial failures, cancellation/recovery, attribution, and push-gate behavior at existing integration/golden surfaces.
- [x] Complete final workspace coverage, targeted failure reruns, formatting, Clippy, documentation, and frontend validation; commit coherent checkpoints on the current branch.
- [x] Pushed the validated delivery through `46cf31c63` to `origin/master`; recorded the successful fast-forward from `ec3c0a3cb`.

## Surprises & Discoveries

The current review host has one `ReviewSlot` per primary, one configured reviewer identity, and a scalar reviewer child id. The driver currently reaches a verdict on its first completion. Existing startup clears active markers and closes orphan reviewer children; new groups need durable snapshots and reattachment instead. The terminal caches only one reviewer pane, and web links to one scalar child. Each of these assumptions needs an explicit per-role representation.

Saving a final outcome without its emitted work leaves a crash window before baseline, forward cleanup, or close dispatch. The snapshot now stores the exact `ReviewRequest` stage payloads and exposes driver-owned `recovery_requests()`, preserving original prompts and command ids rather than regenerating them after an upgrade. The host must persist before acknowledging the result, then dispatch or recover that saved work. Push mailbox verdicts use the stable group id on replay.

The original count-settings child failed restart. Its replacement returned only partial core edits twice; the parent completed core config, CLI, contracts, and documentation, while the presentation owner added the numeric TUI editor. No partially completed slice is accepted as finished.

Independent review found a registration recovery race: a child can be registered before its id is saved in the group. Startup now retains children by the group's durable parent/request-key relation, including registrations whose child id is not yet saved. A retired host no longer closes a child merely because its result channel disappeared; the current durable group owner reconciles it. A controlled restart test covers this gap.

The host checkpoint exposed a second durability gap for failed groups: the handback was acknowledged with the active marker still saved, then the prompt hold was released before a later save cleared the marker. The terminal outcome save now clears that marker before acknowledging and releasing the hold, and failed saves restore the prior driver and state. Recovery only restores a hold when the durable active marker requires one.

The terminal transcript's final-event barrier must accept an interrupted prompt as well as a completed one: closing a reviewer after handback suspends its session and can interrupt its harness turn. Polling now requires a matching child, a started prompt, a terminal Completed/Interrupted outcome, and its applied journal ordinal. This covers suspension without accepting prestart idle or polling forever; the existing barrier regression includes both outcomes and an unapplied-terminal case.

The first delivery push was rejected after upstream added daemon container placement. Both independent branches had selected configuration version 20. The merge retains upstream's daemon fields and uses version 21 for the combined configuration, upgrading versions through 20 in memory. Canonical configuration docs now agree on 21. Only affected merge checks are repeated; the earlier full workspace coverage remains evidence for unchanged code.

This host lacks GTK/WebKit development packages (`pkg-config` cannot find `gtk+-3.0` or `webkit2gtk-4.1`), matching prior project feedback to exclude `brokk-mj-desktop`. ALSA is available. Final Rust checks cover every buildable workspace crate with an explicit desktop exclusion; no host package installation or build-target/cache redirection is needed.

## Decision Log

- Decision: use one saved review-count setting for turn and before-push reviews, defaulting to two.
  Rationale: the user asked for an option and a default without limiting it to one trigger, and both triggers share reviewer orchestration.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: label findings with stable reviewer-session identities and retain independent text, including redundant findings.
  Rationale: the user explicitly asked to receive all findings from the separate sessions and to make possible redundancy clear.
  Date/Author: 2026-11-03 / user and implementing agent.
- Decision: only all-completed-clean groups produce `Clean`.
  Rationale: the user explicitly requires the gate to remain closed unless every reviewer is clean. Partial failures must remain visible and may not erase findings from completed reviewers.
  Date/Author: 2026-11-03 / user and implementing agent.
- Decision: resolve one configured profile/model once per group, then run that selection in independent sessions and deliver one final aggregate in launch order.
  Rationale: the saved profile keeps its current meaning, concurrent sessions provide independent judgments, and completion ordering cannot reorder findings or start competing corrective turns.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: mixed findings and explicit reviewer failures produce findings plus failure metadata; deliver every findings section, keep the gate blocked, refuse override, and preserve the last fully reviewed baseline.
  Rationale: a failure cannot erase another reviewer's findings or imply a completed review. Findings must reach the primary even for an incomplete group. The next review must inspect the whole still-uncovered change rather than only verify known findings.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: use `[review] count`, default two, as a positive `NonZeroUsize`; optional per-session `count` inherits the global count when absent. Offer `--review-count`; count alone selects turn review, while an explicit `--review off` remains off.
  Rationale: this matches existing model/effort override behavior, preserves old records, distinguishes inheritance from an explicit value, and does not impose an arbitrary cap at the recommended range.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: do not add timeouts that cancel a busy reviewer merely for taking too long.
  Rationale: project rules require accepted worker turns to survive daemon replacement and prohibit using a timeout as permission to stop a busy worker. Explicit harness/lifecycle failures or user cancellation determine terminal failure.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: persist the driver's exact emitted stage requests and replay them through the driver after restart.
  Rationale: snapshot phase alone loses cleanup/coverage work in a crash window and cannot guarantee an identical prompt payload for an existing command id. The driver remains the single owner of those decisions.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: exclude only `brokk-mj-desktop` from the full workspace checks on this host.
  Rationale: required GTK/WebKit development libraries are absent and existing user feedback explicitly records this limitation. All touched crates and other buildable workspace crates remain included.
  Date/Author: 2026-11-03 / implementing agent.

## Outcomes & Retrospective

The plaintext and audited-override companion checkpoints and the integrated parallel feature are implemented, validated, and pushed to `origin/master` through `46cf31c63`. Upstream daemon placement is retained, with combined configuration version 21. Every buildable workspace test target has been exercised; identified failures were fixed or verified in focused reruns, without repeating passing suites. Formatting and final workspace Clippy pass, as do documentation and web checks. The host's missing desktop libraries and the single transient fake-engine test failure are documented below.

## Context and Orientation

Mjolnir is a Rust workspace. `mj-core` defines configuration, persisted session and review data, and the worker/daemon wire formats. The controller daemon in `mj-controller` owns lifecycle operations and the review host. `mj-review` constructs reviewer prompts and classifies plaintext reports. The worker in `mj-worker` exposes the `mj-agents` MCP tools to local and remote sessions. The primary requests push review through `request_review`; automatic turn review starts after an eligible prompt-driven turn finishes. In both cases hidden reviewer children report through `handback(message: String)`.

Start the orchestration investigation in `mj-controller/src/review_host/{host,events,reviewer,reviewer_child,dispatch,resolve}.rs`, `mj-review/src/driver.rs`, and `mj-core/src/storage.rs`. Start the settings investigation at `mj-core/src/config.rs`, `mj-core/src/review/settings.rs`, the existing review settings projection in `mj-client`, CLI review arguments in `mj-cli`, and persisted per-session review configuration in `mj-controller`. Follow existing settings projections to the TUI and web viewer rather than creating a separate configuration path.

The current override work replaces parallel preparation flags with one typed preparation owner, uses an ordered persistence lane, and records an acknowledged audit notice before unlocking reviewed tree refs. The parallel-review extension must retain these ownership and durability guarantees.

## Plan of Work

The discovery milestone is complete. The design separates the driver-owned ordered outcome slots from host-owned child registrations. Both are persisted together in the active review group. Driver snapshots live in `mj-core` so the storage crate does not acquire a dependency on `mj-review`. The driver restores from that typed snapshot. Child request keys are stable before registration, and the existing `(parent, reviewer request_key)` lookup makes repeating a registration return the same child.

The settings milestone adds `ReviewConfig.count: NonZeroUsize` with a single default helper returning two. `ReviewConfigInput` reads it; `SessionReview::Turn` and `::Push` gain optional `count` fields, and `ReviewConfig::for_session` applies explicit values. The final configuration version is 21, after upstream used 20 for daemon placement. Add `--review-count` to both `mj new` and `mj import`, and add a numeric reviewer-count control in the existing global TUI review editor. The read-only viewer review status also exposes the count. Existing omitted fields inherit the configured count, whose default is two. Counts above three remain valid. The group snapshots the effective count by creating its ordered slots once at start.

The orchestration milestone implements independent concurrent children over one captured change. Add typed `ReviewDriverSnapshot` and ordered `ReviewerOutcomeSlot` values in `mj-core/src/review/driver.rs`, and make the existing `TurnReviewSeed` serializable. Each slot retains its role, command id, and optional terminal verdict. `TurnReviewDriver` owns the slot transitions, snapshot, restore, role-scoped failure, duplicate/stale completion rejection, and final aggregate. `ReviewRequest::StartReviewer` and `CloseReviewer` identify their role. Preserve all per-reviewer classified findings text, with a section identifying the actual child session for each; do not apply an aggregate truncation that drops the later reviewers. State clearly that independent reports may overlap.

Persist `TurnReviewState.active_group: Option<ActiveReviewGroup>` in a new database JSON column. It holds the driver snapshot, frozen `ResolvedReviewSettings`, capture scope, and ordered `ReviewerRegistration` values containing role, stable request key, and optional child id. Persist the target, slots, and request keys before registering children, each returned child id before startup, and each terminal outcome before handback acknowledgement. Save one authoritative registration map rather than duplicating a live role-to-child map in the driver. Resume valid active groups after daemon replacement, retry registration with its existing key, and reconcile durable child handbacks before marking them acknowledged. Preserve the existing `PendingForward` command-id reconciliation. Add a classified breaking schema revision after the existing storage/action revisions 104 and 105; include the new session count JSON in its compatibility analysis. Inspect correct daemon/client protocol revision requirements before publishing new fields to strict readers.

When all slots are terminal, all `Clean` outcomes collapse to one `Clean`. Any findings are retained in numbered session-labeled sections. A mixed findings/failure group uses serde-defaulted `ReviewPassEvidence.reviewer_failures` metadata and includes the failure reasons beside the findings. Its forwarding/dismissal must not advance coverage or narrow the following pass. A failure-only group is `Failed`. Push verdict delivery clears stale `push_findings` on incomplete/failed aggregates; only fully completed findings groups can create an overridable record. An admitted audited override still requires the exact captured trees.

The presentation slice adds optional `RoleStatus.child_session_id` for outward projection; the host fills it from its registration owner. It updates question ownership and provides ordered child transcript choices in the terminal and one existing conversation-route link per child on the web. Key pane caches, poll requests, pending responses, and scroll state by the selected child id. Never attach a group's transcript through the legacy scalar child id. A role with no assigned child is status-only. Render the aggregate verdict text verbatim. Existing question ids already contain the owning child; preserve that routing for every reviewer.

The fourth milestone updates documentation and the existing golden/integration regressions. Extend user-facing review output expectations rather than creating a family of classifier examples. Exercise at least a clean group, mixed clean/findings, multiple findings with attribution and redundancy notice, a partial failure retaining findings, out-of-order completion, and cancellation or restart while children are outstanding. Preserve the exact-tree and audited-override regressions from the companion plan.

## Concrete Steps

Work from the repository root and do not change Cargo configuration or `target/`; builds use mbx. Cargo package names have a `brokk-` prefix, for example `brokk-mj-review`, `brokk-mj-core`, `brokk-mj-worker`, and `brokk-mj-controller`. Run every Cargo test outside the restricted sandbox with elevated permissions. Every daemon/CLI/live invocation must use an isolated named instance or its existing isolated test directories.

Keep checkpoint validation focused on the affected module/test filters. Do not run final whole suites until all implementation rounds, including the companion plan, are complete. Run the final dev-profile workspace tests and workspace clippy once, rerunning only failures after fixes. Stage only task-owned changed files, commit to the current branch, and push the completed task to `origin/master`; merge upstream changes if the push is rejected, never rebase.

## Validation and Acceptance

The user can select the number of independent reviewer sessions in existing review settings, sees a default of two, and sees the one-to-three recommendation and diminishing-returns explanation. Both automatic turn review and before-push review honor the setting. All reviewer findings reach the primary with separate session labels and the redundancy notice. A group unlocks the gate as clean only when every child reports the clean sentinel. Failure and recovery tests prove that missing outcomes cannot become a clean verdict.

After the final implementation round run `cargo test --workspace --exclude brokk-mj-desktop` and `cargo clippy --workspace --exclude brokk-mj-desktop --all-targets -- -D warnings` in the dev profile, plus applicable docs/frontend checks. The explicit exclusion is the verified host limitation above. Reuse passing docs validation unless later edits invalidate it. Record exact commands, results, log paths, and checkpoint commits here.

## Idempotence and Recovery

Persisted review groups must retain which child sessions belong to them and their observed outcomes, or use an existing durable ownership source that provides those facts. Never infer successful completion from the absence of a child. Keep shipped persisted and wire formats readable, or add a new classified breaking migration with isolated upgrade regressions. Retrying one child result must not append duplicate findings or approve a group twice. Accepted daemon mutation work remains bounded and admitted through handoff; reviewer turns remain in workers.

## Artifacts and Notes

Research handbacks and validation evidence will be added here as the design is settled.

Initial integrated check: `cargo check --workspace --exclude brokk-mj-desktop --all-targets` passed in the dev profile after cross-slice type/literal fixes. Log: `.mj/agents/15c4b40f651b0c5ef4f6ac34f01339ed/initial-cargo-check-third.log`. It reported two obsolete controller items, scheduled for removal before Clippy. `cargo test -p brokk-mj-review` passed all 29 tests and doc tests; log: `.mj/agents/15c4b40f651b0c5ef4f6ac34f01339ed/checkpoint-review-test.log`. Focused host, configuration, MCP, CLI, presentation, and golden checks are in progress.

`cargo test -p brokk-mj-controller review_host` initially passed 51 tests and failed two: a stale attribution expectation and the failed-group durability gap above. Both failing filters passed after correction; logs `.mj/agents/6a2b350443f8aad357629ebf6f6210ae/clean-failed-group-rerun.log` and `push-verdict-attribution-rerun.log`. Obsolete warning-producing variant/helper were removed. Database filters `active_review_group_survives_interrupted_sweep_with_registered_children` and `old_review_rows_load_without_findings_and_recovery_keeps_new_findings` each passed; logs `.mj/agents/15c4b40f651b0c5ef4f6ac34f01339ed/checkpoint-active-review-group.log` and `checkpoint-old-review-rows.log`.

Focused core/CLI checks passed: `cargo test -p brokk-mj-core config` (120 selected), `cargo test -p brokk-mj-core storage` (7), CLI filters `new_review_flags_choose_the_sessions_own_review` and `an_import_can_choose_the_sessions_review` (one each). The mailbox golden update and ordinary verification each passed; inspected changes preserve both separately attributed findings and incomplete-group instructions. Logs: `.mj/agents/57bbea10c099c4902c60532570967dcb/{core-config-test,core-storage-test,cli-new-review-flags-test,cli-import-review-test,core-mailbox-golden-update,core-mailbox-golden-normal}.log`. The worker MCP filter initially passed 33/34 tests and exposed the 2,048-character combined-instruction limit; the gate text was shortened while preserving its contract (1,938 characters, or 2,037 with the Codex hint). Its failing filter passed on rerun; log `worker-review-gate-rerun.log` in the same report directory.

Focused UI checks passed individually: TUI invalid-count save transition; chat reviewer tabs, per-child questions, final journal barrier; controller effective session count and public status projection. Scoped golden update plus ordinary verification passed for `golden_review_settings`, `golden_turn_review_pane`, and `golden_phone_session_snapshot`; the chat golden was byte-identical. Only review-settings and phone-session-snapshot expected output changed. Logs: `.mj/agents/b24130a99cf76839673ff79da9a2a39e/`, named after each test with `-update`, `-verify`, or `-rerun` suffixes as appropriate. The initial TUI count-transition test passed in tool output without a saved log. Existing web Node and documentation checks remain valid.

Checkpoint `f61b30a62` commits the integrated parallel feature after focused validation and `cargo fmt --all -- --check`. Merge `5a5a2d17b` incorporates upstream `1dfc0a4e1` without conflicts. Final dev-profile workspace tests and Clippy are running against that merged checkpoint, with only the verified desktop dependency exclusion.

The first final workspace test command stopped at the chat library: 419 passed, two ignored, and `golden_transcript_navigation` differed only by the host timezone (18:00 versus the UTC golden's 00:00). The failing filter is being verified with `TZ=UTC`; passing crates will be excluded from the continuation so all unrun suites still execute. The first final Clippy run found a redundant `#[must_use]` on an iterator-returning method; it is being removed before the incremental rerun. Logs: `.mj/agents/15c4b40f651b0c5ef4f6ac34f01339ed/final-workspace-{test,clippy}.log`.

The UTC transcript-filter rerun passed (`tz-utc-transcript-golden.log`). No other test binary had run before that chat failure. Continue with chat doc tests alone and `TZ=UTC cargo test --workspace --exclude brokk-mj-desktop --exclude brokk-mj-chat --no-fail-fast`; this completes unrun crates while retaining the 419 passing chat cases. The incremental Clippy rerun passed the client crate, then exposed eight host warnings: tuple type complexity, collapsible conditions, a bool-then filter-map, and two test lock scopes across await. These are being cleaned up before the final lint rerun; log `final-workspace-clippy-rerun.log` in the same report directory.

Clippy passes after those fixes (`final-workspace-clippy-after-fixes.log`), without suppressions. The continuation executed every remaining target and doc tests; chat has no doc tests. Remaining failures are seven more manually rewound controller migration fixtures, three daemon-startup fixtures with the same retained-column problem, the settings-root nested review golden, and a container credential-check test whose agent-dev image is unavailable. Fix the historical fixture layouts through shared test helpers; production migrations remain unchanged. The container prerequisite is being independently diagnosed. Run only these failing filters after all fixture edits finish, avoiding compilation while sources change. Continuation evidence: `continuation-{workspace-tests,chat-doc-tests}.log` in the validation report directory. The log also contains a nested child invocation of the parked-state regression, so aggregate result-summary counts include that repeated failure.

Final acceptance: all seven controller migration filters and three isolated daemon-startup filters passed after historical fixtures restored the table layouts they claim (16 matching cases, including the concurrent-clients parameterized subprocess cases). Logs are `rerun-<filter>.log` in `.mj/agents/15c4b40f651b0c5ef4f6ac34f01339ed/`. Both `cargo fmt --all -- --check` and `cargo clippy --workspace --exclude brokk-mj-desktop --all-targets -- -D warnings` pass after the final fixture edits; logs `final-rustfmt-after-migrations.log` and `final-workspace-clippy-after-fixture-move.log`.

The settings-root golden was updated only for the reviewer field and guidance, then passed ordinary verification; logs `.mj/agents/b24130a99cf76839673ff79da9a2a39e/golden_settings_root-{update,verify}.log`. The credential-check failure actually used a private fake engine and required no real image. Its single elevated UTC rerun passed in 0.06 seconds; `.mj/agents/03872a5a6e7a0673da18c2aff7e8ffee/codex-container-check-rerun.log`. That indicates a transient full-suite interaction but does not identify its cause. No image was downloaded and no unrelated production code was changed.

Documentation: `npm run check` passed with no errors, warnings, or hints; `npm run build` passed with 27 pages and 2,304 valid internal links. Web: `node --test tests/e2e/web/viewer.unit.test.mjs` passed. The old JSON review parser symbols are absent from review, controller, worker, and core source. The desktop crate remains the sole workspace exclusion because this host lacks GTK/WebKit development libraries.

Delivery checkpoint `c76130124` contains final lint, migration-fixture, and nested-golden corrections. Its first push was rejected by upstream advancement, so merge `d7148993b` incorporates `ec3c0a3cb` and uses combined configuration version 21. Merge checks pass: `TZ=UTC cargo test -p brokk-mj-core config` (120 selected including subprocess cases), the full isolated `daemon_startup` integration binary (17), formatting, and workspace Clippy. Logs `.mj/agents/15c4b40f651b0c5ef4f6ac34f01339ed/merge-{core-config,daemon-startup,rustfmt,workspace-clippy}.log`. Rebuilt docs pass with 27 pages and 2,307 links; logs `.mj/agents/57bbea10c099c4902c60532570967dcb/docs-{check,build}-merged-config21.log`. The final merge-preservation case extends the existing config-file save test before push.

All three ordinary post-merge golden checks pass without expected-output edits: review-settings, settings-root, and the viewer-settings exchange (exact controller filter `settings_routes_hide_literal_secrets_and_save_through_the_shared_model`). Logs `.mj/agents/b24130a99cf76839673ff79da9a2a39e/*-postmerge-verify.log`. The existing `save_review_reloads_latest_config_and_preserves_unrelated_sections` integration test now begins with nondefault daemon runtime/image/mounts and a version-20 file, then checks that review save upgrades to 21, keeps the daemon value, and saves review count 3. Its focused elevated UTC test passes; `.mj/agents/57bbea10c099c4902c60532570967dcb/core-save-review-config21-test.log`.

Final post-extension workspace Clippy and formatting both pass without warnings or errors; `.mj/agents/15c4b40f651b0c5ef4f6ac34f01339ed/last-{workspace-clippy,rustfmt}.log`. No Cargo commands remain active before the final commit and push.

Delivery: `git push origin HEAD:master` succeeded, advancing `origin/master` from `ec3c0a3cb` to `46cf31c63`. Implementation checkpoints are `7fb6becda`, `6d5728d85`, `ed653d5c1`, `8ab292b85`, `f61b30a62`, `c76130124`, and `46cf31c63`; merge commits retain upstream work. This final documentation update records the completed delivery.

## Interfaces and Dependencies

No new workspace crate is needed. The pure driver/core slice owns `mj-core/src/review/{driver,verdict,lanes}.rs` as needed and `mj-review` driver/prompt tests. The host slice owns active-group storage/schema and `mj-controller/src/review_host/**`, preserving the earlier override changes. The settings slice owns core config/session choices, CLI flags, global TUI review settings, and configuration/CLI docs. The presentation slice owns `mj-client` review projections, terminal review panes and attachments, viewer role/count projection, web review view, and its goldens. Coordinate explicit struct literals and exhaustive matches between those owners; do not let concurrent writers edit shared files.

The agreed core snapshot is `ReviewDriverSnapshot { review_id: String, seed: TurnReviewSeed, phase: TurnReviewPhase, last_verdict: Option<ReviewVerdict>, deltas: Vec<RepoDelta>, roles: Vec<ReviewerOutcomeSlot>, provenance: FindingsProvenance, sequence: u64, replay_requests: Vec<ReviewRequest> }`. Make `TurnReviewSeed`, its user-message values, and `ReviewRequest` serializable. Each `ReviewerOutcomeSlot` has `role`, `command_id`, and `outcome: Option<ReviewVerdict>`; its position is the frozen launch order. Derive awaited commands, started roles, display status, and count from those owned slots instead of persisting parallel flags. `ReviewPassEvidence.reviewer_failures` is a default-empty vector of `ReviewerFailure { label, reason }`.

The only new-review constructor is `TurnReviewDriver::start(seed, review_id: String, count: NonZeroUsize)`. Provide `snapshot() -> ReviewDriverSnapshot`, `restore(snapshot) -> Result<Self, String>`, `role_turn_completed(command_id, child_session_id, answer)`, and `role_failed(role, child_session_id: Option<&str>, reason)`. Returned work remains `Vec<ReviewRequest>`. The host validates child membership before passing its id into completion. The driver embeds historical attribution in terminal report text but does not own a second live identity map. Keep `resume_forward(seed, pending)` solely for historical durable primary-handoff records. Mixed-failure forwarding preserves the existing baseline and prior-review context; it must not replace them with this incomplete group's result.

The terminal/browser projection adds an optional child id to each role row. Bump daemon/client `mj_client::daemon::PROTOCOL_VERSION` from 70 to 71 because old strict role readers reject that populated field. Do not bump the worker relay protocol for a client-only projection. The presentation slice owns that constant and its tests. Reuse isolated daemon startup/handoff regressions in `mj-cli/tests/daemon_startup.rs`, including concurrent obsolete-daemon replacement, automatic handoff/migration, and incompatible-store downgrade refusal. The host's `question_target` must check the current group registrations rather than the legacy scalar child id.

Provide canonical `ReviewVerdict::is_complete()` beside `is_clean()`: clean is complete, failed is incomplete, and findings are complete exactly when `reviewer_failures` is empty. The driver owns coverage decisions, including restored `PendingForward` acceptance; the host must not duplicate those decisions with defensive predicates. Host push/override eligibility uses the canonical completeness helper. Change `TurnReviewHost::question_target` to an asynchronous `HostEvent::QuestionTarget` request that checks authoritative registrations; add `.await` in its existing daemon/actions and server_runtime/actions call sites.

Plan note (2026-11-03): created to capture the user's added parallel-review requirements while preserving the already-authorized plaintext/override implementation and its checkpoint commits.

Plan note (2026-11-03): recorded the discovery results, positive count setting and inheritance, typed driver/group recovery model, mixed failure policy, session-attributed output, and terminal/web ownership. Busy reviewer cancellation by timeout is explicitly excluded.

Plan note (2026-11-03): fixed exact driver snapshot and callback interfaces and recorded the required daemon/client protocol 71 boundary and isolated upgrade acceptance checks.

Plan note (2026-11-03): centralized completeness semantics in the verdict type, retained the driver as the only coverage decision owner, and selected actor-owned asynchronous reviewer-question routing.

Plan note (2026-11-03): recorded driver-owned durable emitted-work replay, stable push-verdict identity, the parent takeover of incomplete count edits, current implementation progress, and the verified desktop build exclusion.
