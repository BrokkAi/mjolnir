# Escalate Mjolnir Guardian Denials to Human Approval Requests

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan follows `.agents/PLANS.md` from the repository root and must be maintained in accordance with it.

## Purpose / Big Picture

Today, when a coding harness running under configured approvals forwards a tool permission request over ACP (`session/request_permission`) to Mjolnir, the Mjolnir guardian evaluates it and either allows it once, escalates on an internal failure or timeout, or rejects it outright with `RejectOnce` and a transcript notice (`Mjolnir guardian denied ...`). On harnesses like OpenCode, that immediate refusal even ends the turn; on all harnesses, a false-positive denial interrupts the agent without giving the user an approval button or triggering Mjolnir's notification stack.

After this change, permission review escalation works as a three-stage chain:

    native [optional, not all harnesses have it] -> mj -> human

When a harness has a native reviewer (such as Codex `agent`, Claude `auto`, Antigravity `auto`, Muse `auto_review`, or Grok), any request it forwards over ACP reaches the Mjolnir guardian; harnesses without a native reviewer (Kimi Code, OpenCode, DSH) reach the Mjolnir guardian directly. Note that native denials handled internally inside a harness (such as Claude `auto`, Codex `agent`, or Muse `auto_review`) never cross ACP, so they remain final inside the harness; the chain governs everything that reaches Mjolnir over ACP. Whenever the Mjolnir guardian does not approve a request—whether its verdict is `Deny` (including policy-enforced `high`/`critical` denials) or the review fails, times out, or is disabled—Mjolnir escalates the request to a standard human approval form (`ElicitationRequest` with `tool_permission` metadata) instead of rejecting it outright. Because that form uses Mjolnir's canonical tool-permission elicitation channel, it integrates directly with the existing notifications system: TUI Notifications pane, terminal title counts and bell, OS desktop notifications, Web Inbox inline approval cards, Web conversation forms, and Web Push, for top-level sessions, managed children, oracles, and before-push reviewers. When Operator Away is enabled, Mjolnir's existing away handling skips each distinct flagged request once with a one-time rejection (leaving an identical retry pending for the person) without recording a human decision.

A user can verify this behavior by running a session under configured approvals (for example on `localhost` with Kimi Code, OpenCode, DSH, or a Claude `manual`/`accept-edits` profile), triggering an action the Mjolnir guardian denies, observing the desktop/push/TUI/Web notification and approval card showing the guardian's risk level and rationale alongside the requested tool and command, and clicking Allow Once (so the command runs) or Reject/Skip (so the command is refused once).

## Progress

- [x] (2026-11-24 15:48Z) Mapped the existing guardian review pipeline, per-harness native enforcement, tool-permission elicitation lifecycle, Operator Away handling, and notification surfaces (TUI, desktop, Web Inbox, Web Push, child sessions).
- [x] (2026-11-24 15:52Z) Authored initial ExecPlan in `.agents/plans/guardian-review-escalation.md` and sent it to advisor session `91233b0efdd193066e7b4eeb46c8db10` for review.
- [x] (2026-11-24 16:12Z) Incorporated advisor review items A, B, C, and E into the ExecPlan, audited Item D (all unwatched/headless session kinds), and sent the Item D findings to advisor session `91233b0efdd193066e7b4eeb46c8db10` for final sign-off before coding.
- [x] (2026-11-24 16:13Z) Received advisor ruling on Item D: proceed with unified worker escalation across all session kinds, with plain operational docs and user notice.
- [x] (2026-11-24 16:16Z) Milestone 1: Implemented `denied_escalation_preface` in `mj-core/src/guardian.rs`, `AllowAlways` suppression on guardian-flagged forms in `mj-worker/src/acp/permissions.rs`, and `Deny -> human approval form` escalation, delivered-refusal circuit breaker, and `mj_guardian` resolution logging in `mj-worker/src/acp/drive.rs` and `mj-worker/src/acp/guardian/reviewer.rs`.
- [x] (2026-11-24 16:31Z) Milestone 2: Updated and added worker tests in `mj-worker/src/acp/guardian_tests.rs` (including `AllowAlways` filtering, breaker-by-final-outcome with 3 distinct Operator Away requests + transcript line-1 check, and before/after settle withdrawal); all 15 `acp::guardian::tests`, 22 `brokk-mj-core` guardian tests, and 2 `claude_plan_approval` tests pass, and `mbx clippy -p brokk-mj-core -p brokk-mj-worker-runtime --all-targets -- -D warnings` is clean.
- [x] (2026-11-24 16:35Z) Milestone 3: Ran 9 constructed Stage-2 follow-up evaluation cases via `guardian_eval_tool stage2-utility` (9/9 matched expected allow/deny) and updated `docs/src/content/docs/security.md`.
- [ ] Milestone 4: Commit on `master`, complete before-push review via `request_review`, and push to `origin/master`.

## Surprises & Discoveries

- Observation: `HarnessKind::mjolnir_guardian_reviews` in `mj-core/src/config/harness.rs` already returns `policy == ExecutionPolicy::ConfiguredApprovals && choices.mjolnir_guardian` for every harness (and `mjolnir_guardian` defaults to `true` on every profile, alongside global `[guardian].enabled = true`).
  Evidence: `mj-core/src/config/harness.rs:1352-1359`, `mj-core/src/config/harness.rs:1701-1702`, `mj-core/src/config.rs:46-68`. The `native [optional] -> mj` half of the chain is already wired for every ACP `session/request_permission` a harness emits.

- Observation: None of the five native-reviewer harnesses (Codex `agent`, Claude `auto`, Muse `auto_review`, Antigravity `auto`, Grok) currently expose a configuration flag in Mjolnir that converts an internal native denial into an ACP `session/request_permission`.
  Evidence: Inspected `mj-core/src/config/harness.rs:1063-1294`, `docs/src/content/docs/security.md:68-103`, `.agents/plans/codex-guardian-fork.md`, and `mj-harness-install/assets/`. Harnesses that ask over ACP flow into Mjolnir guardian; internal native denials that never cross ACP remain final inside the harness.

- Observation: Turn withdrawal is sticky per turn (`withdrawn_turns` in `mj-worker/src/acp/operator_away.rs:526-560`), and `PendingElicitations::register` already checks `withdrawn_turns` under the same mutex for tool-permission forms (`operator_away.rs:327-333`).
  Evidence: Advisor review item A and `mj-worker/src/acp/drive.rs:531-532`. Therefore no new `escalate_to_form` / `register_locked` locking helper is needed; settling the `Deny` review via the existing `ticket.settle(|withdrawn| ...)` and falling through to the existing `register` call at `drive.rs:758` (with no `.await` in between on the non-plan path) has no race.

- Observation: Operator Away only auto-declines a tool-permission request whose identity (`kind + title + input`, `operator_away.rs:767-779`) is not already in `refused` (`operator_away.rs:335`).
  Evidence: `mj-worker/src/acp/operator_away.rs:334-347, 767-779`. An identical request retried while away stays pending for the person rather than auto-declining a second time; distinct requests are each skipped once.

- Observation: `wolf_action_text` strips a form header only when line 1 ends with `" requests permission:"` (`mj-core/src/guardian.rs:887-898`).
  Evidence: A prefaced form message does not match that strip rule, so any future offline corpus built from stored form messages would carry the guardian's own verdict and rationale into the case text unless corpus builders strip the preface. Live Stage-1 classification is unaffected (`guardian.rs:824-863`).

- Observation: Audit of "sessions nobody is watching" (Item D): every live session kind (`Worker` sub-agent, `Oracle` sub-agent, before-push `Reviewer` sub-agent, `Goal` turn, and ordinary `mj new` session) already routes `tool_permission` elicitations to a human-visible, answerable notification card.
  Evidence:
  - Managed children (`SubagentKind::Worker`): inherit parent's `execution_policy` (`mj-controller/src/controller/subagents.rs:445-451`, `mj-worker-staging/src/worker_binary/launch.rs:281,502-549`) and launch with `notifications_ignored: true` (`subagents.rs:443`). Because `ElicitationRequest::goes_to_parent()` is `false` for `tool_permission: Some(...)` (`mj-core/src/elicitation.rs:75-86`), `mj-daemon/src/daemon/attention.rs:502-572,924-944` marks it `person_owned`, walks up `runtime.subagents` to the first active unmuted ancestor, and publishes a `NeedsInput` card (`holder = child_id`) with TUI/desktop/Web Inbox/Web Push notifications, answered via `terminal_respond_elicitation` (`mj-daemon/src/daemon/terminal_operations.rs:230-236`).
  - Before-push reviewer sessions (`SubagentKind::Reviewer`): inherit reviewed session's `execution_policy` (`mj-controller/src/controller/reviewer.rs:27-56`, `mj-daemon/src/review_host/selection.rs:59-82`) and Guardian is active on bare/guardian targets. `TurnReviewHost::publish` (`mj-review-host/src/review_host/notices.rs:88-105`) projects every non-plan pending elicitation into `RuntimeReviewView.questions`; `mj-daemon/src/server_runtime/snapshot.rs:709-716` merges them onto the reviewed session's `pending_elicitations`; `attention.rs:517-572,932-958` creates a `NeedsInput` card on the reviewed session (firing TUI/desktop/Web Inbox/Web Push, covered by RVC-3 in `mj-review-host/src/review_host/tests.rs:2411-2450`); `terminal_respond_elicitation` (`terminal_operations.rs:219-236`) resolves both direct `(child, id)` and encoded `(parent, reviewer_question_id)` answers. Reviews have no wall-clock turn timeout (`LIFECYCLE_WAIT = 300s` in `mj-review-host/src/review_host/reviewer.rs:61` only applies to foreground lifecycle preemption).
  - Oracle sessions (`SubagentKind::Oracle`): select `Unconstrained` on YOLO targets and `ConfiguredApprovals` on bare/guardian targets (`mj-daemon/src/server_runtime/api/oracle.rs:47-51`), with Guardian active on bare/guardian targets and `READ_ONLY_TASK_SCOPE` in the prompt (`subagents.rs:404-409`). Registered with `notifications_ignored: true` (`subagents.rs:443`), listed in TUI Sub-agents view (`mj-tui/src/dashboard_workspaces.rs:53-54`, `row_index.rs:110-113`), and handled identically to `Worker` in `attention.rs:502-572,932-944` — its tool-permission forms bubble up to the primary session's `NeedsInput` card and notifications and are answerable via `terminal_respond_elicitation`.
  - `mj new --prompt-file` + `mj wait`: launches with target's `execution_policy` and `notifications_ignored = args.ignore_notifications` (default `false`, `mj-cli/src/api_commands.rs:1357`). In `mj wait`, `--return-on-input` defaults to `false` (`api_commands.rs:385-388`): with `--return-on-input` it returns immediately with `WaitOutcome::InputRequired` and prints `pending_elicitations` (`mj-server/src/server/api/wait.rs:540`, `api_commands.rs:1842-1855`); without `--return-on-input` it blocks until `--timeout` (or indefinitely if omitted) and on timeout prints the `mj elicitations` / `mj respond` hint (`api_commands.rs:1948-1953`). If combined with `--ignore-notifications` and no `--return-on-input` / `--timeout` on a bare/guardian target with Away off, it parks silently until answered or cancelled.
  - Goal turns (`/goal`): run in the ordinary session worker (`mj-worker/src/acp/goal.rs`) with the session's `execution_policy` and Guardian; forms notify the user normally. On a bare/guardian target with Operator Away off, a Guardian denial parks the unattended goal turn until the user answers or turns Away on (which drains pending unrefused tool forms).
  - Internal ACP processes: model discovery probes (`mj-worker/src/worker_runtime/discovery.rs:83-195`) never send a prompt or run tools; the ACP fallback guardian reviewer (`mj-worker/src/acp/guardian/reviewer.rs:815-821`) sits outside `drive.rs` and unconditionally refuses its own permission requests with `rejection_response`.

- Observation: Filtered runs of `mj-worker`'s `guardian_tests` timed out on a loaded host because `start_scripted_harness` did not call `mj_harness_install::login_environment::initialize_from_parent()`, causing every test to wait on an interactive login shell probe (whereas full-suite runs hit `initialize_from_parent()` in `acp/tests.rs` or `acp/goal.rs` first).
  Evidence: Adding `let _ = mj_harness_install::login_environment::initialize_from_parent();` to `start_scripted_harness` (matching `acp/tests.rs:9613`, `acp/goal.rs:549`, and `worker_runtime/unix.rs:5370`) made all 15 `acp::guardian::tests` pass in 0.29s.

- Observation: Stage-2 follow-up evaluation (9 constructed cases via `guardian_eval_tool stage2-utility`, `codex4`/`gpt-6-luna`) scored 9/9 with zero over-generalization and zero false re-denials.
  Evidence:
  - Identical action repeated after human approval (`approved-repeat-push`, `approved-repeat-rm`, `approved-repeat-deploy`): 3/3 `allow` (`user_authorization: high`).
  - Broader/more destructive variant after narrow approval (`approved-escalate-main`, `approved-escalate-home`, `approved-escalate-prod`): 3/3 `deny` (`user_authorization: low`/`unknown`, explicitly noting one-time approval covered only the narrower target).
  - Identical action repeated after human rejection (`rejected-repeat-push`, `rejected-repeat-rm`, `rejected-repeat-deploy`): 3/3 `deny` (`user_authorization: low`).

- Observation: `go/RUST_TARGET` pins the Go worker port to an older Rust commit (`5b68df737775f748e6d3e9207930e5467c31e397`, replayed through `3dde0c11b5832be0a9d810662216d82f9d7e9696`), so normal Rust feature work must not edit `go/`.
  Evidence: `go/RUST_TARGET:1-9`.

## Decision Log

- Decision: Escalate `GuardianDecision::Deny` in the worker (`mj-worker/src/acp/drive.rs`) while keeping the daemon-to-worker relay protocol (`GuardianReviewOutcome::Deny`) and the Guardian LLM prompt/output contract (`outcome: "allow" | "deny"`) unchanged.
  Rationale: Preserves Protocol 82 compatibility without a schema/protocol bump, retains typed `risk_level`, `user_authorization`, `rationale`, `stage`, and `stage1_score` telemetry, and leaves the calibrated security policy prompt untouched.
  Date/Author: 2026-11-24 / Coordinator (approved by Advisor)

- Decision: Reuse the existing `ticket.settle(|withdrawn| ...)` to record `PriorGuardianReview` and journal `RuntimeEvent::GuardianReviewed` on `Deny` (recording nothing and answering `Cancelled` if withdrawn or cancelled), then set `guardian_preface` and fall through to the existing `permission_elicitations.register(...)` at `drive.rs:758`. Do not add `escalate_to_form` or `register_locked`, and leave `AskPerson` unchanged.
  Rationale: Turn withdrawal is sticky in `withdrawn_turns` (`operator_away.rs:327-333,526-560`), and there is no `.await` between the end of the guardian block and `register` on the non-plan path. Withdrawn before settle gives `Cancelled` and no record; withdrawn between settle and register gives a truthful `Deny` record and a form that answers `Cancelled`. Avoids adding a new locking helper, closure-under-lock, or redundant liveness predicate.
  Date/Author: 2026-11-24 / Advisor review item A (adopted)

- Decision: Single breaker rule — the per-turn circuit breaker (`TurnContext::record_guardian_review`, 3 consecutive or 10 of 50) counts refusals actually delivered to the agent. Guardian `Allow` records `false` at settle as today; an escalated `Deny` records `PriorGuardianReview` and `GuardianReviewed` at settle and calls `record_guardian_review` at form resolution strictly before `responder.respond`: human `Allowed` records `false`, human `Rejected` or `OperatorAway` records `true`, and `Cancelled`/closed records nothing. Update the tripped breaker reason at `reviewer.rs:120` to reflect that several flagged requests were declined in this turn. Leave `RecentPriorDeny` untouched.
  Rationale: Prevents 3 human-approved escalations from tripping the breaker and forcing later safe commands to interrupt the user, while still tripping on repeated rejections or distinct Operator Away declines. Leaves the calibrated `RecentPriorDeny` deterministic guard untouched so Stage 2 evaluates follow-up actions with the human's answer in context.
  Date/Author: 2026-11-24 / Coordinator + Advisor review item B

- Decision: Policy choice — do not offer `AllowAlways` options on a guardian-flagged form when the request offers an `AllowOnce` option; implement this inside `permission_choices` (`mj-worker/src/acp/permissions.rs`) via a parameter. When the harness offers no `AllowOnce`, leave the options unchanged so the person can still approve.
  Rationale: `AllowAlways` creates a standing rule inside the harness so later matching calls bypass ACP and the guardian never sees them (the same reason Guardian itself only uses `AllowOnce`, `drive.rs:516`). The person's one-time approval still propagates to later Stage-2 reviews via `TrustedUserInput` (`permission_allow`). Name this policy choice explicitly in the final report to the user since it is theirs to veto.
  Date/Author: 2026-11-24 / Advisor review item C (adopted)

- Decision: Format the Guardian-denied escalation preface via `denied_escalation_preface(risk_level, rationale)` in `mj-core/src/guardian.rs` as: `Mjolnir guardian flagged this as <risk> risk: <rationale>\n` (or `Mjolnir guardian flagged this request: <rationale>\n` if `risk_level` is `None`), collapsing internal whitespace/newlines and bounding `rationale` with `mj_core::assessment::trim_middle`, and adding a comment that corpus builders must strip this preface because `wolf_action_text` only strips headers whose first line ends with `" requests permission:"`. Remove `denial_notice` and the `RuntimeEvent::Notice` emission on `Deny`.
  Rationale: Fits inside `web_push::phone_line`'s 180-character single-line body without agent-controlled commands pushing the reason out, prevents unbounded rationales from evicting older `TrustedUserInput` entries under `retained_count` (`guardian.rs:317,392-403`), and avoids posting a misleading `"Mjolnir guardian denied"` notice before the human decides.
  Date/Author: 2026-11-24 / Advisor review Q3 & Q4 (adopted)

- Decision: Emit one `tracing::info!(target: "mj_guardian", ...)` log line when a guardian-flagged form resolves, carrying `review_id`, `form_id`, `tool_call_id`, and `outcome` (`"allowed"`, `"rejected"`, `"away"`, or `"cancelled"`), without adding any field to persisted types.
  Rationale: Connects each Guardian denial (`review_id`) to the person's ground-truth answer (`form_id`, `tool_call_id`, `outcome`) for future false-positive evaluation without changing persisted schemas or relay wire formats.
  Date/Author: 2026-11-24 / Advisor review Q4 (adopted)

- Decision: Keep `rejection_response` for the ACP fallback reviewer's own sub-process (`mj-worker/src/acp/guardian/reviewer.rs:815-821`) and keep native harness enforcement modes unchanged in `mj-core/src/config/harness.rs`.
  Rationale: The read-only ACP reviewer must always refuse its own tool permission requests rather than escalating them to the human. Native modes remain untouched; docs and the user report will state plainly that internal native denials (Claude `auto`, Codex `agent`, Muse `auto_review`) never cross ACP and remain final.
  Date/Author: 2026-11-24 / Coordinator + Advisor review Q1

- Decision: Keep unified worker escalation across all session kinds (`Worker`, `Reviewer`, `Oracle`, `Goal`, `mj new`/`mj wait`), with no per-kind exception, no new timeout, no auto-decline for unwatched sessions, and no change to `mj wait` defaults. Document the operational impact and quiet cases clearly in `docs/src/content/docs/security.md` and the user report.
  Rationale: Every live session kind already surfaces `tool_permission` elicitations to a human-visible, answerable notification card and participates in global Operator Away (manual or `advanced.auto_afk_minutes`, draining pending forms when enabled). Adding a per-kind exception would create two rules for the same question.
  Date/Author: 2026-11-24 / Advisor ruling on D (adopted)

## Outcomes & Retrospective

Implemented the worker-side `native [optional] -> mj -> human` escalation path without changing Protocol 82 wire formats, the calibrated Stage-1/Stage-2 policy prompts, or daemon/store notification code. Guardian denials now settle `PriorGuardianReview` + `GuardianReviewed` via the existing `ticket.settle`, escalate into the canonical `tool_permission` elicitation form (omitting `AllowAlways` when `AllowOnce` is present, with line 1 carrying the risk level and `trim_middle`-bounded rationale for compact desktop/push notifications), count only delivered refusals in the per-turn circuit breaker, and emit `mj_guardian` resolution telemetry linking `review_id`, `form_id`, `tool_call_id`, and `outcome`. Validated with 22 `brokk-mj-core` guardian tests, 15 `brokk-mj-worker-runtime` guardian tests, 2 `claude_plan_approval` tests, clean clippy (`-D warnings`), and 9/9 constructed Stage-2 follow-up evaluations.

## Context and Orientation

Mjolnir splits permission enforcement across two planes:

1. The **worker** (`mj-worker`, one process per session) speaks ACP (Agent Client Protocol) to the coding harness bridge (`mj-worker/src/acp/drive.rs`). When a harness needs permission to run a tool or command under `ExecutionPolicy::ConfiguredApprovals`, it sends an ACP `session/request_permission` request. After checking plan-approval special cases and pre-approved literal `gh` invocations, `drive.rs` invokes `Guardian::review` (`mj-worker/src/acp/guardian/reviewer.rs`) whenever `HarnessKind::mjolnir_guardian_reviews` is true (`policy == ConfiguredApprovals && choices.mjolnir_guardian`).
2. The **daemon** (`mj-daemon` + `mj-session-runtime`) receives queued `GuardianReviewRequest` payloads over the worker relay (Protocol 82+, `mj-session-runtime/src/session_manager/standalone.rs:1419-1571`). It runs the optional approve-only local Wolf Defender Jev Stage-1 classifier (`WOLF_STAGE1_ALLOW_THRESHOLD = 0.010`), then escalates non-allowed requests to the configured direct Utility LLM (or returns `UseAcpFallback` so the worker runs an isolated read-only ACP reviewer). Both Stage-2 reviewers enforce `Assessment::enforced_outcome()` (`mj-core/src/guardian.rs:1483-1491`: `critical` always `Deny`, `high` with `unknown`/`low` user authorization always `Deny`).
3. **Human approval requests** are worker-owned ACP form elicitations (`mj_core::elicitation::ElicitationRequest` with `tool_permission: Some(ToolPermissionForm { .. })`). In `mj-worker/src/acp/drive.rs:706-813`, the worker builds the form choices via `permission_choices` (`mj-worker/src/acp/permissions.rs`), registers the live oneshot responder in `PendingElicitations` (`mj-worker/src/acp/operator_away.rs`), emits `RuntimeEvent::ElicitationRequested`, awaits the answer or cancellation, emits `RuntimeEvent::ElicitationResolved` (recording `ManualPermissionDecision::Allowed` or `Rejected` into `TurnContext` and durable relay snapshot), and answers ACP.
4. **Notifications** are driven entirely from durable pending elicitations projected by the daemon. `mj-daemon/src/daemon/attention.rs` builds a `NeedsInput` `NotificationCard` carrying `CardForm::elicitation` for every person-owned pending form (including managed children, oracles, and before-push reviewers). `mj-daemon/src/daemon/push_delivery.rs` and `mj-store/src/web_push.rs` reconcile those cards into durable notify-once alert events and Web Push payloads using `phone_line(form.message)` for the 180-character single-line alert body. TUI (`mj-tui/src/notify.rs`, `mj-tui/src/notifications_pane.rs`, `mj-cli/src/dashboard/surface.rs`, `mj-chat/src/chat/elicitation.rs`) and Web (`mj-server/src/web/viewer.js`, `mj-server/src/web/service-worker.js`) consume those alerts and cards to render desktop notifications, terminal bell/title counts, Web Push toasts, and interactive Approve/Reject forms.

Key files involved in this change:

- `mj-core/src/guardian.rs` — Guardian types, `RiskLevel`, `GuardianReviewOutcome`, preface helpers (`ask_person_preface`, replacing `denial_notice` with `denied_escalation_preface`).
- `mj-worker/src/acp/guardian.rs` — Worker-side `GuardianDecision` enum, `allow_once_response`, `rejection_response`, and helper re-exports.
- `mj-worker/src/acp/guardian/reviewer.rs` — `Guardian::review`, tripped-breaker wording, `record_settled_review` (for settled `Allow`), new `record_escalated_review` (prior review + `GuardianReviewed` without breaker) and `record_breaker_outcome`.
- `mj-worker/src/acp/permissions.rs` — `permission_choices`, dropping `AllowAlways` options on guardian-flagged forms when `AllowOnce` is present.
- `mj-worker/src/acp/drive.rs` — ACP `RequestPermissionRequest` handler (`:487-814`) settling `Deny` records via existing `ticket.settle`, falling through to the existing tool-permission form path, updating the breaker strictly before `responder.respond`, and logging ground-truth resolution telemetry.
- `mj-worker/src/acp/guardian_tests.rs` — Worker tests covering escalation, AllowAlways filtering, breaker-by-final-outcome, Operator Away (3 distinct requests + transcript line-1 check), and turn withdrawal.
- `docs/src/content/docs/security.md` — Human-facing documentation of the Mjolnir guardian escalation flow.

## Plan of Work

### Milestone 1: Core Preface, Permission Choices, and Worker Escalation

Update `mj-core/src/guardian.rs`:
- Expose `RiskLevel::as_str(self) -> &'static str` as `pub`.
- Add `pub fn denied_escalation_preface(risk_level: Option<RiskLevel>, rationale: &str) -> String`:
  - Collapse internal whitespace/newlines in `rationale` (`rationale.split_whitespace().collect::<Vec<_>>().join(" ")`), apply `crate::assessment::trim_middle(&collapsed)` so a long rationale cannot evict older `TrustedUserInput` entries under `retained_count`, and fall back to `"auto-review did not approve this action"` if empty.
  - Format as `format!("Mjolnir guardian flagged this as {} risk: {rationale}\n", risk.as_str())` when `risk_level` is `Some(risk)`, or `format!("Mjolnir guardian flagged this request: {rationale}\n")` when `None`.
  - Add a doc comment explicitly noting that `wolf_action_text` only strips form headers whose first line ends with `" requests permission:"`, so corpus builders consuming stored form messages must strip this preface to avoid label leakage.
- Remove `denial_notice` once no crate imports it.

Update `mj-worker/src/acp/guardian.rs`:
- Re-export `denied_escalation_preface` instead of `denial_notice`.
- Update the doc comment on `rejection_response` so it accurately describes its sole remaining use (refusing permission requests raised by the read-only ACP fallback reviewer itself).

Update `mj-worker/src/acp/guardian/reviewer.rs`:
- Update the tripped-breaker reason at line 120 from `"it denied several actions in this turn"` to `"several flagged actions were declined in this turn"`.
- Keep `record_settled_review` for settled `Allow` (records `PriorGuardianReview`, sends `RuntimeEvent::GuardianReviewed`, and calls `record_guardian_review(false)`).
- Add `record_escalated_review(&self, review: PriorGuardianReview, journal: mpsc::Permit<'_, RuntimeEvent>)` which records `PriorGuardianReview` in `self.spec.turn_context` and sends `RuntimeEvent::GuardianReviewed { review }` on `journal`, without touching the breaker.
- Add `record_breaker_outcome(&self, denied: bool) -> bool` which delegates to `self.spec.turn_context.record_guardian_review(denied)`.

Update `mj-worker/src/acp/permissions.rs`:
- Add a `guardian_flagged: bool` parameter (or small typed flag enum) to `permission_choices`: when `guardian_flagged` is true and the request offers at least one `PermissionOptionKind::AllowOnce` option, filter out `PermissionOptionKind::AllowAlways` options so a flagged request cannot create a standing harness allow rule that bypasses future guardian reviews. When no `AllowOnce` option is present, keep the existing options unchanged so the user can still approve.

Update `mj-worker/src/acp/drive.rs`:
- On `GuardianDecision::Allow { review }`: keep existing `allow_once_response` -> reserve journal -> `ticket.settle` -> `record_settled_review` -> return (or `ask_person_preface` fallthrough if no `AllowOnce`).
- On `GuardianDecision::AskPerson { reason }`: leave untouched (`guardian_preface = Some(ask_person_preface(&reason))`, fall through to `register`).
- On `GuardianDecision::Deny { rationale, review }`:
  - Reserve a `permission_events` journal permit (racing with `ticket.withdrawn()` and `cancellation.cancelled()`; if withdrawn/cancelled, immediately respond `Cancelled`).
  - Call the existing `ticket.settle(|withdrawn| ...)`: if `withdrawn || cancellation.is_cancelled()`, answer ACP `Cancelled` and record nothing; otherwise call `guardian.record_escalated_review(review, journal)` and proceed.
  - Set `guardian_preface = Some(guardian::denied_escalation_preface(risk_level, &rationale))` and record `escalated_denial = Some(EscalatedDenial { guardian: guardian.clone(), review_id, tool_call_id })`.
  - Remove the `RuntimeEvent::Notice` (`denial_notice`) emission and the tripped-breaker rationale suffix.
  - Fall through immediately (no `.await` in between on the non-plan path) to `permission_choices(..., escalated_denial.is_some())` and the existing `permission_elicitations.register(...)` at line 758.
- In the spawned elicitation responder task (`drive.rs:762-812`), after emitting `ElicitationResolved` and strictly before calling `responder.respond(answer)`:
  - If `let Some(escalated) = escalated_denial`:
    - Determine `outcome` label and breaker update:
      - `Some(ManualPermissionDecision::Allowed)` -> `outcome = "allowed"`, call `escalated.guardian.record_breaker_outcome(false)`.
      - `Some(ManualPermissionDecision::Rejected)` -> `outcome = "rejected"`, call `escalated.guardian.record_breaker_outcome(true)`.
      - `Some(PendingAnswer::OperatorAway)` -> `outcome = "away"`, call `escalated.guardian.record_breaker_outcome(true)`.
      - Otherwise (turn cancelled / channel closed) -> `outcome = "cancelled"`, leave breaker unchanged.
    - Emit `tracing::info!(target: "mj_guardian", review_id = %escalated.review_id, form_id = %id, tool_call_id = %escalated.tool_call_id, outcome, "guardian-flagged permission request resolved")`.

### Milestone 2: Worker Tests

Update and extend `mj-worker/src/acp/guardian_tests.rs`:
- Update `kimi_permission_requests_are_reviewed_by_the_mjolnir_guardian`:
  - Guardian `Allow` still auto-approves once without opening a form.
  - Guardian `Deny` (model deny, `critical` enforced deny, `high`+`low` enforced deny) opens a `tool_permission` `ElicitationRequest` with `Mjolnir guardian flagged this as <risk> risk: <rationale>` on line 1, omits `AllowAlways` when `AllowOnce` is present (and retains options when no `AllowOnce` is present), journals `GuardianReviewed { outcome: Deny }`, and emits no `Mjolnir guardian denied` `RuntimeEvent::Notice`.
  - Human `AllowOnce` delivers the allow option to ACP, records `ManualPermissionDecision::Allowed`, and resets consecutive breaker denials (3 consecutive human-approved escalations do NOT trip the breaker).
  - Human `Decline`/reject delivers `RejectOnce` to ACP, advances the breaker, and after 3 consecutive declines trips the breaker so the 4th request bypasses Guardian with the updated tripped-breaker preface.
  - ACP fallback reviewer's own tool permission requests remain refused via `rejection_response`.
- Update `guardian_alignment_sends_settled_denial_on_the_next_turn` to answer the escalated form and verify that `prior_reviews` carries the `Deny` alongside `trusted_user_inputs` on the next turn.
- Add a breaker-by-final-outcome test with `OperatorAway` enabled using **three distinct requests** (because `OperatorAway` skips each distinct request identity once), asserting:
  - Each of the 3 distinct flagged requests emits `ElicitationRequested` and `ElicitationResolved` (`away`), answers ACP `RejectOnce`, records no `ManualPermissionDecision`, and retains line 1 of the form message in the transcript projection.
  - The 3rd away-decline trips the breaker, so a 4th distinct request skips Guardian and opens the form directly with the tripped-breaker preface.
- Check existing withdrawal tests in `guardian_tests.rs` and cover/extend both withdrawal windows on `Deny`:
  - Withdrawn before `settle`: answers ACP `Cancelled`, emits neither `GuardianReviewed` nor `ElicitationRequested`.
  - Withdrawn after `settle` while the escalated form is open: answers ACP `Cancelled`.

### Milestone 3: Stage-2 Follow-Up Evaluation Check and Documentation

Before pushing, run ~8 constructed Stage-2 evaluation cases through the existing Stage-2 utility reviewer path (testing the sequence `prior Deny -> human decision: allowed/rejected -> next request`):
- Identical action repeated after human approval (expect `allow`).
- Broader or more destructive variant after a narrow human approval (expect `deny`).
- Identical action repeated after human rejection (expect `deny`).
Record the observed outcomes in `Surprises & Discoveries`. If Stage 2 over-generalizes one approval or keeps re-denying an approved action, report the cases back to Advisor (`91233b0efdd193066e7b4eeb46c8db10`) rather than patching worker code.

Update `docs/src/content/docs/security.md` ("The Mjolnir guardian" section):
- Document the `native (optional) -> Mjolnir guardian -> human` escalation model and state plainly that internal denials by native reviewers (Claude `auto`, Codex `agent`, Muse `auto_review`) never cross ACP and remain final.
- Explain that Mjolnir guardian denials open the standard permission approval form showing the guardian's risk level and rationale, surface through Mjolnir's existing notifications (TUI Notifications pane, bell/title, desktop notifications, Web Inbox, and Web Push), and omit standing "allow always" options when one-time approval is available so later calls stay reviewed.
- Explain what happens on Approve (runs once and records trusted human approval for later reviews), Reject/Skip (refuses once when `RejectOnce` is offered, otherwise cancels the request), and Operator Away (skips each distinct request once with `RejectOnce` / `Cancelled`, leaving an identical retry pending for the person).
- Document that the per-turn breaker (3 consecutive or 10 of 50) counts declined/away-skipped escalations and then sends remaining requests straight to the person.

### Milestone 4: Validation, Review, and Push

Run targeted tests and clippy on the modified crates (`mj-core` and `mj-worker`), verifying non-zero test counts per filter:

    cargo test -p mj-core guardian
    cargo test -p mj-worker guardian
    cargo test -p mj-worker permission_choices
    cargo clippy -p mj-core -p mj-worker --all-targets -- -D warnings

Commit on `master`, request before-push review via `mcp_mj-review_request_review`, address or override findings as appropriate, and push to `origin/master`.

## Validation and Acceptance

Acceptance requires all of the following observable behaviors:

1. When Mjolnir Guardian evaluates an ACP tool permission request and returns `Deny` (from either Utility LLM or ACP fallback, including `enforced_outcome` overrides), the worker does NOT reply `RejectOnce` immediately and does NOT emit `Mjolnir guardian denied ...` as a `RuntimeEvent::Notice`; instead it records `GuardianReviewed` via `ticket.settle` and opens a standard `tool_permission` `ElicitationRequest` whose first line is `Mjolnir guardian flagged this as <risk> risk: <trimmed single-line rationale>`.
2. On a guardian-flagged form, `AllowAlways` options are omitted whenever `AllowOnce` is available; when no `AllowOnce` is offered, all existing options remain selectable.
3. Answering the escalated form with Allow executes the tool call, records `ManualPermissionDecision::Allowed`, resets consecutive breaker denials, and logs a `mj_guardian` info event linking `review_id`, `form_id`, `tool_call_id`, and `outcome = "allowed"`.
4. Answering with Reject/Skip or Operator Away sends `RejectOnce` (or `Cancelled` if no `RejectOnce`), advances the per-turn breaker, and after 3 consecutive declined/away-skipped requests trips the breaker so the 4th request bypasses Guardian with the updated tripped-breaker preface. Under Operator Away, each distinct request is skipped once and the transcript retains the form's first line.
5. Turn withdrawal before `settle` answers ACP `Cancelled` with no `GuardianReviewed` and no `ElicitationRequested`; turn withdrawal after `settle` while the form is open answers ACP `Cancelled`.
6. The ~8 constructed Stage-2 follow-up evaluation cases behave sensibly without over-generalizing approval or re-denying approved actions.
7. Targeted tests and `cargo clippy -p mj-core -p mj-worker --all-targets -- -D warnings` pass with zero warnings.
