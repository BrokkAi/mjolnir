# Run configurable parallel reviewers and combine their verdicts

This ExecPlan is a living document maintained under `.agents/PLANS.md`. Keep `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` current. It extends `.agents/plans/plain-text-review-verdicts-and-override.md`, whose initial implementation is in progress in this same checkout.

## Purpose / Big Picture

The user wants independent reviewer sessions to inspect the same captured change in parallel. A saved review setting defaults to two reviewers. Explain in settings and human documentation that one to three reviewers are reasonable and that more reviewers have diminishing returns. More than three is advice, not by itself an invalid choice.

The primary agent must receive every reviewer's findings, labeled with the separate reviewer session that produced them, and be told that findings may overlap. Do not deduplicate or synthesize away an independent finding. When every reviewer says `No findings.`, one combined clean verdict is enough. The pre-push gate may treat a group as clean only when every requested reviewer completed cleanly; a missing, failed, cancelled, or still-running reviewer cannot count as clean.

The current plaintext contract remains: the exact trimmed ASCII-case-insensitive `No findings.` sentinel is clean, an empty answer is a failure, and other text is findings. An explicit audited override remains a separate user-visible decision from a clean review.

## Progress

- [x] (2026-11-03) User requested parallel reviews during the plaintext/override work; parent stated the assumption that the saved count applies to both turn and push reviews.
- [x] (2026-11-03) Mapped driver, host, durable registration, configuration/session/CLI, terminal/web presentation, and restart assumptions; approved the typed snapshot and per-role registration design below.
- [ ] Add the saved count, default two, to every existing review setting surface and document the one-to-three guidance.
- [ ] Launch independent reviewer children concurrently and combine outcomes without discarding findings or approving an incomplete group.
- [ ] Cover group ordering, partial failures, cancellation/recovery, attribution, and push-gate behavior at existing integration/golden surfaces.
- [ ] Complete final validation, commit coherent checkpoints on the current branch, and push to `origin/master`.

## Surprises & Discoveries

The current review host has one `ReviewSlot` per primary, one configured reviewer identity, and a scalar reviewer child id. The driver currently reaches a verdict on its first completion. Existing startup clears active markers and closes orphan reviewer children; new groups need durable snapshots and reattachment instead. The terminal caches only one reviewer pane, and web links to one scalar child. Each of these assumptions needs an explicit per-role representation.

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

## Outcomes & Retrospective

Implementation has not started. Initial research and completion of the existing plaintext/override checkpoints are underway.

## Context and Orientation

Mjolnir is a Rust workspace. `mj-core` defines configuration, persisted session and review data, and the worker/daemon wire formats. The controller daemon in `mj-controller` owns lifecycle operations and the review host. `mj-review` constructs reviewer prompts and classifies plaintext reports. The worker in `mj-worker` exposes the `mj-agents` MCP tools to local and remote sessions. The primary requests push review through `request_review`; automatic turn review starts after an eligible prompt-driven turn finishes. In both cases hidden reviewer children report through `handback(message: String)`.

Start the orchestration investigation in `mj-controller/src/review_host/{host,events,reviewer,reviewer_child,dispatch,resolve}.rs`, `mj-review/src/driver.rs`, and `mj-core/src/storage.rs`. Start the settings investigation at `mj-core/src/config.rs`, `mj-core/src/review/settings.rs`, the existing review settings projection in `mj-client`, CLI review arguments in `mj-cli`, and persisted per-session review configuration in `mj-controller`. Follow existing settings projections to the TUI and web viewer rather than creating a separate configuration path.

The current override work replaces parallel preparation flags with one typed preparation owner, uses an ordered persistence lane, and records an acknowledged audit notice before unlocking reviewed tree refs. The parallel-review extension must retain these ownership and durability guarantees.

## Plan of Work

The discovery milestone is complete. The design separates the driver-owned ordered outcome slots from host-owned child registrations. Both are persisted together in the active review group. Driver snapshots live in `mj-core` so the storage crate does not acquire a dependency on `mj-review`. The driver restores from that typed snapshot. Child request keys are stable before registration, and the existing `(parent, reviewer request_key)` lookup makes repeating a registration return the same child.

The settings milestone adds `ReviewConfig.count: NonZeroUsize` with a single default helper returning two. `ReviewConfigInput` reads it; `SessionReview::Turn` and `::Push` gain optional `count` fields, and `ReviewConfig::for_session` applies explicit values. Bump configuration version 19 to 20 through the existing version-upgrade path. Add `--review-count` to both `mj new` and `mj import`, and add a numeric reviewer-count control in the existing global TUI review editor. The read-only viewer review status also exposes the count. Existing omitted fields inherit the configured count, whose default is two. Counts above three remain valid. The group snapshots the effective count by creating its ordered slots once at start.

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

After the final implementation round run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` in the dev profile, plus applicable docs/frontend checks. Reuse passing docs validation unless later edits invalidate it. Record exact commands, results, log paths, and checkpoint commits here.

## Idempotence and Recovery

Persisted review groups must retain which child sessions belong to them and their observed outcomes, or use an existing durable ownership source that provides those facts. Never infer successful completion from the absence of a child. Keep shipped persisted and wire formats readable, or add a new classified breaking migration with isolated upgrade regressions. Retrying one child result must not append duplicate findings or approve a group twice. Accepted daemon mutation work remains bounded and admitted through handoff; reviewer turns remain in workers.

## Artifacts and Notes

Research handbacks and validation evidence will be added here as the design is settled.

## Interfaces and Dependencies

No new workspace crate is needed. The pure driver/core slice owns `mj-core/src/review/{driver,verdict,lanes}.rs` as needed and `mj-review` driver/prompt tests. The host slice owns active-group storage/schema and `mj-controller/src/review_host/**`, preserving the earlier override changes. The settings slice owns core config/session choices, CLI flags, global TUI review settings, and configuration/CLI docs. The presentation slice owns `mj-client` review projections, terminal review panes and attachments, viewer role/count projection, web review view, and its goldens. Coordinate explicit struct literals and exhaustive matches between those owners; do not let concurrent writers edit shared files.

The agreed core snapshot is `ReviewDriverSnapshot { review_id: String, seed: TurnReviewSeed, phase: TurnReviewPhase, last_verdict: Option<ReviewVerdict>, deltas: Vec<RepoDelta>, roles: Vec<ReviewerOutcomeSlot>, provenance: FindingsProvenance, sequence: u64 }`. Make `TurnReviewSeed` and its user-message values serializable. Each `ReviewerOutcomeSlot` has `role`, `command_id`, and `outcome: Option<ReviewVerdict>`; its position is the frozen launch order. Derive awaited commands, started roles, display status, and count from those owned slots instead of persisting parallel flags. `ReviewPassEvidence.reviewer_failures` is a default-empty vector of `ReviewerFailure { label, reason }`.

The only new-review constructor is `TurnReviewDriver::start(seed, review_id: String, count: NonZeroUsize)`. Provide `snapshot() -> ReviewDriverSnapshot`, `restore(snapshot) -> Result<Self, String>`, `role_turn_completed(command_id, child_session_id, answer)`, and `role_failed(role, child_session_id: Option<&str>, reason)`. Returned work remains `Vec<ReviewRequest>`. The host validates child membership before passing its id into completion. The driver embeds historical attribution in terminal report text but does not own a second live identity map. Keep `resume_forward(seed, pending)` solely for historical durable primary-handoff records. Mixed-failure forwarding preserves the existing baseline and prior-review context; it must not replace them with this incomplete group's result.

The terminal/browser projection adds an optional child id to each role row. Bump daemon/client `mj_client::daemon::PROTOCOL_VERSION` from 70 to 71 because old strict role readers reject that populated field. Do not bump the worker relay protocol for a client-only projection. The presentation slice owns that constant and its tests. Reuse isolated daemon startup/handoff regressions in `mj-cli/tests/daemon_startup.rs`, including concurrent obsolete-daemon replacement, automatic handoff/migration, and incompatible-store downgrade refusal. The host's `question_target` must check the current group registrations rather than the legacy scalar child id.

Provide canonical `ReviewVerdict::is_complete()` beside `is_clean()`: clean is complete, failed is incomplete, and findings are complete exactly when `reviewer_failures` is empty. The driver owns coverage decisions, including restored `PendingForward` acceptance; the host must not duplicate those decisions with defensive predicates. Host push/override eligibility uses the canonical completeness helper. Change `TurnReviewHost::question_target` to an asynchronous `HostEvent::QuestionTarget` request that checks authoritative registrations; add `.await` in its existing daemon/actions and server_runtime/actions call sites.

Plan note (2026-11-03): created to capture the user's added parallel-review requirements while preserving the already-authorized plaintext/override implementation and its checkpoint commits.

Plan note (2026-11-03): recorded the discovery results, positive count setting and inheritance, typed driver/group recovery model, mixed failure policy, session-attributed output, and terminal/web ownership. Busy reviewer cancellation by timeout is explicitly excluded.

Plan note (2026-11-03): fixed exact driver snapshot and callback interfaces and recorded the required daemon/client protocol 71 boundary and isolated upgrade acceptance checks.

Plan note (2026-11-03): centralized completeness semantics in the verdict type, retained the driver as the only coverage decision owner, and selected actor-owned asynchronous reviewer-question routing.
