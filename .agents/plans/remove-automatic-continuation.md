# Remove automatic continuation

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` (repository root) and must be maintained in accordance with that file. Tracking issue: #1532.

## Purpose / Big Picture

Mjolnir ("mj") runs coding agents in sessions. "Automatic continuation" was a feature where, after an agent ended its turn with work the user had asked for still unfinished, mj itself sent the agent a fixed "continue" prompt, up to three times per user message, instead of waiting for the person to type "continue". A classifier decided when to do this. Two studies (2026-11-12 and 2026-11-13) showed no classifier rule reaches the required error rate (at most 1% wrong continuations): most bare "continue" replies people type are waits on background work, and the known-good positives are too few to certify any rule. The feature has been held off for current workers since 2026-11-12. The user decided to remove it.

After this change, mj never sends an automatic "continue" prompt for ordinary unfinished work, the `[continuation]` configuration section and its settings page are gone (an existing `[continuation]` block in a user's config file is still accepted and ignored), and the code that only existed for the feature is deleted. Everything that merely shares the word "continuation" keeps working: resuming a session after its provider quota resets ("quota recovery"), server and authentication retries, goal mode, handback reminders and mailbox wakes. To see it working: in an isolated instance, a config file containing `[continuation]\nenabled = false` still loads; the settings page (TUI and web) has no Continuation entry; a session that hits a quota limit still resumes after the reset.

## Progress

- [x] (2026-11-13) Milestone 1: worker and core: remove the GLiDE Continue switch, the worker `continuation_enabled` flag and its environment variable, keep legacy decoding.
- [x] (2026-11-13) Milestone 2: daemon (also removed `ContinuationConfig` and the `Config.continuation` field with the legacy `[continuation]` decoder, because core could not compile otherwise): remove the automatic-continuation decider and submit path; keep quota recovery, the seed job, the update feed and the review observer.
- [x] (2026-11-13) Milestone 3: configuration, settings UI, goldens, docs, shipped skill, e2e scripts, AGENTS.md.
- [ ] Milestone 4: proxy: keep serving the old routes, move their question files into the proxy package.
- [ ] Full validation, commit, review, push; close #1304 and #1532.

## Surprises & Discoveries

- Observation: `mj-controller/src/daemon/continuation.rs` (about 1,235 lines) is not only automatic continuation. It also owns quota recovery, the "seed authorization" job, the update feed that every session update passes through, and the review observer. It cannot be deleted; only its automatic-continuation branches go.
  Evidence: reconnaissance map `.mj/agents/4dad49d8c7e779ee135dfb0efba16a3d/MAP.md` §2.2 (not checked in; facts repeated below).
- Observation: the `[continuation] enabled` switch also gates quota recovery (`claim()` in `daemon/continuation.rs` ~402-424 calls `Config::automatic_continuation_enabled()`, which is `continuation.enabled && jev.enabled`).
- Observation: Cargo package names carry a `brokk-` prefix (`cargo test -p brokk-mj-worker`).
- Observation: the worker's live `ContinueAuthorizedWork` handler was the only thing the quota tests used to raise `attempts`; those tests were reduced to their quota assertions (`attempts` now stays 0 live and is only set by replaying old journals). `generated_prompts_neither_renew_allowance_nor_grant_authorization` was kept, trimmed and renamed `generated_prompts_do_not_grant_authorization`, because its Hard-won marker (8b08e8bc1: handback reminders and peer messages must not count as user authorization) still applies to quota recovery's `user_command_id`.
- Observation: the worker flag `continuation_enabled` also gates the sub-agent handback check (`mj-worker/src/relay/verdict.rs` ~2281).

## Decision Log

- Decision: quota recovery stays and is gated only by `[jev] enabled` (the Privacy switch).
  Rationale: quota detection for current workers comes from the worker's GLiDE turn classifier, which `[jev] enabled` already controls. Removing `[continuation]` would otherwise remove quota recovery's only switch; the classifier switch is the one that decides whether evidence leaves the machine, so it is the right owner. A user who had `[continuation] enabled = false` gains quota recovery; this is reported in the change summary.
  Date/Author: 2026-11-13, parent session.
- Decision: drop the daemon-side Jev continuation classifier entirely, including the quota detection it provided for workers on relay protocol 18 to 24.
  Rationale: those workers predate the worker-side assessment (protocol 25). Workers are replaced with the installed build when idle, so such workers only exist briefly after an upgrade; keeping a second classifier path for them is the kind of parallel mechanism the user rejects. Those workers lose automatic quota recovery until they are replaced.
  Date/Author: 2026-11-13, parent session.
- Decision: a completed turn for which the frozen GLiDE rule would choose Continue keeps returning `Action::Uncertain`.
  Rationale: that is today's measured behavior (`ENABLE_AUTOMATIC_CONTINUE = false` makes the rule abstain), and the published calibration (`frozen_fresh_test_probabilities_match_research_with_continue_abstention`) was fitted with it. Letting those turns fall through to Finished or Wait would change attention glyphs without evidence. The rule is kept as a plain abstention ("authorized unfinished work: abstain"), with the switch constant and the `Action::Continue` output removed. The GLiDE questions file and proxy route stay unchanged.
  Date/Author: 2026-11-13, parent session.
- Decision: keep every persisted shape decodable: `RelayCommand::ContinueAuthorizedWork` (with its journal replay), `GeneratedPrompt::AutoContinue` (`auto-continue-` prefix), `assessment::Action::Continue`, the `Status::Deferred` reason `authorized_continuation_ready`, `ContinuationState.attempts` and `.suppressed` (still serialized), the transcript projection line for old `continue_authorized_work` events, and the `ActivityState` wire value `checking-continuation`. No relay protocol bump, no database migration.
  Rationale: worker journals and the daemon database already contain these values, and older daemons or workers read snapshots written by newer ones. The precedent is `RelayCommand::SetSubagentWait`, a legacy command "decoded and replayed without changing state" (`mj-core/src/relay/snapshot.rs` ~68-73).
  Date/Author: 2026-11-13, parent session.
- Decision: rename the Rust variant `ActivityState::CheckingContinuation` to `CheckingQuota`, keeping `#[serde(rename = "checking-continuation")]`, and change its user-visible label from "Checking continuation" to "Checking quota".
  Rationale: after removal the daemon publishes this state only while it checks or waits on quota recovery; the wire value stays for compatibility.
  Date/Author: 2026-11-13, parent session.
- Decision: the sub-agent handback check is gated only by `[jev] enabled` (`MJ_JEV_DISABLED` in the worker); the worker field `continuation_enabled`, `mj_core::jev::CONTINUATION_DISABLED_ENVIRONMENT` and `apply_continuation_switch` are removed.
  Rationale: the handback check is a GLiDE classifier call; the Privacy switch owns classifier calls.
  Date/Author: 2026-11-13, parent session.
- Decision: the proxy keeps serving `/v1/continuation-verdict` and `/v2/continuation-verdict` for installed older releases; their question JSON files move from `mj-core/src/continuation/` into `services/jev-proxy/`.
  Rationale: the hosted proxy is shared by every installed mj release, including public ones. The repository's precedent is to keep old routes (turn-verdict `/v1` to `/v7` are all still served). Route retirement is a separate, later decision.
  Date/Author: 2026-11-13, parent session.
- Decision: the dead v7 Jev turn-verdict client (`ask_logged`, `Verdict::action`, `apply_turn_assessment`, the `mj-core/tests/jev-scenarios` suite) is out of scope here; the Jev-to-GLiDE plan `.agents/plans/afk-and-locator-on-glide.md` retires it.
  Rationale: it is part of retiring the Jev client, not of automatic continuation, and the scenario fixtures may feed the question-locator evaluation.
  Date/Author: 2026-11-13, parent session.
- Decision: the daemon module keeps its name `daemon/continuation.rs`.
  Rationale: quota recovery is also a continuation; renaming a 1,200-line module adds churn without changing behavior.
  Date/Author: 2026-11-13, parent session.

- Decision (revises the abstention decision above): the GLiDE rule's Continue branch and `CONTINUE_FLOOR` are deleted outright, not kept as an explicit abstention, and `eot_glide::action` loses its `authorization_complete` parameter. The frozen-sample test is renamed `frozen_fresh_test_probabilities_match_research`.
  Rationale: output-equivalent. When the branch's condition held, `turn_state.continue` >= .90 forces `turn_state.finished` <= .10 (< `FINISHED_CATEGORICAL` .916) and `work.authorized_unfinished` >= .90 forces `work.waiting` <= .10 (< `WAITING_WORK_WINNER` .40), so the function already fell through to the final `Action::Uncertain`.
  Date/Author: 2026-11-13, parent session.
- Decision: the daemon's quota assessment path stays. The admission formerly labelled "automatic continuation" is relabelled "quota assessment"; the unified-worker branch that turns a worker's `Action::RecoverQuota` assessment into quota resolution stays, and so does whatever minimal quota-only value it needs in place of `ContinuationVerdict`. Only the Continue parts and the legacy non-unified Jev classify branch go. A quota test must cover a worker `RecoverQuota` assessment leading to a scheduled recovery.
  Date/Author: 2026-11-13, parent session.

- Decision (milestone 2 deviations, implementer): `legacy_quiet` stays in the daemon because `quota_schedulable` and `quota_resume_blockers` still use it for workers older than relay protocol 23, so a stored recovery on such a worker can still be resumed; only automatic-continuation uses went. `incomplete_and_oversized_evidence_is_rejected` in `mj-core/src/continuation.rs` stays (minus its `quota_message` field), because it is the only test of `ContinuationEvidence::validate` limits, which the GLiDE and handback paths still use. The three `mj-controller` evidence tests (authorization across exchanges, newest user window, generated input excluded) moved to `mj-transcript/src/authorization.rs`, which owns `evidence()`, instead of being deleted. `Phase::Checking` was renamed `Phase::Resolving` (it now only resolves a quota reset deadline). A quota assessment no longer goes through a classify task: admission spawns the quota preparation directly.
  Date/Author: 2026-11-13, implementer.
- Decision: `a_self_started_turn_that_hits_the_quota_schedules_recovery_against_itself` now asserts that workers on protocol 24 and 22 are not scheduled (the accepted loss of legacy-worker quota recovery) and that a protocol-25 worker's `RecoverQuota` assessment schedules the recovery; `quota_owner_wait_rechecks_identity_and_turn_without_holding_handoff` and `handoff_defers_a_quota_assessment_without_consuming_its_trigger` drive the same worker-assessment path, which satisfies the parent's requirement that a test cover a worker `RecoverQuota` assessment leading to a scheduled recovery.
  Date/Author: 2026-11-13, implementer.

- Decision (milestone 3, implementer): the settings golden exchange and the web settings e2e used the Continuation page as the page that is opened, edited and kept across a notification navigation (Hard-won 7a50f9da7). They now use the Web Access (`phone`) page and its `Enabled` toggle, because the exchange must still contain one page edit. `scripts/jev-continuation-eval.py` was deleted. The Privacy help now reads "Sends turn text to a hosted classifier; quota recovery needs it. Off stops both; sessions follow on resume or restart." (it has to fit two rows, and tests require the words "resume or restart"). `docs/src/content/docs/sessions.md` has a "Quota recovery" section in place of "Automatic continuation".
  Date/Author: 2026-11-13, implementer.

## Outcomes & Retrospective

(Fill in at completion.)

## Context and Orientation

The repository is a Rust workspace plus a TypeScript Cloudflare Worker. Terms used below:

The daemon (`mj daemon-run`, crate `mj-controller`) is the control plane: it owns the database, session records and the HTTP API. A worker (crate `mj-worker`, one per session) runs the coding agent ("harness") and owns a journal of relay commands. A relay command (`mj_core::relay::snapshot::RelayCommand`) is a message from the daemon to a worker, such as "submit this prompt"; workers journal them and replay the journal after restarts. `mj-core` holds shared types. `mj-client`, `mj-tui` and `mj-chat` are the terminal and settings clients; the web viewer's settings page is generated from the same schema in `mj-client/src/settings/schema.rs`. The proxy (`services/jev-proxy`) is the hosted HTTP service that forwards classifier requests either to Jev (the TypeSafe classifier, model `jev-latest`) or to GLiDE (the Fastino classifier).

How automatic continuation works today. After each completed turn, the daemon's continuation service (`mj-controller/src/daemon/continuation.rs`) decides whether to continue. For workers on relay protocol 25 or newer, the worker's own turn assessment decides: the worker asks GLiDE (`/v1/glide-turn-verdict`), and the frozen rule in `mj-core/src/activity/eot_glide.rs` (`fn action`) can return `Action::Continue`; but `ENABLE_AUTOMATIC_CONTINUE = false` (line ~35) turns that into `Action::Uncertain`, so current workers never ask for a continuation. For workers on protocol 18 to 24, the daemon itself asks Jev (`mj-controller/src/continuation.rs`, `classify`, POST `/v2/continuation-verdict`). When the decision is Continue and the switches allow it, the daemon submits `RelayCommand::ContinueAuthorizedWork` with command id `auto-continue-<ordinal>-<attempt>` (`daemon/continuation.rs` ~1017-1064), at most `MAX_NUDGES = 3` per user message (`mj-core/src/continuation.rs`). The worker accepts it only if it is still current (`mj-worker/src/relay/commands.rs` ~833-868) and parks `Action::Continue` assessments as `Status::Deferred` with reason `authorized_continuation_ready` (`mj-worker/src/relay/verdict.rs` ~509-514).

What shares the code and must keep working. Quota recovery (`daemon/continuation/quota.rs`, `RelayCommand::ResumeAfterQuota`, `GeneratedPrompt::QuotaRetry`, `QuotaRecovery`, the timer loop in `daemon/continuation.rs` ~1151-1190) resumes a session with the fixed prompt `mj_core::continuation::PROMPT` after a quota reset; the same prompt text is used by `auth-retry-` (`mj-worker/src/relay/commands.rs` ~1720) and by Move (`mj-controller/src/controller/move_session.rs` ~2471). The seed job `SeedAssessmentContext` (`daemon/continuation.rs` ~696-722) fills `ContextHistory.authorization_complete`, which the handback check also reads (`mj-worker/src/relay/verdict.rs` ~2327-2333). The update feed (`UpdateFeed`, consumed in `mj-controller/src/daemon/process.rs` ~366, 791, 846, 929) carries every session update through this service, and `publish` (~457-472) notifies the review observer. `mj_transcript::authorization` (evidence for classifiers), the `GeneratedPrompt` table and `is_user_authored_message` in `mj-core/src/continuation.rs`, `harness_turn_id`, and the remaining fields of `ContinuationState` are all used elsewhere. Goal mode (`mj-worker/src/acp/goal.rs`) and `ActivityState::Expecting` (the agent's own scheduled wake) are unrelated.

## Plan of Work

Milestone 1 (core and worker). In `mj-core/src/activity/eot_glide.rs`, delete `ENABLE_AUTOMATIC_CONTINUE` and make the continuation branch return `Action::Uncertain` unconditionally, with a comment saying that authorized unfinished work abstains because automatic continuation was removed (#1532) and the frozen rule gives it precedence over Finished and Wait. Update the doc comment on `REQUIRED_INPUT` that mentions the switch so it says redundant permission asks count as needing the user. Keep `CONTINUE_FLOOR`, the questions file and the abstention test. In `mj-core/src/assessment.rs`, keep `Action::Continue` with a comment "legacy: decoded from stored assessments; no longer produced by the GLiDE rule". In `mj-core/src/jev.rs` remove `CONTINUATION_DISABLED_ENVIRONMENT` and `continuation_disabled_by_environment()`. In `mj-worker`, remove the `continuation_enabled` field (`relay.rs` ~308, 634, 1116-1117 and its payload arm ~815-817, `worker_runtime/unix.rs` ~964, `worker_runtime/unix/dispatch.rs` ~1263), make the handback check (`relay/verdict.rs` ~2281) depend only on the Jev switch, remove the Continue-to-Deferred parking (`relay/verdict.rs` ~509-514; a legacy `Action::Continue` assessment read from an old journal must map to the same outcome as `Uncertain`), and make the live handler for `ContinueAuthorizedWork` in `relay/commands.rs` reject the command as no longer supported while keeping its journal replay in `mj-core/src/relay/snapshot/apply.rs` (~458-462) unchanged so old journals restore `completed_command_id`. Delete the auto-continue tests listed in Validation; keep the quota, goal, handback and legacy-wait tests in `mj-worker/src/relay/continuation_tests.rs`. Rename `ActivityState::CheckingContinuation` to `CheckingQuota` (`mj-core/src/activity.rs` ~227-228, 268, 310) with the serde rename, and follow the rename through `mj-client/src/usage_format.rs`, `mj-tui/src/render/sessions.rs` ~743, `mj-chat/src/chat/status.rs` ~198, `mj-controller/src/server/api/wait.rs` ~395-404 and `server_runtime/snapshot.rs` ~1040-1052; the label becomes "Checking quota".

Milestone 2 (daemon). In `mj-controller/src/daemon/continuation.rs`, remove the `Phase::Checking` classify path, `DeferredReason::Continue`, `continuation_allowed`, `eligible`, `driven`, `driver_name`, `legacy_quiet`, the Continue arm of `check_eligible` (including the non-unified legacy-worker branch ~362-377 and its quota detection), the `Classifier` type, the submit block (~983-1100), `Outcome::Classified`, the diagnostics kind "continuation" and the admission label "automatic continuation" (~782). `claim()` uses a new `Config::quota_recovery_enabled()` (`self.jev.enabled`) in place of `automatic_continuation_enabled()`. In `publish`, the review observer observes every view immediately unless an unsubmitted quota recovery exists (keep the `quota_recovery.is_none_or(|r| r.submitted)` check). Delete `mj-controller/src/continuation.rs` except the two re-exports `evidence` and `evidence_from_items`, which move to their caller (`database/materialized.rs` ~2267 can call `mj_transcript::authorization` directly). Remove `apply_continuation_switch` and its call in `controller/worker_binary/launch.rs` (~692, 935-946) and its test in `worker_binary/tests.rs` (~3705-3710). In `mj-core/src/continuation.rs` delete `MAX_NUDGES`, `ContinuationConfig`, `ContinuationVerdict`, `CONFIDENCE`, `QUESTIONS`, `ContinuationEvidence.quota_message` and its validation branch, `eligible()`, and the two tests named in Validation; keep `attempts` and `suppressed` serialized with a "legacy, always written" comment. Delete the auto-continue tests in `daemon/continuation/tests.rs` at ~97, 230, 333 and 1052, and reduce the mixed test at ~1291 (`handoff_defers_a_completion_without_consuming_its_trigger`) to its quota part, or delete it if nothing quota-specific remains.

Milestone 3 (configuration and surfaces). In `mj-core/src/config.rs`, replace the `continuation` field in `StoredConfig` with `#[serde(default, skip_serializing)] continuation: LegacyContinuationConfig`, a private struct with `#[serde(default, deny_unknown_fields)]` and the one field `enabled: bool`, modelled on `LegacyMailboxConfig`; remove the field from `Config`, its conversions, and `automatic_continuation_enabled()`; fix the doc comment on `JevConfig` (~795-797) and the version comments that mention continuation. Do not bump `CONFIG_VERSION`. In `mj-client/src/settings/schema.rs` and `settings/page.rs`, remove the continuation section, its summary, help text and "Off because Jev is off" note; describe quota recovery under the Privacy/Jev help if it needs a home. Regenerate the goldens `mj-controller/tests/golden/viewer-settings-exchange.txt` and `tests/e2e/web/golden/viewer_settings.txt`, and update `tests/e2e/web/settings.spec.js` (~142-158), `mj-tui/src/setup/tests/rendering.rs`, and `mj-controller/src/server/tests.rs` (~5064-5068). Remove the `[continuation]` lines from `tests/e2e/parent_report_notifications.py` and `tests/e2e/review_before_push.py`; keep their assertions that no `auto-continue-` command is sent. Update user docs `docs/src/content/docs/sessions.md` (remove the "Automatic continuation" section but keep its quota paragraph under a quota heading), `docs/src/content/docs/security.md` (~24, 36), the shipped skill `mj-core/assets/skills/mj/references/configuration.md`, `.agents/docs/jev.md`, `.agents/docs/jev-proxy.md`, and `AGENTS.md` (the handoff paragraph lists "automatic continuations" as daemon-owned work; replace with "quota recovery"). Delete `scripts/jev-continuation-eval.py`. Keep `scripts/jev-scenarios-extract.py`'s `auto-continue-` prefix (historical transcripts) and its pinned test.

Milestone 4 (proxy). Move `mj-core/src/continuation/questions.json` and `questions_v1.json` to `services/jev-proxy/src/continuation-questions*.json` and update the imports in `services/jev-proxy/src/continuation.ts`. Leave the routes, `CONTINUATION_RATE_LIMITER` and the proxy tests in place. No deployment is needed for this milestone because the served behavior does not change.

## Concrete Steps

Work in the repository root. After each milestone run only the tests of the crates it touched, for example:

    cargo test -p mj-core
    cargo test -p mj-worker relay::
    cargo test -p mj-controller daemon::continuation
    cd services/jev-proxy && npm ci && npm run check && npm test

Run every `cargo test` outside the restricted sandbox. Regenerate goldens with the repository's existing golden update mechanism (search the golden test for its update environment variable) and review every changed golden line: only continuation entries should disappear and quota wording change.

## Validation and Acceptance

Tests to delete (auto-continue only): in `mj-controller/src/daemon/continuation/tests.rs` `continuation_checks_run_concurrently_and_defer_review_until_each_settles`, `continuation_rechecks_disconnected_attachment_once_and_new_input_cancels_it`, `continuation_submits_a_guarded_prompt_and_reviews_only_after_the_chain`, `background_work_defers_continuation_and_its_end_brings_a_fresh_check`; in `mj-worker/src/relay/continuation_tests.rs` the tests at ~61, 109, 151 and 561 (`background_work_holds_continuation_back...`); `a_continue_verdict_is_not_parked_when_nobody_will_act_on_it` in `mj-worker/src/relay/verdict.rs` (~2163, its subject is removed); in `mj-core/src/continuation.rs` `confident_quota_preempts_ordinary_continuation_and_invalid_scores_fail_closed` and `incomplete_and_oversized_evidence_is_rejected`; `mj-controller/src/continuation.rs` tests; `mj-core/src/config/tests.rs` ~828 if it tests only the switch.

Tests to keep and expect passing: all quota tests in `daemon/continuation/tests.rs` (~527, 626, 770, 886, 1180, 1391, 1415, 1582, 1947, 1994, 2150), the worker quota/goal/handback/legacy-wait tests (`continuation_tests.rs` ~213-838 including `legacy_subagent_wait_journal_command_is_a_noop_after_replay`), `frozen_fresh_test_probabilities_match_research_with_continue_abstention`, and the GLiDE abstention test in `mj-worker/src/relay/verdict.rs` ~684.

New test (integration with config files, one only): extend the existing config test that loads legacy keys (search `config/tests.rs` for `mailbox` or `build_cache`) so a stored config containing `[continuation]\nenabled = false` loads, and saving it writes no `[continuation]` section. If a journal-replay test for `continue_authorized_work` does not exist, add one beside `legacy_subagent_wait_journal_command_is_a_noop_after_replay` that replays an old journal containing a `continue_authorized_work` command and checks the restored state; this protects journal compatibility, which nothing else would catch.

Behavioral acceptance in an isolated instance (never the default instance): build, start `mj --instance continuation-removal` with an `MJ_CONFIG_DIR` whose config contains `[continuation]\nenabled = false`; the daemon starts and `mj --instance continuation-removal settings` (or the TUI settings page) shows no Continuation entry. A web settings golden run shows the same.

Final validation, once after the last milestone: `cargo clippy --all-targets -- -D warnings` for the workspace, and the full `cargo test` suites of every touched crate (`mj-core`, `mj-controller`, `mj-worker`, `mj-client`, `mj-tui`, `mj-chat`, `mj-transcript` if touched), the proxy `npm test`, and the web settings e2e test.

## Idempotence and Recovery

All steps are source edits; rerunning them is safe. No database migration and no protocol bump are involved, so an older daemon can still read everything a newer one writes. If a golden regeneration shows unexpected differences outside settings, stop and investigate rather than accepting them.

## Artifacts and Notes

The reconnaissance map used to write this plan is `.mj/agents/4dad49d8c7e779ee135dfb0efba16a3d/MAP.md` (local, not checked in). The Continue study evidence is in `/mnt/optane/tmp/eot-continue-study-20261112` (local).

## Interfaces and Dependencies

In `mj-core/src/config.rs`, `Config` gains:

    /// Quota recovery resumes a session after a provider quota resets. It
    /// depends on the turn classifier, so the Privacy switch owns it.
    #[must_use]
    pub fn quota_recovery_enabled(&self) -> bool { self.jev.enabled }

and `automatic_continuation_enabled` is removed. `ActivityState::CheckingQuota` replaces `CheckingContinuation` with `#[serde(rename = "checking-continuation")]`. No new dependencies.
