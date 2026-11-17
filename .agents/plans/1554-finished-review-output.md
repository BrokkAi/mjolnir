# Read a finished review after its reviewer sessions are destroyed

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md` in the repository root.

## Purpose / Big Picture

A person or an evaluation script must be able to read what each reviewer of a finished review reported, even though Mjolnir destroys a reviewer's session as soon as its role ends. Before this change the only durable copy of a reviewer's findings was `subagent_handbacks.handback_message`, which the next full state save prunes with the reviewer relation; the daemon exposed only the currently open review.

After this change, `mj review show --session <parent> [--review <id>]` and `GET /api/v1/sessions/{parent}/reviews` answer with the finished review's per-role output: profile, model, effort, full handback text, verdict, resolution, and the reviewer turn with its token usage. It keeps working after the reviewer child session, its relation, and its handback row are gone. A reviewer turn also records the effort it was launched with in `session_turn_selections`, which existing usage reports read.

A person can see it work by running a review, letting it finish, and running `mj review show --session <id> --json`; the `roles` array still names the reviewer model, effort, handback, and usage even when `mj sessions --session <reviewer>` no longer finds the reviewer.

## Progress

- [x] (2026-11-17T02:31Z) Add `review_reports` schema revision 134 and `FinishedReviewResolution`.
- [x] (2026-11-17T02:31Z) Snapshot each role's settings, handback, and verdict on every review-state save; finalize the rows with an explicit resolution when the review closes.
- [x] (2026-11-17T02:31Z) Add the database read model `FinishedReview`/`FinishedReviewRole`, including the retained reviewer turn.
- [x] (2026-11-17T02:31Z) Add `GET /api/v1/sessions/{id}/reviews[?review=<id>]`, `ApiClient::finished_review`, and `mj review show`.
- [x] (2026-11-17T02:31Z) Fall back to the registered child's model and effort when recording `session_turn_selections`.
- [x] (2026-11-17T02:31Z) Add the database survival test, the API regression test, the CLI parse extension, and the effort regression test.
- [x] (2026-11-17T02:31Z) Update the CLI, API, and review documentation.
- [x] (2026-11-17T02:31Z) Run workspace clippy and the full suites of the touched crates. `brokk-mj-core` (659) and `brokk-mjolnir` (all targets) pass; the full `brokk-mj-controller` suite reports 2675 passed and four failures that reproduce alone on this host and are outside the touched code (a root-readability permission fixture and three quota-based spawn-selection tests).
- [x] (2026-11-17T04:05Z) Review round 1: reviewers found that destroyed parent sessions left their reports behind. Added the `review_reports_session_destroyed` trigger to migration 134, extended the database test to assert the cleanup, and re-ran the schema reader tests (45), the review-host tests (75), and workspace clippy.
- [x] (2026-11-17T05:20Z) Review round 2: reviewers found the migration left the compatibility floor unchanged although older writers lose review output, and that a stale handback from another command could be retained for a failed role. Raised the floor to 134 (`MINIMUM_COMPATIBLE_VERSION`, `move_close_intent` assertions, `ec2_move_ownership` assertion), and retained a handback only for Clean/Findings outcomes with a regression.
- [x] (2026-11-17T05:40Z) Finalization follows the explicit close flag rather than the presence of an earlier row, and a replaced review is finalized from its own driver. Re-ran the database suite (264), the review-host suite (75), and workspace clippy.
- [x] (2026-11-17T06:30Z) Review round 3: all three reviewers found that the cleared-group branch ignored the host's explicit resolution, which both production closes take. Restored it and added a regression for a review cancelled after its verdict was recorded. Re-ran `database::tests` (143), the schema reader tests (45), the review-host tests (75), and workspace clippy.
- [ ] Commit and push.

## Surprises & Discoveries

- Observation: four unrelated controller tests fail in this development container.
  Evidence: `stored_profile_content_that_cannot_be_copied_fails_the_upload` chmods a file to 000, which root can still read; `spawn_runs_the_child_on_the_profile_with_the_most_quota_that_offers_the_model`, `spawn_refuses_a_profile_whose_login_is_known_to_be_refused`, and `a_profile_that_cannot_be_discovered_is_left_out_not_fatal` fail on their quota assertions. None touches the review, usage-selection, schema, or API files changed here.

- Observation: the CLI PTY suite is not hermetic against this container's own agent environment.
  Evidence: with `DEEPSEEK_API_KEY` exported, the dashboard detects a `dsh` profile and the startup goldens do not match; running the suite with that variable and the other harness home variables removed passes all 14 PTY tests. The `instance` fixture now also removes those variables and installs the fake Claude command only on the run that expects it.

- Observation: `MaterializedTurnOutcome` is stored in `session_turn_usage` in a wrapper with a `raw_outcome` field, and `session_turn_usage` outlives the child's `sessions` row.
  Evidence: `mj-controller/src/database/turn_outcome.rs` and the pre-existing usage tests that delete the session and still read its accounting.

- Observation: the review-host test `push_review_delivers_each_verdict_without_holding_or_prompting_the_primary` hung in its `manualoff` case.
  Evidence: a trace showed cases through `pushfailed`, then `manualoff start` and no capture. Commit `46d7962ea` made a review request a no-op when the session's review is off; the case expected a capture and a reviewer. The stale cases and the now-unused fake `subagent` flag were removed; `manualturn` keeps manual-start coverage with review enabled.

## Decision Log

- Decision: store finished review output in a new standalone `review_reports` table instead of adding a field to `turn_review_state` or `ReviewDriverSnapshot`.
  Rationale: a new table is compatible with older readers and writers, and handback text can be large. A field in the driver snapshot would be dropped by an older daemon that rewrote the review state during a downgrade, which the store-compatibility rules treat as a breaking change. The review owns its output; the reviewer child is transient.

- Decision: raise the store's compatibility floor to 134.
  Rationale: a revision-133 daemon can settle reviewers, clear the active group, and prune their handbacks without writing `review_reports`, which loses that review's output irrecoverably. AGENTS.md treats preservation of new data by older updates as part of compatibility, so this is a breaking change even though the table itself is standalone. Round-1 review raised this.

- Decision: snapshot handbacks during normal review-state saves rather than only at close.
  Rationale: the review host destroys each reviewer child as soon as its role settles, and `save_state_to` prunes `subagent_handbacks` whose relation is gone. Reading the handback on every state save copies it while the child is alive. `COALESCE` in the upsert keeps the last non-null text if a later save cannot read it.

- Decision: finalization uses the host's explicit `FinishedReviewResolution`, never the stored group's verdict, whenever the saved state clears the active group.
  Rationale: both production closes clear `active_group` before saving, so that branch is the one they take. A cancelled review's stored group can still carry the verdict it never delivered, and a permanent delivery failure can arrive after a clean verdict was recorded, so re-deriving the outcome from the older snapshot would contradict the host's decision. Inference is left only for a cleared group saved without a decision, which no production path produces. Round-3 review raised this; the database regression for a review cancelled after its verdict covers it.

- Decision: retain a handback only when the host settled the role from one (Clean or Findings).
  Rationale: `subagent_handbacks` keeps a child's newest handback, which can belong to another command. The review host rejects such a report when it decides the role, so copying the text regardless would present unrelated findings beside a failed verdict. A failed, pending, or cancelled role keeps no handback. Round-1 review raised this.

- Decision: a trigger removes a session's reports when its `sessions` row is deleted, rather than relying on callers.
  Rationale: review output belongs to the requesting session, so destroying that session must not leave unreachable handbacks behind. Every operational delete goes through `delete_session_from`, which deletes only the `sessions` row and lets triggers do the rest (as `prompt_history_session_destroyed` and `session_rows_deleted` already do). A trigger therefore covers every deletion path, and a reviewer child's deletion cannot remove its parent's report because the key is the parent's id. Round-1 review raised this gap; the trigger was added to revision 134 while the revision was still unreleased, so the shipped migration is complete rather than followed by a repair.

- Decision: finalization takes an explicit `FinishedReviewResolution` from the review host.
  Rationale: a cancelled review keeps the stored driver snapshot from before cancellation, which can still carry a verdict the review was about to deliver. Inferring cancellation from the snapshot would call that review completed. `close_turn_review_state` writes the resolution and clears the active review in one transaction. The generic `save_turn_review_state` still infers a resolution for test and legacy paths, but every production close uses the explicit value.

- Decision: read the reviewer turn by `session_turn_usage.session_id` ordered by completion, not by matching the driver's role command id.
  Rationale: the reviewer child's prompt command id is allocated by the child startup delivery, not necessarily the review driver's role command id. A reviewer child runs exactly one turn, so the newest turn row is its review turn.

- Decision: use the subagent relation as the fallback for missing model/effort in `record_turn_selection`.
  Rationale: the resolved reviewer settings are already the authority for a reviewer's launch. Some harnesses' projected configuration can carry the model without the effort the worker applied, which is the defect in the issue. The relation is present while the reviewer runs.

- Decision: remove the stale `manualoff`/`manualchild` cases from the review-host verdict test.
  Rationale: the review-off no-op is the intended current behavior and is covered by `a_review_request_with_review_off_records_the_tree_and_chooses_no_reviewer`. Leaving the cases made the test wait for a capture that can never arrive.

## Outcomes & Retrospective

Implemented on 2026-11-17. The finished report is written as each reviewer settles and finalized when the review closes, so the read path does not depend on reviewer sessions. The report is readable through the CLI, JSON API, and database model. Effort is recorded for reviewer turns. Targeted tests pass: the database survival test, the API route test, the effort regression test, the CLI parse test, the 45 schema reader tests, and all 75 review-host tests. Workspace clippy, the full core suite, and the full CLI suite pass; the full controller suite has four unrelated container-baseline failures listed under Surprises. Publishing remains.

## Context and Orientation

- `mj-controller/src/database/turn_reviews.rs` owns the review state read/write path. `save_turn_review_state_in` is called by the review host's persistence lane for every state transition; `close_turn_review_state_in` is the close variant.
- `mj-controller/src/review_host/environment.rs` defines `ReviewEnvironment`, the boundary between the review host and the real controller store.
- `mj-controller/src/review_host/resolve.rs` delivers a completed verdict and clears the active review.
- `mj-controller/src/review_host/roles.rs` handles a cancelled or otherwise closed review.
- `mj-controller/src/review_host/events.rs` runs the ordered persistence lane.
- `mj-controller/src/server/api/subagent_backend.rs` is the API's store boundary; `turns.rs` and `routes.rs` define the HTTP route.
- `mj-cli/src/api_client.rs` and `mj-cli/src/api_commands.rs` define the CLI transport, parser, and text rendering.
- `mj-controller/src/database/usage.rs` records the model, effort, and profile selected when a turn starts.
- `mj-core/src/storage.rs` holds the shared `FinishedReviewResolution` and `TurnReviewState`.

A "reviewer child" is the sub-agent session that runs one reviewing role. A "role" is one reviewer slot such as `reviewer-1`. "Finished" means the review's active marker was cleared; its resolution is `completed`, `failed`, or `cancelled`.

## Plan of Work

In `mj-controller/src/database/schema.rs`, add the compatible migration to schema revision 134 for the `review_reports` table and the `review_reports_session_destroyed` trigger that removes a session's reports with the session row. Add the same `DROP TRIGGER` and `DROP TABLE` to the test downgrade helper in `stamp_schema_version`.

In `mj-controller/src/database/turn_reviews.rs`, add `FinishedReview` and `FinishedReviewRole`. `save_turn_review_state_in` loads the previous active group, syncs report rows for the current or previous group, and finalizes a group that is being replaced or cleared. `close_turn_review_state_in` passes an explicit resolution. `finished_review_in` selects the newest finished review (or the named one), reads its role rows, parses the verdict, and reads the newest `session_turn_usage` row for the child.

In `mj-controller/src/review_host/environment.rs`, add `close_review` with a default that calls `save_state`, and implement it with `close_turn_review_state`. In `events.rs`, carry the resolution on `PersistenceCompletion::Close` and route that completion through `close_review`. In `resolve.rs`, call `close_review` with `Completed` or `Failed` after the verdict is queued. In `roles.rs`, derive the resolution from the driver's phase and last verdict.

In `mj-controller/src/database/usage.rs`, query the `subagent_sessions` relation for the child's model and effort and use them only where the projected configuration has no value.

In `mj-controller/src/server/api/types.rs`, add `FinishedReviewQuery` and `FinishedReviewResponse`; in `subagent_backend.rs`, add the database read; in `turns.rs`, answer the route and return `404` only when a named review is missing; in `routes.rs`, register `GET /sessions/{session_id}/reviews`.

In `mj-cli/src/api_client.rs`, add `finished_review` with an optional `review` query parameter. In `mj-cli/src/api_commands.rs`, add `review show`, extend the parser test, and render each role's settings, verdict, turn usage, and handback.

## Concrete Steps

Run from the repository root:

    cargo test -p brokk-mj-controller a_finished_review_keeps_each_reviewers_output
    cargo test -p brokk-mj-controller reviewer_turn_selections_fall_back
    cargo test -p brokk-mj-controller finished_review_api_returns_reviewer_output
    cargo test -p brokk-mjolnir review_parses_start_and_status
    cargo test -p brokk-mj-controller database::schema::reader_tests::
    cargo test -p brokk-mj-controller review_host::tests::

Each command must report `test result: ok`. The first three each run one new test; the schema group runs 45 and the review-host group runs 75. To exercise the CLI against a live daemon in an isolated instance, run a review and then:

    mj --instance finished-review-test review show --session <parent> --json

The JSON must carry a `review.roles` array whose entries keep `handback`, `model`, `effort`, `profile`, `verdict`, and `turn` after `mj sessions --session <reviewer>` reports that the reviewer is gone. Stop the test daemon with `mj --instance finished-review-test daemon stop` when finished.

## Validation and Acceptance

Acceptance is observable in all three paths:

- The database test builds a two-reviewer review, records handbacks, saves the open state, closes it as completed, deletes both child session rows, clears `subagent_sessions` and `subagent_handbacks`, and still reads both handbacks, both verdicts, both profiles/models/efforts, and the first reviewer's usage. It then deletes the requesting session and asserts that no report row remains. A second regression asserts that a failed reviewer whose child still holds a handback for another command shows no handback.
- The migration regressions assert that a store upgraded to revision 134 refuses revision-133 readers and writers, because they would lose a finished review's output.
- The API test performs the same lifecycle through the isolated writer and assert the HTTP response fields, including `404` for an unknown review id.
- The CLI parser accepts `mj review show --session s1 --review r1 --json` and defaults `--review` to absent.

## Idempotence and Recovery

The migration is additive and repeats safely. Report syncing is an upsert; finalization updates only rows whose `finished_at_ms` is still null, so a retried close cannot move the finish time or change a resolution. If a daemon crashes between reviewer settlements, the next state save copies the handbacks that still exist; a report row already holding a handback is never overwritten with null. A destroyed session's reports go with its `sessions` row, and deleting them twice is harmless. Re-running any test is safe; the tests use temporary directories or isolated stores. No production data is migrated or deleted.

## Artifacts and Notes

The review-host trace that identified the stale test case:

    review-host trace: case pushfailed shutdown
    review-host trace: case manualoff start
    ... (no capture; test killed after the next-reviewer wait)

The database test's surviving output includes `roles[0].handback == "The parser drops queued input."` after `DELETE FROM subagent_handbacks`.

## Interfaces and Dependencies

In `mj-core/src/storage.rs`:

    pub enum FinishedReviewResolution {
        Completed,
        Failed,
        Cancelled,
    }

In `mj-controller/src/database/turn_reviews.rs`:

    pub struct FinishedReview {
        pub session_id: String,
        pub review_id: String,
        pub finished_at_ms: i64,
        pub resolution: FinishedReviewResolution,
        pub roles: Vec<FinishedReviewRole>,
    }

    pub struct FinishedReviewRole {
        pub role: String,
        pub label: String,
        pub command_id: String,
        pub child_session_id: Option<String>,
        pub profile: String,
        pub model: Option<String>,
        pub effort: Option<String>,
        pub handback: Option<String>,
        pub verdict: Option<mj_core::review::verdict::ReviewVerdict>,
        pub turn: Option<mj_core::state::MaterializedTurnOutcome>,
    }

    pub fn close_turn_review_state(
        session_id: &str,
        state: &TurnReviewState,
        resolution: FinishedReviewResolution,
    ) -> anyhow::Result<()>;

    pub fn finished_review(
        session_id: &str,
        review_id: Option<&str>,
    ) -> anyhow::Result<Option<FinishedReview>>;

In `mj-controller/src/review_host/environment.rs`:

    fn close_review(
        &self,
        session_id: &str,
        state: &TurnReviewState,
        resolution: crate::database::FinishedReviewResolution,
    ) -> Result<(), String>;

In `mj-controller/src/server/api/types.rs`:

    pub struct FinishedReviewQuery {
        pub review: Option<String>,
    }

    pub struct FinishedReviewResponse {
        pub session_id: String,
        pub review: Option<crate::database::FinishedReview>,
    }

The HTTP route is `GET /api/v1/sessions/{session_id}/reviews` with an optional `review` query parameter. The CLI command is `mj review show --session <id> [--review <id>] [--json]`.

Revision note (2026-11-17): initial plan written alongside the implementation; all design decisions above are the ones actually used. The stale review-host test cases were found during the plan's validation step and removed there. Round-1 review added the `review_reports_session_destroyed` trigger; the plan's migration, decision log, progress, and acceptance text were updated with it. Round-2 review raised the compatibility floor to 134 and restricted retained handbacks to accepted outcomes; the context, decision log, and acceptance text were updated again. Round-3 review found that the cleared-group close ignored the host's explicit resolution; the decision log and the database regressions were updated a third time.
