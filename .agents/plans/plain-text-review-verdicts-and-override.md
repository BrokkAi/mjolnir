# Plain-text review verdicts and a primary-agent override for before-push review

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir can review an agent's committed work before the agent pushes it. The agent calls a `request_review` tool, a hidden reviewer session reads the diff, and the verdict arrives later as a message in the agent's conversation. A clean verdict records the reviewed Git tree so the repository's pre-push hook lets the push through; a findings verdict tells the agent to fix the findings, commit, and ask again.

Two things change after this plan.

First, the reviewer answers in plain text instead of a JSON document. Today the reviewer is asked to return a Codex-style JSON object, Mjolnir parses it, then flattens it to text and throws the structure away. The JSON contract is the part reviewer models most often get wrong, and nothing uses the structure. After this plan the reviewer writes its findings as ordinary text, or writes exactly `No findings.` when there is nothing material to report. An empty answer still counts as a failed review, never as a clean one.

Second, the primary agent gets a way to override a findings verdict it believes is wrong. Today its only options are to comply or to run `git push --no-verify`, which leaves no record. After this plan a new `override_review` tool, available in the same sessions that have `request_review`, records the exact tree the reviewer saw as reviewed, clears the pending findings, and writes a visible notice into the session transcript with the agent's stated reason and the findings it overrode. The tool refuses if HEAD has moved since the verdict or if a review is still running.

You can see both working by running the unit tests named in each milestone, and end to end by starting a `push`-trigger session, committing a change, calling `request_review`, receiving a findings verdict, and then calling `override_review` and watching the push succeed while the transcript shows the override notice.

## Progress

- [x] (2026-11-03) User authorized implementation and pushing; checkout updated to origin/master `60aadd739`, and this plan copied from its authoring session.
- [x] (2026-11-03) Milestone 1 committed as `7fb6becda`: plaintext verdicts implemented; reviewer parser/driver suite passed (24 tests), controller review host (38), controller API (55), and worker MCP (33) checkpoint tests passed. The affected rubric prompt test passed after final rubric edits.
- [x] (2026-11-03) Milestone 2 committed as `6d5728d85`: host override, durable findings trees, storage migration 104. Controller review-host tests passed (44), legacy/storage regression passed (1), core storage tests passed (7).
- [x] (2026-11-03) Milestone 3: `override_review` MCP/action/API/allowlist wiring and breaking schema 105 validated. Core subagent tests (36), worker MCP (34), Claude allowlist (1), controller dispatch (1), and all six corrected migration fixtures passed. Checkpoint commit pending.
- [x] (2026-11-03) Milestone 4: mailbox text/golden (4 tests), gate instructions, and docs are complete. Docs `npm run check` and `npm run build` passed, including 2,304 internal links across 27 HTML files. Guidance checkpoint commit pending.
- [ ] Follow-up requested by the user: configurable parallel reviewers, default 2, with separate findings attribution and an all-clean gate. The detailed companion plan is `.agents/plans/parallel-reviewers.md`; complete its implementation before final full validation and push.

## Surprises & Discoveries

The current JSON parser defaults omitted fields, so `{}` is classified clean. Removing the parser must make that text a findings verdict. The reviewer-specific MCP tool description also requests JSON, independently of the rubric and appended handback instruction, and must be updated with them.

Cargo package selectors use the published names (`brokk-mj-review`, `brokk-mj-controller`, `brokk-mj-core`, and `brokk-mj-worker`), which differ from their directory names. The commands in this plan use those package names.

The current upstream schema revision is 103, with compatibility floor 102. Review state is stored as individual SQL columns, not as one JSON blob. Milestone 2 therefore needs breaking revision 104 adding nullable `turn_review_state.push_findings TEXT` and its read/write handling. Milestone 3 needs a separate breaking revision 105 for the new durable action. Historical schema fixtures must remove the new column for revisions below 104. Never rewrite a migration after its checkpoint commit.

The host's existing `record_notice` is a detached courtesy operation whose failure is only logged. It cannot provide an audit guarantee for an override that unlocks a push. Override notices must instead be acknowledged before recording a reviewed ref, with wording that describes an override request rather than claiming an operation already succeeded.

Review state writes use a FIFO persistence lane, but preparation reads originally bypass that lane. A cancelled open can release its preparation reservation while its clearing write remains queued. Override loads and saves must go through that lane to prevent an older queued write from overwriting an override. Push verdict delivery also runs concurrently with the lane, so the existing review owner must remain a refusal until its completion has been persisted.

Six historical migration tests manually rewound their revision metadata while retaining the new `push_findings` column. Their first rerun failed with `duplicate column name: push_findings`. The attempted production migration guard was rejected because revision 104 was already committed; the parent restored that SQL verbatim and added one test-only historical review-state restoration helper shared by those fixtures and `stamp_schema_version`. Production migrations remain strict.

## Decision Log

- Decision: commit each validated milestone on the current branch and push the completed implementation to `origin/master`.
  Rationale: the user explicitly answered "implement and push", superseding the peer session's earlier no-push request.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: run affected tests for milestone checkpoints, then full validation once after the last implementation round.
  Rationale: repository `AGENTS.md` requires reusing passing validation, rerunning only failures, and running full suites and workspace clippy once after the last round. This replaces repeated full clippy runs in the milestone command examples below.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: use one typed preparation owner per session for review preparation and override work, and serialize override state loads and saves through the existing persistence lane.
  Rationale: a second override/review must not pass admission on a different snapshot, and review cancellation/shutdown must distinguish cancellable preparation from accepted override mutations.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: implement disjoint host, protocol, and messaging slices concurrently while keeping checkpoint commits in milestone order.
  Rationale: the host API has a settled signature and does not depend on the new action variant; separate file ownership avoids edit conflicts. Compilation and tests wait until the concurrent drafts are complete.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: add a storage migration in milestone 2 and a separate durable-action compatibility migration in milestone 3.
  Rationale: inspecting `database/turn_reviews.rs` showed state fields are SQL columns; each milestone must stand on its own, and older revisions must not silently lose pending findings or misread a queued override action.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: record and await the override-request audit notice before unlocking the reviewed trees. Acquire labeled daemon upgrade admission only after cancellable recapture finishes, and hold it through the bounded audit, mutation, and persistence work.
  Rationale: the existing best-effort notice could disappear after a successful override; an acknowledged pre-mutation notice prevents that. Its wording remains truthful if the subsequent mutation fails. Long preparation must not delay daemon handoff or cancel accepted mutation work.
  Date/Author: 2026-11-03 / implementing agent.
- Decision: a clean verdict requires the exact text `No findings.`; an empty or whitespace-only handback stays a failed review.
  Rationale: the push gate must fail closed. A reviewer that ran out of context, hit an error, or called `handback` with nothing must not unlock a push. Requiring a positive assertion costs one line of parsing.
  Date/Author: 2026-11-03 / plan author.
- Decision: drop the JSON output contract entirely rather than keep it alongside plain text.
  Rationale: `mj-review/src/verdict.rs` already discards the parsed structure (only `synthesis: String` is stored; `evidence` is a legacy field that is always empty). The JSON buys only the clean signal, which the sentinel replaces. Keeping both would be two mechanisms for one outcome.
  Date/Author: 2026-11-03 / plan author.
- Decision: the override applies to the whole last verdict, not to individual findings, and takes a free-text `reason`.
  Rationale: a corrective pass only re-reports findings that remain (`mj-review/src/lanes.rs`, `review_pass_context`), so "fix what you agree with, re-review, then override what remains" is per-finding in effect without finding ids. Per-finding ids would require retaining structure that this plan removes.
  Date/Author: 2026-11-03 / plan author.
- Decision: the override is valid only for the exact trees the reviewer saw.
  Rationale: recording a tree the reviewer never looked at as reviewed would make the gate meaningless. HEAD's tree per repository must equal the tree recorded with the findings verdict; otherwise the tool says to call `request_review`.
  Date/Author: 2026-11-03 / plan author.
- Decision: no field for the primary to send text to the reviewer (no `disputes`, no `additional_context`).
  Rationale: the reviewer prompt deliberately excludes the primary agent's account of its own work (`mj-review/src/lanes.rs`, the `user_messages` paragraph). Opening that channel is a separate design decision and is out of scope here.
  Date/Author: 2026-11-03 / plan author.
- Decision: a user-facing Dismiss for before-push reviews is out of scope, but the host operation is written so a later surface can call it.
  Rationale: push reviews close as soon as their verdict is delivered, so there is no open review for the review view to dismiss; adding a surface for a closed review is UI work beyond this plan. The host method takes the session id and a reason and does not depend on the MCP tool.
  Date/Author: 2026-11-03 / plan author.

## Outcomes & Retrospective

Milestone 1 replaces model-generated JSON parsing with a plain-text contract and an explicit clean sentinel. Empty reports still fail closed, and JSON-shaped text such as `{}` is now findings. The rubric, reviewer handback instructions, MCP reviewer description, and saved-report fixtures agree on that contract. The parser/driver suite and the affected controller/worker checkpoint tests pass; later milestones remain.

Milestone 2 persists pending findings in SQL, refuses overrides for changed or incomplete repository sets, records an acknowledged audit request before unlocking refs, and completes admitted work through shutdown. Tests cover missing/changed/duplicate roots, audit refusal, failed-save retries, reservation ownership, cancellation, ordered persistence, and shutdown. The first test attempt exposed test-fixture setup mistakes, which were fixed and only the failing cases rerun before the final affected-module checkpoint.

Milestones 3 and 4 expose the audited override through the same push-only MCP gate, preserve its reason across transport/API boundaries, and document its exact-tree behavior. Compatibility revision 105 rejects readers that cannot decode the new durable action. The schema-fixture reruns use restored historical layouts, without changing committed migration 104. All checkpoint checks pass. The user-added parallel-review extension and final whole-workspace checks still remain before pushing.

## Context and Orientation

Mjolnir ("mj") is a Rust workspace. The pieces this plan touches:

- `mj-review/` is the crate that builds the reviewer's prompt and classifies its answer. `mj-review/templates/review_rubric.md` is the review rubric text (vendored from Codex and included with `include_str!` in `mj-review/src/lanes.rs`). `mj-review/src/lanes.rs::review_prompt` assembles the prompt: rubric, the `git diff` command the reviewer must run, a per-file diffstat, the user's messages, and on a corrective pass the prior findings. `mj-review/src/verdict.rs` turns the reviewer's text into a `ReviewVerdict`, which is defined in `mj-core/src/review/verdict.rs` as `Findings { synthesis: String, evidence }`, `Clean`, or `Failed { reason }`.
- `mj-controller/` is the daemon. `mj-controller/src/review_host/` is the "review host": the state machine that starts a reviewer child session, receives its answer, and delivers the verdict. Key files: `begin.rs` (`begin_push` starts a before-push review), `reviewer_child.rs` (registers the hidden reviewer child; `HANDBACK_INSTRUCTION` is appended to its prompt), `resolve.rs` (`finish_push` and `finish_push_delivery` deliver a push verdict and, when clean, record the tree), `events.rs` (`HostEvent`, `PushReview`, `PushCompletion`), `dispatch.rs` (routes `HostEvent`s), `host.rs` (the public `ReviewHost` handle: `begin_push`, `start`, `resolve`), `notices.rs` (`record_notice` writes a line into the primary session's transcript), `tests.rs`.
- `mj-core/src/storage.rs::TurnReviewState` is the per-session review state the daemon persists in its SQLite database (through `mj-controller/src/database/turn_reviews.rs`). Its `prior_review` field holds the last findings text so the next review is a corrective pass.
- The "mj-agents MCP server" is a small tool server the worker exposes to the agent's harness. `mj-worker/src/subagent_mcp.rs` defines the tools (`parent_tool_definitions` adds `request_review` when the session's review trigger is `push`), parses calls into `mj_core::subagent::SubagentToolAction`, and sends them to the daemon. `mj-core/src/subagent.rs` holds `SubagentToolAction` and its wire twin `SubagentToolActionWire`. `mj-controller/src/server_runtime/api.rs` executes actions; `SubagentToolAction::RequestReview {}` calls `self.exports.request_review(session_id)`, which calls `review_host().begin_push`. `mj-controller/src/daemon/delegation.rs` lists actions that run without upgrade admission. `mj-worker/src/acp/launch.rs` adds `mcp__mj-agents__request_review` to Claude Code's allowed tool list when the gate is on.
- The worker (`mj-worker/`) runs inside the session's container. The daemon asks it to capture the diff (`ReviewerRequest::CaptureDelta`, returning one `RepoDelta` per repository with `current_tree` and `head_commit`) and to record a reviewed tree (`ReviewerRequest::RecordReviewed { trees }` writes `refs/mj/reviewed/<tree>`). These are in `mj-core/src/relay/protocol.rs`; the daemon-side client is `mj-controller/src/worker_client/reviewer.rs` (`capture_review_delta`, `record_reviewed`, `advance_review_baseline`). The daemon reaches the worker through `SessionManagerControl::session(id).reviewer(ReviewerAction::…)`; `ReviewerAction` is in `mj-client/src/session.rs`. `resolve.rs::push_action` shows the call pattern.
- `mj-core/src/mailbox.rs` renders the verdict message the primary agent receives (`MailboxEventBody::ReviewVerdict`, around the `"Fix these, commit, and call request_review again"` text).
- Docs: `docs/src/content/docs/turn-review.md`, section "What the reviewer reads" and "Review before push".

Build and test with `cargo` from the repository root. This checkout uses a build cache tool (`mbx`); do not change `target/` locations or cargo configuration.

## Plan of Work

### Milestone 1: plain-text verdicts

Scope: the reviewer is asked for plain text, and the classifier reads it. Nothing else changes. At the end of this milestone every existing review path still works; tests that fed JSON answers feed text instead.

Edit `mj-review/templates/review_rubric.md`. Keep everything up to and including the "FORMATTING GUIDELINES" paragraph (what counts as a finding, the `[P0]`–`[P3]` title tags, one-paragraph descriptions). Delete the sentence that begins "Additionally, include a numeric priority field in the JSON output" and the sentence about outputting an "overall correctness" verdict. Replace the whole "OUTPUT FORMAT" section (from `OUTPUT FORMAT:` through the final bullet "Do not generate a PR fix.") with a plain-language section like:

    OUTPUT FORMAT:

    Write your findings as plain text, highest priority first. Start each finding on its own line with a dash, then the `[Pn]` tag and title, then the file path and the shortest line range that locates the problem (`path/to/file.rs:120-124`), then one paragraph explaining why it is a problem. Do not generate a fix.

    If there is no finding that a person would definitely want to fix, your entire answer must be exactly:

    No findings.

Update the comment above `CODEX_REVIEW_RUBRIC` in `mj-review/src/lanes.rs` (currently "Vendored verbatim from Codex commit …") to say the criteria are from that Codex commit but the output section was rewritten for plain text, and why (the structure was never used; see this plan).

Edit `HANDBACK_INSTRUCTION` in `mj-controller/src/review_host/reviewer_child.rs` to:

    "\n\nHand back your review as the complete text of a single handback call: plain-text findings as described above, or exactly `No findings.` when there is nothing material to report. Do not edit files or push. After calling handback, stop."

Rewrite `mj-review/src/verdict.rs`. Remove `ReviewOutput`, `ReviewFinding`, `CodeLocation`, `LineRange`, `ParsedReviewOutput`, `parse_review_output_event`, `format_review_findings_block`, `render_review_output_text`, and the `serde`/`PathBuf` imports they needed. Keep `pub use mj_core::review::verdict::*;`. Add one public constant, `pub const NO_FINDINGS: &str = "No findings.";`, and make `review_output_verdict` (keep the name; callers already use it) do exactly this: trim the text; if empty, return `Failed { reason: "the reviewer returned an empty report" }` as today; if it equals `NO_FINDINGS` ignoring ASCII case, return `Clean`; otherwise return `Findings { synthesis: bound_tail(trimmed, SYNTHESIS_LIMIT, "review output"), evidence: Default::default() }`. Update the module doc comment and the crate doc in `mj-review/src/lib.rs` (the line "follows Codex's `/review` rubric and JSON output contract") to say plain text with the `No findings.` sentinel.

Check `grep -rn "parse_review_output_event\|render_review_output_text\|format_review_findings_block\|ReviewOutput\b\|ReviewFinding\b" --include=*.rs .` (excluding `target/`) returns nothing outside the deleted code. At the time of writing the only user was `mj-review/src/verdict.rs` itself.

Replace the JSON tests in `mj-review/src/verdict.rs` with tests that: empty and whitespace-only text are `Failed`; `No findings.`, ` no findings. ` and `NO FINDINGS.` are `Clean`; `No findings. One nit: …` and any other prose are `Findings` with the trimmed text as `synthesis`; a synthesis longer than `SYNTHESIS_LIMIT` is bounded with the existing marker.

Update the fixtures that fed JSON answers: `mj-review/src/driver/tests.rs` (three uses of `{"findings":[]}`), `mj-controller/src/review_host/tests.rs` (around lines 1771, 2011 and 2380, answers such as `{"findings":[],"overall_explanation":"No issues found."}`), and `mj-controller/src/server_runtime/api/tests.rs` around line 1637, which builds a JSON answer with an oversized finding body to check that the stored verdict is bounded; rewrite it to use plain text of the same length. Also change `mj-review/src/lanes.rs` tests only if any assert on JSON wording in the prompt (the test `review_prompt_contains_the_codex_rubric_tree_ids_and_diff_command_without_a_diff` asserts rubric presence; make sure whatever substring it checks still exists).

Run, from the repository root:

    cargo test -p brokk-mj-review
    cargo test -p brokk-mj-controller review_host
    cargo test -p brokk-mj-controller server_runtime::api
    cargo clippy --workspace --all-targets -- -D warnings

Acceptance: all pass; a search for `overall_correctness` in the repository (excluding `target/`) finds nothing. Commit with a message such as "Reviewers answer in plain text; `No findings.` is the clean verdict".

### Milestone 2: the host records findings trees and can override them

Scope: after this milestone the daemon remembers which trees a findings verdict covered and exposes a `ReviewHost::override_push(session_id, reason)` method, with unit tests, but no tool calls it yet.

Add to `mj_core::storage::TurnReviewState` (in `mj-core/src/storage.rs`) a field:

    /// The trees a before-push findings verdict covered, so the primary can
    /// override that exact verdict. Cleared by a clean verdict or an override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_findings: Option<PushFindingsRecord>,

and define `PushFindingsRecord` next to it with `review_id: String`, `trees: BTreeMap<PathBuf, (String, String)>` (repository root → (tree id, head commit), the same shape `ReviewerRequest::RecordReviewed` takes), and `synthesis: String`. Derive the same traits `TurnReviewState` derives. Check `mj-controller/src/database/turn_reviews.rs` stores the state as JSON; if it stores fields in columns, add the column with a migration (follow the pattern of the newest `if version < N` block in `mj-controller/src/database/schema.rs`; find N with `grep -n "if version < " mj-controller/src/database/schema.rs`). A `serde(default)` field in a JSON blob needs no migration.

In `mj-controller/src/review_host/resolve.rs::finish_push_delivery`, when the verdict is `Findings`, set `state.push_findings = Some(PushFindingsRecord { review_id, trees, synthesis })` with trees built from `deltas` exactly as the clean branch builds them for `RecordReviewed`. In the clean branch, set `state.push_findings = None` next to `state.prior_review = None`. (`finish_push` in the same file sets `prior_review` for findings; `finish_push_delivery` is where `state` is finally saved, so set the new field there so both are in one durable write.)

Add the host operation. In `mj-controller/src/review_host/events.rs` add a `HostEvent::OverridePush { session_id: String, reason: String, reply: oneshot::Sender<Result<serde_json::Value, StartRefusal>> }`. In `host.rs` add `pub async fn override_push(&self, session_id: &str, reason: String) -> Result<serde_json::Value, StartRefusal>` following `begin_push`'s send-and-await pattern. In `dispatch.rs` route it to a new `HostState::override_push` in `begin.rs` (or a new `override.rs` module declared in `mod.rs`, whichever keeps files small). That handler:

The host must reserve the session before starting asynchronous work. Replace the parallel preparation membership and cancellation facts with one typed per-session preparation owner that distinguishes cancellable reviewer preparation from override work. The override task reads state behind prior FIFO persistence writes and awaits a FIFO save before releasing ownership. During shutdown, stop cancellable capture, let already-admitted override mutations finish, and keep their persistence sender alive until they finish. No filesystem or database operation may block the host event loop.

1. Refuses (with a `StartRefusal` sentence, like `begin_push` does) when: the session's trigger is not `push` ("this session's review trigger is not push"); a move owns the session; a review is open, preparing, or recovering for the session ("a review is open; wait for its verdict or cancel it"); the reason is empty after trimming ("override_review needs a reason").
2. Loads the session's `TurnReviewState` the way `begin_push`/`prepare_scoped` do (through `self.environment`), and refuses if `push_findings` is `None` ("no findings verdict is pending; call request_review").
3. Captures the current pushable delta through the same worker call the push review uses (`capture_review_delta` with `CaptureScope::PushableCommits` and `state.baselines`; see `prepare_scoped` in `reviewer.rs` for how the host reaches it, and `push_action` in `resolve.rs` for the timeout pattern). Compare: the set of repository roots must equal the keys of `push_findings.trees`, and each delta's `current_tree` must equal the recorded tree. On mismatch refuse with "HEAD changed since the last verdict; call request_review". Time-box the whole operation like `begin_push` (90 s) and refuse on timeout.
4. On match: take `activity_unless_draining("push review override")` admission and retain it through the acknowledged mutation and state save. Submit an acknowledged, bounded `RelayCommand::RecordNotice` before changing reviewed refs; refuse on failure so an unaudited override cannot unlock a push. Its text is:

        Before-push review override requested by the agent. Reason: <reason>
        Overridden findings (review <review_id>, commits <base>..<head> per repository):
        <synthesis>

   Keep the notice bounded (reuse `bound_tail` with `SYNTHESIS_LIMIT`). Then call `ReviewerAction::RecordReviewed { trees }` and `ReviewerAction::AdvanceBaseline { trees }` with the same success/failure handling as the clean branch of `finish_push_delivery` (a `RecordReviewed` failure is a refusal; an `AdvanceBaseline` failure is logged). Update `baselines` to the trees, clear `prior_review` and `push_findings`, and await saving through the FIFO persistence lane before releasing the session owner. Reply with `{"accepted": true, "review_id": …, "repositories": [{"repository": …, "tree": …, "head_commit": …}]}`.

Write host tests in `mj-controller/src/review_host/tests.rs`, modelled on `push_review_delivers_each_verdict_without_holding_or_prompting_the_primary` and `open_push`: a findings verdict leaves `push_findings` populated in the saved state and a clean verdict clears it; `override_push` while a review is open is refused and changes nothing; `override_push` with a mismatched `current_tree` is refused and records no reviewed tree; a matching override calls `RecordReviewed` with the recorded trees, clears `prior_review` and `push_findings`, records a notice containing the reason, and a following `begin_push` is not a corrective pass (its prompt has no `<prior_review_findings>` section). Look at how the existing tests fake the environment and the worker (`ReviewEnvironment` implementations in that file) and extend the fake to answer the capture with a configurable `current_tree`.

Run:

    cargo test -p brokk-mj-core
    cargo test -p brokk-mj-controller review_host

Acceptance: tests pass. Commit: "Record the trees of a before-push findings verdict and let the host override it".

### Milestone 3: the `override_review` tool

Scope: the agent can call the tool. After this milestone the end-to-end scenario in Purpose works.

In `mj-core/src/subagent.rs` add `OverrideReview { reason: String }` to `SubagentToolAction` and `SubagentToolActionWire`, map between them where `RequestReview` is mapped, and include it in `mutates_child_state` next to `RequestReview`. Read the comment above the `if version < 97` block in `mj-controller/src/database/schema.rs`: a new durable action variant means older daemons cannot decode queued actions, so add a new `if version < N+1` block that raises `minimum_compatible_version` the same way, where N is the current newest version.

In `mj-worker/src/subagent_mcp.rs`: in `parent_tool_definitions`, next to `request_review`, add

    override_review — "Override the last before-push findings verdict and record the reviewed tree so the push can proceed. Use it only for findings you have judged incorrect or unreasonable; the reason is written into the session transcript. Refused if a review is running or if HEAD changed since the verdict."

with schema `{"type":"object","properties":{"reason":{"type":"string","description":"Why the findings do not need to be addressed."}},"required":["reason"],"additionalProperties":false}`. Allow it under the same `push_review` condition as `request_review` (the check near `params.name == "request_review" && role == Parent && options.review_gate`), parse it into `SubagentToolAction::OverrideReview { reason }`, and refuse an empty reason at parse time like `spawn` refuses empty instructions. Extend the existing test that checks `request_review` appears only with the gate on (around line 1168) to cover `override_review`, and keep `REVIEW_GATE_INSTRUCTIONS` under the 2048-character limit that test asserts.

In `mj-worker/src/acp/launch.rs` add `mcp__mj-agents__override_review` beside `mcp__mj-agents__request_review`, and update the expectation in `mj-worker/src/acp/tests.rs` (around line 441).

In `mj-controller/src/server_runtime/api.rs`: add `fn override_review(&self, session_id: String, reason: String)` to the exports trait beside `request_review` (default implementation bails "push review is unavailable"), implement it by calling `review_host().override_push(&session_id, reason)` and mapping the refusal to `anyhow!("override_review refused: {}", error.0)`, and handle `SubagentToolAction::OverrideReview { reason }` in the match where `RequestReview` is handled. In `mj-controller/src/daemon/delegation.rs` add `OverrideReview` to the list beside `RequestReview` so it is not blocked by upgrade admission. Search the controller for other exhaustive matches on `SubagentToolAction` (`cargo check` will list them) and treat `OverrideReview` like `RequestReview` in each.

Run:

    cargo test -p brokk-mj-core
    cargo test -p brokk-mj-worker subagent_mcp
    cargo test -p brokk-mj-worker acp
    cargo test -p brokk-mj-controller
    cargo clippy --workspace --all-targets -- -D warnings

Acceptance: tests pass. If a live `push`-trigger session is available (see `tests/e2e/review_before_push.py` for how one is set up), the scenario from Purpose works: after a findings verdict, `override_review` with a reason returns `accepted: true`, `git push` succeeds, and the transcript shows the override notice. Commit: "Add the override_review tool for before-push findings".

### Milestone 4: user-facing text

In `mj-core/src/mailbox.rs`, the findings message currently ends "Fix these, commit, and call request_review again before pushing." Append: " If a finding is wrong or unreasonable, call override_review with your reason instead." Update the snapshot or assertion test for that message in the same file. In `mj-worker/src/subagent_mcp.rs::REVIEW_GATE_INSTRUCTIONS` add one sentence: "override_review records a findings verdict you judge incorrect as reviewed; give the reason."

In `docs/src/content/docs/turn-review.md`: rewrite the first paragraph of "What the reviewer reads" to say the reviewer follows the Codex review rubric and answers in plain text; a reply of exactly `No findings.` is a clean review, any other text is the findings shown to the agent and the user, and an empty reply is a failed review. In "Review before push", after the sentence about a findings verdict, add a paragraph describing `override_review`: what it does, that it applies only to the exact commits the reviewer saw, that the reason and the overridden findings appear in the transcript, and that it is refused while a review is running. Update `mj-core/assets/skills/mj/references/configuration.md` only if it describes the reviewer's output format (at the time of writing it does not).

Run `cargo test -p brokk-mj-core mailbox` and `cargo test -p brokk-mj-worker subagent_mcp`. Build the docs if the repository has a docs check (look in `docs/package.json`). Commit: "Describe plain-text verdicts and override_review".

## Concrete Steps

Work from the repository root in this order: Milestone 1, 2, 3, 4, committing after each. Before each commit run `cargo fmt --all` and the tests named in the milestone. Fill in `Progress` with timestamps at each stopping point and record anything unexpected in `Surprises & Discoveries`.

## Validation and Acceptance

The whole change is accepted when:

- `cargo test --workspace` passes (or the failures are shown to be pre-existing and unrelated, with evidence).
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- `grep -rn "overall_correctness\|parse_review_output_event" --include=*.rs --include=*.md . | grep -v target/` finds only this plan.
- The host tests prove: `No findings.` → clean and the tree is recorded; prose → findings and `push_findings` is saved; empty → failed; override refused while a review is open, refused when HEAD moved, accepted and recorded when the trees match, with a transcript notice containing the reason.
- The tool appears in `parent_tool_definitions` only when the review gate is on, and is in Claude Code's allowed tools under the same condition.

## Idempotence and Recovery

Every step is an ordinary code edit and can be repeated. The new `TurnReviewState` field is `serde(default)`, so existing stored states load unchanged. The schema compatibility bump only refuses older daemons from reading a database a newer daemon has written, which is the existing policy for new durable action variants. An override that fails after `RecordReviewed` succeeded but before the state is saved leaves a reviewed ref and a stale `push_findings`; a repeated `override_review` call then records the same tree again (idempotent) and clears the state, so the recovery is to call the tool again.

## Artifacts and Notes

Record here the final wording of the rubric output section, the `HANDBACK_INSTRUCTION`, the tool description, and any test output that proves the override path.

## Interfaces and Dependencies

New or changed public surface:

- `mj_review::verdict::NO_FINDINGS: &str` and the new behavior of `mj_review::verdict::review_output_verdict(text) -> ReviewVerdict`.
- `mj_core::storage::TurnReviewState::push_findings: Option<PushFindingsRecord>` and `mj_core::storage::PushFindingsRecord`.
- `mj_core::subagent::SubagentToolAction::OverrideReview { reason: String }` and its wire form.
- `mj_controller::review_host::ReviewHost::override_push(&self, session_id: &str, reason: String) -> Result<serde_json::Value, StartRefusal>`.
- The `override_review` MCP tool in the mj-agents server, offered to parent sessions whose review trigger is `push`.

No new crates or external dependencies.

Implementation note (2026-11-03): recorded explicit user authorization to push, the current upstream schema revision, the additional reviewer-specific MCP JSON instruction, and the repository-required incremental validation schedule.

Implementation note (2026-11-03): corrected Cargo package selectors after the first validation command showed that directory names are not package names.

Implementation note (2026-11-03): strengthened override ownership and persistence ordering after tracing the cancellation/write race, and moved acknowledged audit delivery before reviewed-ref mutation. The ordinary courtesy `record_notice` is not used as the override audit guarantee.

Implementation note (2026-11-03): recorded the validated plaintext checkpoint commit and allowed disjoint implementation slices to overlap without changing commit order or final acceptance requirements.

Implementation note (2026-11-03): corrected the SQL storage assumption and split schema revisions 104 and 105 between the storage and action checkpoints. The user also requested configurable parallel reviewers; a companion ExecPlan tracks that extension, and final validation waits until the extension is implemented.

Implementation note (2026-11-03): fixed historical downgrade fixtures instead of rewriting committed migration 104. Clarified the override tool description to refer to changed tree contents, matching its actual predicate rather than implying commit-message-only amendments invalidate the reviewed snapshot.
