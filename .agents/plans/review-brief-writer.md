# Give mj review a relation brief from a writer session before the reviewers start

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`.


## Purpose / Big Picture

Today `mj review` starts its reviewers (fresh agent sessions, usually Codex Terra/high) with a prompt that holds the review rubric, the git diff command for the change, and the user's messages. A reviewer sees the diff and nothing else unless it goes looking, and on bugs whose cause lies outside the diff it rarely does. A measured study in this repository (`.agents/evals/review-arms/README.md`, sections "Relation briefs") found that one extra step changes that. Before the reviewers start, a separate "brief writer" session reads the change and the repository and writes a short list of relations between the changed code and the rest of the repository: who calls the changed code and with what, what else reads or writes the same state, which other paths reach it. On 18 Hard-won mjolnir bugs whose decisive fact is such a relation, Terra/high reviewers caught 20% with no brief and 56% with such a brief, which was written without knowledge of the bug. On the 10 diffs where it helped, the rate went to 90%. The writer used only standard tools (git, ripgrep, file reads) and cost about $0.19 and two minutes per change.

After this change, a user who turns on `[review] brief_writer` gets that step on every review. `mj review` first runs a writer session on the change, then starts the reviewers with the writer's brief added to their prompt. The brief is labelled as relations, not findings. If the writer fails or is cancelled, the review runs exactly as today and the user is told the brief was skipped. To see it work, enable the setting, make a commit in a session, request a review, and read the reviewer prompt: it contains a "Relations computed from the repository" section with `file:line` items.

A later milestone gives the writer Bifrost query tools (RQL steps over a code graph) in addition to the shell, one query template at a time, measured for how many of the decisive facts it reaches that grep misses.


## Progress

- [ ] Milestone 0 (validation, in the review-arms harness, in parallel with milestones 1-3): writer model check (`.agents/evals/review-arms/brief/PLAN-writer-terra.md`) and generalization to bifrost-dev bugs (`brief/PLAN-bifrost-bugs.md`). Owners: helpers V1 (session c8c109b6c1c9a2378fe25ca5c76c9016) and V2 (d2fa840b25852a07c2e024357afec69d).
- [x] Milestone 1 (2026-11-16): `brief_writer` config and a brief section in the reviewer prompt, with no writer yet (a brief supplied in the seed renders; none renders nothing).
- [x] Milestone 2 (2026-11-16): the WritingBrief phase. Driver, writer prompt (`mj-review/templates/brief_writer.md`), host registration and routing, recovery, and a "Brief writer" row in the review view (running, then done or failed; its child session id is attached like a reviewer's). Tests: 6 driver transitions (written, failed or empty, cancelled, group failure, restart), 3 host flow tests (hands back, ends without a handback, fails to launch), 2 host restore tests in WritingBrief. Full suites and clippy: see Outcomes.
- [x] Milestone 2 compatibility follow-up (2026-11-17): merged `origin/master` (`a43b47fbb`) without conflicts; added breaking schema revision 135/minimum-compatible 135 and daemon protocol 102; updated historical schema fixtures and final compatibility assertions. Targeted tests: 46 schema/migration + 80 review-host tests, 1 core review-driver test, 11 client protocol tests passed. Final workspace Clippy and formatting checks passed. API baseline classification: see Outcomes.
- [ ] Milestone 3: end-to-end in an isolated instance with a real harness. A real review shows the brief in reviewer prompts; usage is accounted; the writer is destroyed at the end.
- [ ] Milestone 4: ship decision. Set the default writer model from milestone 0, document the setting, and decide whether it is on by default from milestone 0's generalization result.
- [ ] Milestone 5: Bifrost tools for the writer (a per-role MCP server for the writer child), added template by template, each measured.


## Surprises & Discoveries

- Observation: reviewers are fresh child sessions and do not see the requesting session's conversation. The study put the brief into a "carrier" session's context, which reviewers then read; production has no such path, so the brief must go into the reviewer prompt itself.
  Evidence: `mj-review/src/lanes.rs:106` `review_prompt` builds the prompt from the rubric, the diff command, the diffstat and the user messages. The requester's assistant turns are excluded.
- Observation: since 8fe3c40bd, reviewer child sessions are destroyed when their role ends; their handback text survives in the store's `subagent_handbacks` table. The writer child follows the same lifecycle.

- Observation: `mj-tui/src/setup/schema.rs` has no `[review]` entries; reviewers are edited in Settings → Code Review (`mj-tui/src/review_settings.rs`), which saves the whole loaded `ReviewConfig` and so preserves `brief_writer`. Milestone 1 therefore documents the table (`docs/src/content/docs/turn-review.md`) and leaves it config-file only. A settings page for it is a follow-up.
- Observation: `TurnReviewSeed` has a hand-written `Serialize` (for older snapshot readers). It writes `brief` only when set, so a snapshot with no brief is byte-identical to before.


## Decision Log

- Decision: the brief writer is a stage of `mj review`, not a Bifrost command with its own model loop.
  Rationale: Jonathan chose this on 2026-11-16. mj already owns sessions, profiles, quota, review delivery and restart; Bifrost stays a tool provider.
  Date/Author: 2026-11-16, Jonathan (relayed by session dbcece95).
- Decision: ship the writer first with standard tools behind a config flag; add Bifrost tools later, template by template.
  Rationale: the grep-only writer is already measured. Jonathan's condition: before shipping, check whether Terra can write the brief (cheaper, and it's the recommended reviewer), and check that the design generalizes to bifrost-dev's own bugs (milestone 0).
  Date/Author: 2026-11-16, Jonathan.
- Decision: the writer runs as a new review phase (WritingBrief) between change capture and reviewer launch, as a child registered like a reviewer.
  Rationale: it reuses the reviewer child machinery: registration with a durable request key, the outcome save before the handback is acknowledged, restart restore, destroy on end, and accounting. Only the phase, the seed field, the prompt section, the writer prompt, the config and a notice are new (map in this session's report `review-map.md`; see Context).
  Date/Author: 2026-11-16, dbcece95.
- Decision (milestone 2, 2026-11-16, implementer): the writer child uses `SubagentKind::Reviewer` with its own role key `brief-writer`, not a new kind.
  Rationale: `settle_child` in `daemon/delegation.rs` routes every Reviewer child's handback to the review host, and the host, the startup sweep and the request-key scheme already key off that kind. A new kind would need each of those taught about it. The cost is that the writer shows up wherever reviewer children are listed and in the same accounting row type. Revisit if the UI must tell them apart.
- Decision (milestone 2): the writer's registration lives in `ActiveReviewGroup.registrations` (so close, sweep and question routing include it) with the resolved settings in a new `ActiveReviewGroup.brief_writer`. It is not a reviewer slot, so it has no verdict.
- Decision (milestone 2): the driver learns whether to write a brief from the caller (`delta_captured_with_writer(deltas, bool)`), not from the seed, so the persisted snapshot carries no setting. The prompt for the writer is built from the same diff-command section as the reviewers' and the user's task (no commit message exists for a tree-to-tree capture).
- Decision (milestone 2): if the writer's model cannot be resolved or its profile fails `check`, `prepare` records the reason and the review starts without a writer; the requester gets the "Review started without a relation brief" notice when the review opens.
- Decision (milestone 2): the review view gets a "Brief writer" role row, first in the list, from a new `ReviewDriverSnapshot.brief_writer` state (running, clean, failed; absent for a review with no writer). The TUI, web viewer and `mj api` review lines render roles generically (label and state), so none needed a change. The skip reason is in the requester's notice, not on the row. The writer's child also counts in "Reviewers · n" and `owns_reviewer`, which keeps it out of the plain Subagents list.
- Decision (milestone 2, compatibility, revised 2026-11-17): add breaking schema revision 135 with minimum compatible revision 135, and advance daemon protocol 101 to 102.
  Rationale: schema revisions 97 and 105 establish that new stored enum values need a compatibility fence even without SQL shape changes. Older daemons cannot decode `writing_brief`, a seed's brief, or the writer registration. Their startup recovery refusal blocks every review until the active group is cleared; this was a dead end, not an acceptable downgrade behavior. Revision 135 updates the compatibility floor in the same transaction as the revision, before new review values can be stored. `RuntimeReviewView` carries the phase across the wire, so protocol 102 refuses older running clients before they parse it (#1557). Parent reversed the earlier no-fence decision after reviewing that precedent and AGENTS.md's stored-JSON/enum compatibility rule.
  Date/Author: 2026-11-17, dbcece95 (implemented by continuation helper).
- Decision (milestone 2 continuation): remove obsolete review-off cases from the push-verdict test, retaining the dedicated review-off integration test.
  Rationale: the merge-base reproduction and temporary checkpoints prove the old test waits for a reviewer on the path that records an override and chooses none; its unanswered manager request caused the full suite to hang.
  Date/Author: 2026-11-16, continuation implementer.
- Decision: a writer failure never fails or blocks the review. Its outcome is "no brief", with a notice to the requester, and the reviewers launch as they do today.
  Rationale: the brief is an aid; a review without it is what users get now.
  Date/Author: 2026-11-16, dbcece95.
- Decision: the writer prompt is the measured one (`.agents/evals/review-arms/brief/WRITER.md`), including its output budget of at most 25 items and about 1,500 tokens.
  Rationale: that exact prompt produced the measured lift. The budget is part of the measured configuration, not a guard against an unobserved failure. Changing it needs a new measurement.
  Date/Author: 2026-11-16, dbcece95.
- Decision (accepted M2 review findings, 2026-11-17): retain `CloseBriefWriter` and `NoteBriefSkipped` in Running recovery, surface writer forms during WritingBrief through the existing review-child flow, and carry `ReviewChildRole::BriefWriter` in `ReviewerLaunchConfig` so MCP initialization, handback-tool and message-field instructions request relations. An exact `No findings.` handback follows the empty-brief failure path.
  Rationale: fixes accepted P2 findings on `5fc728617`: persisted outcomes lost writer cleanup, writer permissions were hidden, and the reviewer's handback contract contaminated the brief.
- Decision (worker skew): no relay protocol bump. The launch-only `role` field is omitted for ordinary reviewers and defaults to Reviewer when absent. Older `ReviewerLaunchConfig` readers use `deny_unknown_fields`; `WorkerLaunchConfig::read` parses that nested type before harness startup (`mj-worker/src/main.rs` login bootstrap and Run), so an older worker given a writer launch refuses it visibly with the worker/daemon build-mismatch diagnostic. The review host then skips the failed writer with a notice, rather than accepting reviewer-formatted output. Unknown role values are rejected by the typed enum and MCP reader too.
- Decision (review follow-up): startup is the sole owner of a child's relay prompt ID. Reconciliation uses the registration request key with the existing startup-step ID helper, for writers and reviewers, while the pure driver retains its logical role keys. Recovery fixtures obtain recorded IDs from `api_followup_steps`, the actual producer, so they cannot hide a mismatch by inventing a driver ID. Running recovery also replays the failed-writer notice alongside cleanup and reviewer starts.
- Decision: the default writer model is Sol/high, not Terra/high.
  Rationale: milestone 0's writer-model check (`.agents/evals/review-arms/brief/PLAN-writer-terra.md`, round J, Sol/high judges, all caught/partial verdicts hand-checked) failed both registered criteria for Terra. On the 18 primary relation diffs, Terra reviewers caught 29.6% with Terra-written briefs, 55.6% with Sol-written briefs and 20.4% with no brief: TT − BT = −25.9 points (95% CI −44.4 to −9.3), TT − no-brief = +9.3 (−5.6 to +27.8). A Terra brief costs about a quarter of a Sol brief ($0.051 vs $0.198 median) but doesn't carry the lift.
  Date/Author: 2026-11-17, dbcece95 (helper V1 c8c109b6 ran the check).

## Outcomes & Retrospective

Milestone 2 continuation (2026-11-16): the writer role row fixes the two host tests that could not find the running writer. Both host restore tests pass for a writer that ended with and without a handback, with one reviewer registered and no writer restart. The previous controller-suite hang reproduces at merge base `4dc0e2851` in a temporary worktree (subsequently deleted): obsolete review-off cases wait for a reviewer although `start()` honors Off and awaits `RecordOverridden`. Removed those cases from the verdict test; the dedicated review-off acceptance test already covers that path. Removed the fake's unused subagent flag, exposed by that cleanup. No production change was needed for this baseline hang. Validation (all Cargo tests outside the sandbox, dev profile): `cargo test -p brokk-mj-controller review_host::tests::a_brief` passed 3; the two `a_restart_after_the_writer` host tests passed. The full four-crate command completed mj-client (56 unit + 1 integration passed, 1 ignored) and mj-controller (2,678 passed, 28 ignored, 3 unrelated API profile-selection failures). Cargo stopped at the controller failures, so `cargo test -p brokk-mj-core -p brokk-mj-review` completed the remaining suites: core 659 passed/4 ignored, review 28 passed. Remaining doc-test phases were run separately; all doc tests passed (one core example ignored). The three API failures reproduced in a failure-only rerun: `a_profile_that_cannot_be_discovered_is_left_out_not_fatal` and `spawn_refuses_a_profile_whose_login_is_known_to_be_refused` expected registration that did not occur; `spawn_runs_the_child_on_the_profile_with_the_most_quota_that_offers_the_model` expected paid-credit use without enabling it. These untouched API/selection expectations are outside milestone 2. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed. Logs and commands are in the continuation report directory (session 4fc59802).


Milestone 2 compatibility follow-up (2026-11-17): merged `origin/master` at `a43b47fbb` without conflicts (merge `601d3d5cd`). Breaking schema revision 135 and minimum compatible revision 135 now fence the new stored review JSON before any can be written; daemon protocol 102 fences the new review-view phase for connected clients. The historical fixture rewind helper restores the pre-fence floor for revisions 132–134, while final-store assertions require the new floor. Targeted validation passed: 46 database schema/migration tests (including the isolated direct-upgrade matrix), 80 review-host tests, 1 mj-core review-driver serialization test, and 11 mj-client daemon/protocol tests. Workspace `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed. No full suites were rerun.

The same three API-selection tests fail after the merge, on clean `origin/master` at `a43b47fbb`, and on that clean master with inherited `MJ_*` removed and fresh isolated config/data plus instance `brief-api-env`. They are pre-existing and are not caused by the session's `MJ_*` environment. The two missing-registration fixtures remove or reject `codex-high` but expect selection of `parent`, whose quota fixture is zero; the third gives `codex-high` paid credits without enabling `load_balancing_may_consume_credits`. No API fixtures or selection behavior were changed. The temporary comparison worktree was removed after both runs. Logs: `fence-controller.log`, `fence-core.log`, `fence-client.log`, `fence-clippy.log`, `fence-fmt.log`, `merged-api.log`, `master-api.log`, `master-api-clean-env.log` in the continuation report directory (session 4fc59802).

Accepted-review follow-up on `5fc728617` (2026-11-17): fixed durable writer cleanup across Running restore, writer question/permission visibility during WritingBrief, and the relation-brief MCP contract carried by a typed launch role. A leaked `No findings.` handback now skips the brief. New regressions restore serialized outcomes after success and failure and observe one writer close, exercise the chat's typed daemon view, and compare MCP initialization and handback descriptions for a legacy reviewer launch and a writer launch. No relay bump is needed because older workers reject the new role field in strict launch decoding before starting the harness, with the existing failed-writer notice/fallback; ordinary reviewer launches omit the field.

Targeted validation in `~/Projects/mjolnir-wt/m2-review`, all Cargo tests outside the sandbox: `cargo test -p brokk-mj-controller -p brokk-mj-review -p brokk-mj-chat -p brokk-mj-worker -p brokk-mj-core --lib -- review_host::tests mj_review::driver driver::tests turn_review subagent_mcp::tests worker_launch::tests` passed 158 tests: chat 1, controller 84 (81 review-host plus 3 other driver cases), core 16, review 18, worker 39. Workspace `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed. Evidence: `review-fixes-tests.log`, `review-fixes-clippy.log`, `review-fixes-fmt.log` in continuation report directory session 4fc59802. No full suites rerun; previous worktree untouched.

Final-review follow-up: retained `NoteBriefSkipped` during Running recovery and asserted delivery of the saved reason in the host restore test. Matched restored and ended child handbacks against startup-owned prompt IDs instead of driver-local role keys; this fixes the shared writer/reviewer reconciliation path. The fixtures now generate recorded handbacks through `api_followup_steps`, and a writer ending after recording its handback has a separate regression. Affected tests passed: 82 review-host tests, 18 driver tests, and 1 core serialization test (`review-startup-tests.log`); the intermediate notice-only run also passed (`review-notice-tests.log`). Final workspace lint/format evidence is in `review-followup-clippy.log` and `review-followup-fmt.log`. All tests ran outside the sandbox in the same worktree; no full suites rerun.

## Context and Orientation

Words used here:

- A **review** is what `mj review start`, the `request_review` MCP tool, or a push gate starts. It runs one or more **reviewers**: child agent sessions that read the change and call a **handback** tool with their findings.
- The **review host** is the daemon-side owner of reviews, in `mj-controller/src/review_host/`.
- The **review driver** is the pure state machine for one review, in `mj-review/src/driver.rs`. It is persisted as a snapshot so that a review survives a daemon restart.
- The **seed** (`TurnReviewSeed`, `mj-core/src/review/driver.rs:99`) is the review's fixed input: task, user messages, repositories and so on, from which the driver builds the reviewer prompt once.
- A **brief** is a Markdown text of at most 25 relation items, under the headings `## Affected callers`, `## Paths` and `## Value flow`. Each item states one relation between code locations, with `file:line` anchors, and no judgment words.

How a review runs today, by file:

- Entry: `TurnReviewHost::begin_push` (`mj-controller/src/review_host/host.rs:143`), then `HostState::begin_push` (`begin.rs:45`).
- A cancellable task runs `prepare()` (`mj-controller/src/review_host/reviewer.rs:120`): it captures the change, resolves reviewers through `review_selection.rs:21`, and checks profiles.
- `prepared()` (`persist.rs:4`) calls `TurnReviewDriver::start` (`mj-review/src/driver.rs:34`) and then `delta_captured` (`driver.rs:292`). That builds the prompt once via `mj-review/src/lanes.rs:106` `review_prompt` and emits `ReviewRequest::StartReviewer` per role.
- `run_one` (`roles.rs:4`) executes requests. `StartReviewer` becomes `register_reviewer_child` (`reviewer_child.rs:98`), then `register_child` (`environment.rs:280`): `Controller::register_subagent` with kind Reviewer and a durable request key. The initial prompt is the review prompt plus `HANDBACK_INSTRUCTION` (`reviewer_child.rs:3`). Reviewers get no MCP servers (`environment.rs:290` passes `&[]`).
- A handback arrives as `HostEvent::ReviewerFinished` (`dispatch.rs:205`) and goes to `reviewer_finished` (`reviewer_child.rs:567`). That updates the driver, saves the outcome, then acknowledges. A child that ends without a handback goes through `child_ended` (`:501`) to `role_failed`.
- Partial results are mailbox events written in the save transaction (`delivery.rs`); the final verdict goes out from `environment.rs:223`. Closing is `destroy_reviewer_child` (`daemon/resume.rs:725`), with a startup sweep (`reviewer_child.rs:372`).
- On daemon restart, `RestoreGroups` (`dispatch.rs:244`) relaunches or reattaches children from the stored state.
- The persisted state is `TurnReviewState.active_group` (`mj-core/src/storage.rs:241`).

Configuration: `[review]` is `ReviewConfig` (`mj-core/src/config.rs:141`), parsed through `ReviewConfigInput` with `deny_unknown_fields` (`:247`). Reviewers are `SubagentChoice { model, effort }`, resolved by `review_selection.rs`. The TUI's setup schema (`mj-tui/src/setup/schema.rs`) and the user docs (`docs/src/content/docs/turn-review.md`) describe it.

Tests that a change here extends:

- host flow tests in `mj-controller/src/review_host/tests.rs` (fake environment and manager);
- driver and prompt tests in `mj-review/src/driver/tests.rs` (prompt tests near lines 38-216; restart replay near 313) and `mj-review/src/lanes.rs` tests (near 210);
- the golden `mj-core/tests/golden/mailbox-events.txt`;
- the opt-in live test `tests/e2e/review_before_push.py`.

Repository rules that bind this work (AGENTS.md):

- The daemon's host loop must never block. Work runs in spawned tasks that report events.
- Any daemon-side task must be short or resumable from durable state after a restart, and replay must not start a reviewer or writer twice.
- No speculative caps or timeouts.
- Tests follow "Which tests to add": goldens first, then end-to-end, then integration and tricky conditions.
- Test new code in an isolated `--instance`, never the default one.


## Plan of Work

Milestone 1 adds the brief to the reviewer prompt, with no writer. In `mj-core/src/review/driver.rs`, give `TurnReviewSeed` a field `brief: Option<String>` with `#[serde(default)]`, so that snapshots written before this change restore with no brief. Carry it into `ReviewJob` (`mj-review/src/lanes.rs:25`). In `review_prompt`, when it is `Some` and non-empty, render a section after the diff command and before the user messages:

    ## Relations computed from the repository

    These are relations computed from the repository at this commit, not findings. They may be incomplete.

    <brief text, verbatim>

When it is `None`, render nothing, so prompts are byte-identical to today's. Add `brief_writer` to `ReviewConfig`, `ReviewConfigInput` and `ReviewConfigOutput` as an optional table: `enabled` (bool, default false), `model` and `effort`, resolved through the same `SubagentChoice` path as reviewers. Validate it (`model` is required when enabled) and round-trip it in the serialized config. Add it to the TUI setup schema and to `docs/src/content/docs/turn-review.md`. Tests: a golden of the whole reviewer prompt with and without a brief, as a new file under the existing golden directory (`mj-core/tests/golden/` or the crate's own), since none exists today. Also a restore test showing an old snapshot without the field still restores and renders without a brief.

Milestone 2 adds the writer. In the driver (`mj-review/src/driver.rs`), add a phase `WritingBrief` that `delta_captured` enters, instead of emitting `StartReviewer` directly, when the resolved settings carry an enabled brief writer. It emits one `ReviewRequest::StartBriefWriter` with the writer prompt. Build that prompt from `.agents/evals/review-arms/brief/WRITER.md` as a template in `mj-review/templates/`, filling in the base commit, the commit message or task, and the diff command, as the study did. The prompt tells the writer to reply with the brief through its handback tool, rather than writing `brief.md`, because the handback text is what the store keeps.

Three new driver inputs handle the writer's outcome:
- `brief_written(text)` stores the text in the seed, rebuilds the reviewer prompt, emits `StartReviewer` per role and `CloseBriefWriter`, and enters `Running`;
- `brief_failed(reason)` emits the same reviewer starts with no brief, `CloseBriefWriter`, and a notice request;
- cancellation closes the writer and ends the review as cancelling does today.

In the host, `roles.rs` executes `StartBriefWriter` through the reviewer child path. That means `register_child` with a distinct role key (`brief-writer`), the same durable request key scheme, `SubagentKind::Reviewer`, or a new kind if accounting or UI must tell them apart (decide while implementing, and record it here), and the resolved writer model and effort. `reviewer_finished` and `child_ended` route the writer's role key to `brief_written` and `brief_failed` instead of `role_turn_completed` and `role_failed`. The writer's outcome is saved in the same save-before-acknowledge transaction, so a restart neither loses the brief nor starts reviewers twice.

The requester's accept answer currently fires on the first launch persistence (`persist.rs`). Keep it firing when the writer launches, so the requester isn't kept waiting.

The skipped-brief notice is a requester-visible line, like the existing "Review did not start" line (`reviewer.rs:317`), for example "Review started without a relation brief: <reason>".

Tests:
- driver state-machine tests for every transition: written, failed, cancelled during WritingBrief, and restart during WritingBrief (restore relaunches or reattaches the writer and never starts reviewers early);
- host flow tests in `review_host/tests.rs` with the fake environment, covering a writer that hands back, one that ends without a handback, and one that fails to launch;
- extending the mailbox-events golden if the notice is a mailbox event.

Milestone 3 proves it end to end. Extend `tests/e2e/review_before_push.py`, or add a sibling script with the same options (`--mj --worker --root --instance`). It runs a real `mj` and daemon in an isolated instance with `[review] brief_writer = { enabled = true, model = <a configured model>, effort = "high" }`, makes a commit, requests a review, and checks:
- the reviewer's recorded prompt contains the relations section;
- the writer's handback is in `subagent_handbacks`;
- both children are destroyed after the review;
- the writer's usage is in `mj usage` for the requesting session.
Capture the transcript excerpt as evidence in this plan.

Milestone 4 decides the shipped defaults from milestone 0:
- the default writer model is Terra/high if `PLAN-writer-terra.md` reads "Terra writes", otherwise Sol/high;
- the setting is on by default only if `PLAN-bifrost-bugs.md` reads "Generalizes", otherwise it stays opt-in;
- the docs state the measured effect and cost.

Milestone 5 gives the writer Bifrost tools. Today reviewer children get no MCP servers (`environment.rs:290`). Add a per-role MCP server list to the writer's launch config, from config, so the writer can call Bifrost's `query_code` (RQL). The writer prompt gets the query templates from the query census (bifrost-dev `.agents/scratch/h6/SYNTHESIS.md`, accepted by Bifrost's query-surface owner):
- callers with argument values;
- other readers and writers of the storage the diff touches;
- guards around a site;
- error propagation.
Each template is added only when its steps answer completely and quickly on real code. Its effect is measured as the share of census decisive facts the writer's brief states, against the grep-only writer, plus the reviewers' catch rate. Bifrost's prerequisites are tracked in bifrost-dev (#13921, #13211 slices, #13871, #13616, #13401, #13612/#13728, #13216 S2) and are not part of this plan's code.


## Concrete Steps

From the repository root, `cargo build` and the targeted tests per milestone:

    cargo test -p mj-review driver
    cargo test -p mj-review lanes
    cargo test -p mj-core review
    cargo test -p mj-controller review_host

After the last milestone, run the full suites of every touched crate and `cargo clippy --all-targets -- -D warnings`, per AGENTS.md. The end-to-end script runs with an isolated instance:

    python3 tests/e2e/review_before_push.py --mj target/debug/mj --worker <worker> --root <tmp root> --instance brief-writer-e2e

(See the script's `--help` for its exact options.)


## Validation and Acceptance

- Milestone 1: the new golden shows the relations section when a brief is present and a prompt byte-identical to today's when it is absent. An old snapshot restores.
- Milestone 2: the driver and host tests pass for written, failed, cancelled and restarted. A failed writer leads to a review that runs and resolves normally, with the notice line.
- Milestone 3: in the isolated instance, a real review's reviewer prompt contains a non-empty relations section with `file:line` items, and the writer's usage appears under the requesting session.
- Milestone 4: the review-arms readouts for both validation plans are recorded here, with the defaults they set.
- Milestone 5: each added template states at least one census decisive fact the grep-only writer missed, without lowering the reviewers' catch rate on the census diffs.


## Idempotence and Recovery

The seed field and the config table are additive, and old snapshots and configs load unchanged. If a milestone-2 change misbehaves, setting `brief_writer.enabled = false` returns reviews to today's behavior, without starting a new writer. Schema revision 135 fences older daemons before they can read or write the new stored review forms. Restarts during WritingBrief are covered by the durable request key and the save-before-acknowledge order. A test proves that replay starts neither the writer nor the reviewers twice.


## Artifacts and Notes

The study's evidence and the writer prompt are in `.agents/evals/review-arms/`: README sections "Relation briefs, stage B/C", "ablation", "dilution" and "Wizard-of-Oz", and `brief/WRITER.md`. The measured cost of a writer pass, Sol/high, median over 70: $0.185, about 114 s, 469k tokens, of which about 410k are cached re-reads. Relation-finding is about 19% of that cost (transcript analysis, 2026-11-16).


## Interfaces and Dependencies

At the end of milestone 2 these exist:

- In `mj-core/src/review/driver.rs`: `TurnReviewSeed { ..., #[serde(default)] pub brief: Option<String> }`.
- In `mj-core/src/config.rs`: `ReviewConfig { ..., pub brief_writer: Option<BriefWriterConfig> }`, where `BriefWriterConfig { enabled: bool, model: String, effort: Option<String> }`, resolved alongside the reviewers.
- In `mj-review/src/driver.rs`:
  - a phase `WritingBrief`;
  - a request `ReviewRequest::StartBriefWriter { prompt: String, model: ..., effort: ... }` and `ReviewRequest::CloseBriefWriter`;
  - inputs `brief_written(&mut self, text: String) -> Vec<ReviewRequest>` and `brief_failed(&mut self, reason: String) -> Vec<ReviewRequest>`.
- In `mj-review/templates/`: `brief_writer.md`, from `brief/WRITER.md`.
