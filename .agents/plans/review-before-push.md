# Review before push: child-session reviewers, a request_review tool and a pre-push gate

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Today Mjolnir can review an agent's work only at the end of each turn. A user who wants review before code leaves the machine has no setting for it. After this change the user picks one of three review triggers in `config.toml`: `off`, `turn` (today's end-of-turn review) or `push`. In `push` mode a Claude Code or Codex session is told, through the `mj-agents` MCP server it already has, to commit its work and call a new `request_review` tool before any `git push` or `gh pr create`. The tool answers within seconds that a review has started (or why it cannot), the agent goes on or ends its turn, and the verdict arrives minutes later as a Mjolnir mailbox message, the same way a child's report does. A clean verdict records HEAD's tree as reviewed inside the repository's own git directory. A pre-push hook that Mjolnir installs in the session checkout refuses any push whose commit tree has no such record, and tells the agent to call `request_review`.

The reviewer itself changes for both modes. Today the reviewer is a "sidecar": a second harness conversation that runs inside the primary session's worker process under a staged profile, with its own relay journal. After this change the reviewer is an ordinary Mjolnir child session, created through the same code that `spawn` uses, and it reports its verdict through the same `handback` tool a child uses. One mechanism runs reviewers; the review host keeps only the state machine around them. The sidecar is deleted, and with it the plan-approval "second opinion" feature, which was the sidecar's only other user.

To see it working: set `[review] trigger = "push"`, start a Claude or Codex session in a repository with a remote, ask the agent to make a change and push it. The agent commits, calls `request_review`, and either waits or ends its turn. The review runs as a child session visible in the dashboard. A message arrives in the agent's conversation: either "Review clean: tree abc123 recorded; you may push" or the findings with an instruction to fix, commit and request review again. If the agent tries `git push` before a clean review, git prints `mj: refs/heads/feature (tree abc123) has not been reviewed` and the push fails.

## Progress

- [x] (2026-10-23 20:30Z) Research complete: turn-review map at `.mj/agents/0580117db883375505d8b39713f67198/turn-review-map.md` and subagent/MCP/hook map at `.mj/agents/8b1fec655c687886bc499af6ac3a428f/review-before-push-map.md`. Decisive facts are repeated in Context and Orientation below so this plan stands alone.
- [x] (2026-10-23 21:00Z) ExecPlan authored.
- [ ] Milestone 1: three-state review trigger in configuration, per-session override, CLI, API, TUI settings and web viewer. `enabled = true` still loads as `turn`.
- [ ] Milestone 2: turn review runs its reviewer as a child session. The sidecar is no longer used for turn review.
- [ ] Milestone 3: remove the plan second opinion and the reviewer sidecar. Settings model/effort discovery moves to the profile capability catalog.
- [ ] Milestone 4: push review. `request_review` tool in `mj-agents`, review host entry that runs during a turn, "pushable commits" capture scope with the base rule from the Decision Log, verdict by mailbox message, reviewed-tree record on a clean verdict.
- [ ] Milestone 5: pre-push hook installed by the worker in push mode, gated by an environment variable, chained with any existing hook. A transcript notice for harnesses without `mj-agents`.
- [ ] Milestone 6: documentation in `docs/src/content/docs/` (turn-review, configuration, cli-reference, api-reference).

## Surprises & Discoveries

- Observation: the repository has no "review since merge base with upstream" logic. Turn review's baseline is HEAD plus dirty tracked files at session start (`initialize_review_baselines`, `mj-worker/src/review/capture.rs:276-338`), and attribution uses the reflog, not a remote branch (`mj-worker/src/review/attribution.rs:60-115`). The push-mode base selection in Milestone 3 is new code.
  Evidence: `grep -rn "merge-base" mj-worker/src/review/` finds no use outside tests of unrelated behavior.
- Observation: the plan-approval "second opinion" reviewer and turn review share the sidecar (`reviewer_as` at `mj-controller/src/review_host/reviewer.rs:35`, `mj-controller/src/daemon/actions.rs:558`, `mj-controller/src/daemon/serve.rs:176`; `mj-client/src/review.rs:116-120` derives the reviewer relay id as `<primary>-reviewer` for both). The user chose to remove the second opinion rather than keep the sidecar for it.
- Observation: the Settings screen's review model/effort discovery (`discover_review_settings`, `mj-controller/src/review_settings.rs:54`, calling `ReviewerAction::Start` at `:290-307`) also starts the sidecar. A per-profile capability catalog with models and efforts already exists (`mj-controller/src/server_runtime/profile_catalog.rs`, `ProfileCapabilities { choices, efforts }`), so discovery has a replacement. Whether the catalog covers every harness the review settings screen supports is not yet confirmed; check before deleting `review_settings.rs`.

## Decision Log

- Decision: the review trigger is a three-value setting `trigger = "off" | "turn" | "push"` that replaces the boolean `enabled`. A configuration that still says `enabled = true` loads as `turn`, `enabled = false` or absent loads as `off`, and saving writes only `trigger`. CLI and API take the three words directly and drop `on`/`off` and `--no-review`.
  Rationale: the user asked for off / end of turn / before push as one setting. The project convention is that configuration accepts old keys silently, renames drop CLI and API aliases, and database names stay as they are.
  Date/Author: 2026-10-23, Jonathan Ellis with Claude.
- Decision: both turn review and push review run their reviewer as a Mjolnir child session, registered through `Controller::register_subagent` with a new kind `Reviewer`. The verdict is the child's `handback` text, classified by the existing `mj_review::verdict::review_output_verdict`.
  Rationale: the user chose one reviewer mechanism over keeping the sidecar for turn review and adding a second path for push review. Child sessions already have a worker, a transcript, a mailbox report path and a borrowed parent target; the sidecar duplicates all of that.
  Date/Author: 2026-10-23, Jonathan Ellis (chose "child session reviewer for both modes").
- Decision: the plan-approval second opinion is removed, and the reviewer sidecar is deleted with it: `ReviewerAction::Start/Attach/Submit/Status/Pause/RespondElicitation`, the worker's sidecar conversation, `mj-controller/src/controller/reviewer.rs` profile staging, `mj-core/src/second_opinion.rs`, `mj-chat/src/chat/second_opinion.rs`, the `second_opinion_reviews` table and its `database/reviews.rs`, and the plan dialog's Second opinion action. `CaptureDelta`, `AdvanceBaseline` and the new `RecordReviewed` stay as worker commands. Settings discovery of a review profile's models and efforts reads the profile capability catalog instead of starting a sidecar.
  Rationale: the user chose one reviewer mechanism. Keeping the sidecar for one feature would leave two mechanisms, which this project avoids. The second opinion is the only remaining sidecar user.
  Date/Author: 2026-10-23, Jonathan Ellis ("remove 2o instead").
- Decision: reviewed state is recorded as a git ref `refs/mj/reviewed/<tree-id>` pointing at the commit whose tree it is. The hook checks the pushed commit's tree, not its id.
  Rationale: the user chose tree matching. Rewording or re-signing a commit keeps its tree and does not force a new review; any content change does.
  Date/Author: 2026-10-23, Jonathan Ellis.
- Decision: push review refuses when there are uncommitted tracked changes and ignores untracked files. The base is chosen in two steps. First compute `mb`: the merge base of HEAD and the default branch; when HEAD is on the default branch (the merge base is HEAD itself) or there is no default branch, `mb` is the commit HEAD named at session start. Then the base is the newest `refs/mj/reviewed/*` commit that is both an ancestor of HEAD and a descendant of `mb`; if there is none, the base is `mb`. The default branch is the branch `refs/remotes/origin/HEAD` points to; if the remote has none, a local `main`, then `master`.
  Rationale: the user specified this rule. A review point older than `mb` belongs to history before a rebase or merge and must not shorten the review. The merge base with the default branch is fixed until the branch is rebased or merged, so the first push review in a session covers every commit the branch adds, including commits from earlier sessions. `@{upstream}` and `origin/HEAD` were rejected as bases: the upstream moves with every push of the branch, and `origin/HEAD` as a plain ref moves with the default branch.
  Date/Author: 2026-10-23, Jonathan Ellis.
- Decision: in push mode findings are not forwarded as a corrective prompt and no prompt hold is taken. The verdict is delivered as a mailbox event with `wake: true`. Clean advances the baseline and records the reviewed tree; findings leave both unchanged. The review view keeps Cancel; Forward and Dismiss do not apply.
  Rationale: the agent asked for the review during its own turn and is waiting for the answer. A hold exists to keep edits from racing a between-turns review; here there is nothing to hold against.
  Date/Author: 2026-10-23, Jonathan Ellis ("async w/ mbox is correct").
- Decision: the pre-push hook enforces only when `MJ_REVIEW_GATE=1` is in the environment of the `git push` process. The worker sets that variable in the harness environment when the session's trigger is `push`.
  Rationale: a raw session can share the user's own checkout. The user's pushes from a terminal must not be refused, and a session in `turn` or `off` mode must not be refused by a hook left behind by an earlier `push` session.
  Date/Author: 2026-10-23, Claude.
- Decision: `trigger = "push"` for a primary session whose harness does not receive `mj-agents` (anything other than Claude Code or Codex) runs without the hook, the gate variable or the tool, and the worker writes one notice into the session transcript at start: "Before-push review is not available for <harness>; this session runs without review."
  Rationale: the user said push review is not supported on other harnesses and asked for nothing more invasive than a transcript notice.
  Date/Author: 2026-10-23, Jonathan Ellis.
- Decision: the reviewer child is closed, not parked, once its verdict is recorded. Its transcript stays in the database.
  Rationale: a corrective pass starts a fresh reviewer with the prior findings in its prompt (`PriorReviewContext`), so nothing in the old child is reused. Closing releases its processes and its report directory.
  Date/Author: 2026-10-23, Claude.

## Outcomes & Retrospective

Not started.

## Context and Orientation

Mjolnir (`mj`) runs coding agents ("harnesses" such as Claude Code and Codex) in sessions. A daemon (the long-running `mj` process, code in `mj-controller/`) owns sessions; each session has a worker process (`mj-worker/`) that launches the harness and talks to the daemon over a "relay" (a journaled command protocol, `mj-core/src/relay/`). The shared types live in `mj-core/`. The review-specific logic that is independent of the daemon lives in `mj-review/` (prompt, verdict classification, driver state machine).

A turn is one prompt answered by the harness. Turn review, documented in `docs/src/content/docs/turn-review.md`, starts one reviewer after a prompt-driven turn goes idle, gives it the user's messages and two git tree ids per repository, and classifies its final reply. The parts:

The review host, `mj-controller/src/review_host/`, is a task in the daemon. `begin.rs` `observe` watches each session's execution state and arms a review on the running-to-idle edge of a prompt-driven turn when `(self.config)(&session_id).enabled` is true (`begin.rs:30-40`). `begin` holds new prompts for the session (`hold_prompts`, `prompts.rs:64-75`) and spawns `prepare` (`reviewer.rs:101-257`), which captures the repository delta through the worker, resolves the reviewer profile, and creates the driver. `refuse_start` (`begin.rs:162-199`) refuses a review while a turn is running with "a review runs between turns; this one is still working". `roles.rs` executes the driver's requests: `CaptureDelta`, `StartRole`, `PromptRole`, `PauseRole`, `AdvanceBaseline`, `PromptPrimary`. `resolve.rs` forwards findings as a corrective prompt to the primary (`forward_fresh_findings`, `resolve.rs:106-120`) and releases the hold. `persist.rs`, `notices.rs`, `slots.rs` (the `VerdictView` projection), `events.rs` (`ReviewSlot`, `HostEvent`) and `environment.rs` (profile checks, `stage_reviewer_profile`) complete the host. `host.rs` wires the loop. Tests are in `review_host/tests.rs` (2,650 lines; read them before changing the host).

The config closure `(self.config)` is `ReviewConfigSource`, built in `mj-controller/src/daemon/state.rs:47-74`; it applies `ReviewConfig::for_session` (`mj-core/src/config.rs:187-203`) to the global `[review]` section with the session's stored choice.

The driver, `mj_review::driver::TurnReviewDriver` (`mj-review/src/driver.rs`), is a pure state machine. `start(seed)` returns requests; `delta_captured`, `role_started`, `role_turn_completed(command_id, answer)`, `request_failed`, `forward`, `dismiss`, `cancel` advance it. `role_turn_completed` is where the reviewer's reply text becomes a verdict through `mj_review::verdict::review_output_verdict` (`mj-review/src/verdict.rs:87-104`): structured JSON with an empty `findings` array is `Clean`; anything else is `Findings { synthesis }`; empty text is `Failed`. The reviewer prompt is `mj_review::lanes::review_prompt(&ReviewJob)` (`mj-review/src/lanes.rs:87-139`); it embeds Codex's review rubric, one `git -C <root> diff --no-ext-diff <baseline_tree> <capture_tree>` command per repository, a changed-files table, the user's messages, and optional prior findings. `TurnReviewSeed` (`mj-core/src/review/driver.rs:132-146`) carries task, user messages, baselines, ordinal and prior review.

The reviewer today is a sidecar. The daemon calls `handle.reviewer_as(role, action)` (`review_host/reviewer.rs:21-62`); the worker runs a second ACP conversation in the same process, journaled separately (`mj-core/src/relay/protocol.rs:183-210`, command names `reviewer_start`, `reviewer_attach`, `reviewer_submit`, `reviewer_status`, `reviewer_pause`, `reviewer_capture_delta`, `reviewer_advance_baseline` at `:320-328`). The worker-side handlers are in `mj-worker/src/review.rs` with capture in `mj-worker/src/review/capture.rs`. The staged reviewer profile comes from `mj-controller/src/controller/reviewer.rs`. The TUI and web viewer attach to the reviewer's journal through the relay id `<primary>-reviewer` (`mj-client/src/review.rs:116-120`); the chat UI's review pane is `mj-chat/src/chat/turn_review.rs`.

Capture: `capture_repository_deltas` (`capture.rs:27-161`) diffs a baseline tree against a synthetic tree of the current working state (HEAD plus dirty and untracked paths). A clean verdict advances the baseline with `pin_review_tree` (`mj-checkpoint/src/archive/git.rs:1308`, writes a ref with `git update-ref`); captures also record a tag through `record_review_point` (`git.rs:688`). `read_workspace_state` (`capture.rs:1673+`) lists dirty tracked and untracked entries. The result type is `RepoDelta` (`mj-core/src/relay/protocol.rs:264-300`).

Sub-agents: a parent session's harness gets the `mj-agents` MCP server with tools `spawn`, `list_agents`, `send_message`, `close` (Parent role) or `handback` (Child role); the allowlist is `SubagentMcpRole::tool_names` (`mj-core/src/subagent.rs:714-742`). The server process is `hel worker subagent-mcp` (`mj-worker/src/subagent_mcp.rs`); its instructions text is `SERVER_INSTRUCTIONS` (`:21-26`). It turns a tool call into a durable `SubagentToolRequest` (`mj-core/src/subagent.rs:328-397`) on the worker's queue (`mj-worker/src/worker_runtime/subagents.rs:680-711`), waits up to 120 s for the daemon's answer (`subagent_mcp.rs:160-185`), and the daemon observes the queue in `mj-controller/src/session_manager/subagent_queue.rs:24-53`. `spawn` ends in `Controller::register_subagent(RegisterSubagentRequest)` (`mj-controller/src/controller/subagents.rs:13-30, 368+`), which takes `parent_session_id`, `task_name`, `profile_id`, `model`, `effort`, `working_directory`, `initial_prompt`, `request_key`, `report_root`; the child borrows the parent's target and may not spawn grandchildren (`subagents.rs:446-489`). A finished child's `handback` is recorded by `mj-controller/src/server_runtime/api.rs:1025-1099`; `mj-controller/src/daemon/delegation.rs:239-286` reconciles the finish, produces a `SubagentFinished` mailbox event for the parent through `mj-controller/src/database/delegation.rs:171-235`, and parks the child. `SubagentRecord` (`mj-core/src/subagent.rs:647+`) stores extra fields in `record_json` without schema changes.

The mailbox: durable events for a session (`mj-core/src/mailbox.rs`), delivered by `mj-controller/src/mailbox_outbox.rs` (`deliver_mailbox_event`, `:372-440`). A busy parent receives them at a tool boundary (Claude: a `PostToolBatch` hook, `mj-worker/src/mailbox_hook.rs`; Codex: its own hook, `worker_runtime/subagents.rs:333-450`); an idle parent gets a `MailboxWake` turn (`mj-worker/src/relay/commands.rs:1986-2035`). Structured bodies need relay protocol 34; `SubagentFinished` needs 41 and is rendered as plain text for older workers (`mailbox_outbox.rs:428-436`). Current protocol is 42 (`mj-core/src/relay.rs:81-84`).

Registration of `mj-agents`: for Claude the worker writes a private `.claude.json` entry and allow rules (`configure_claude_subagent_mcp`, `worker_runtime/subagents.rs:48-164`); for Codex a private `config.toml` entry (`:320-439`); both are called from worker startup (`worker_runtime/unix.rs:1531-1610`). Harness processes are launched with a built environment in `mj-worker/src/worker_runtime/harness_launch.rs` (around line 86, `env_clear` then explicit variables).

The checkout: a managed session clones into `<repository>/.mj/clones/<session-id>` (`mj-controller/src/controller/worktree.rs:345-408`); a raw session uses the directory as is. The worker and harness run on the same filesystem as the checkout (local, SSH or container). Clone creation copies the user's `core.hooksPath` preference (`worktree.rs:2270-2280`). Nothing installs hooks today.

## Plan of Work

### Milestone 1: three-state trigger

Scope: replace the boolean with `ReviewTrigger` everywhere a review decision is read, without changing behavior for `turn`. At the end, `mj config` shows `trigger`, the TUI Settings screen offers Off / End of turn / Before push, `mj new --review push` is accepted and stored, and the daemon arms turn review only for `turn`.

In `mj-core/src/config.rs`, define `pub enum ReviewTrigger { Off, Turn, Push }` with serde `rename_all = "snake_case"` and `Default = Off`. Replace `ReviewConfig::enabled: bool` with `trigger: ReviewTrigger` (serde default, skip when `Off`). Keep a private deserialize-only `enabled: Option<bool>` and, in the loading step that already normalizes `tier` (`mj-core/src/config/loading.rs`), map `enabled = Some(true)` to `Turn` when `trigger` is absent; never write `enabled`. Replace `SessionReview::Off | On { model, effort, .. }` with a shape that stores the trigger; keep decoding the stored tags (`on` reads as `turn`), because `sessions.review_json` rows already exist. The test `a_session_review_choice_is_stored_in_a_stable_shape` in `mj-core/src/config/tests.rs` (near line 1952) defines the stored shape; extend it rather than replace it. Update `ReviewConfig::for_session` to copy the trigger. Add `ReviewConfig::arms_turn_review(&self) -> bool` (`trigger == Turn`) and `is_push(&self) -> bool`, and replace every `.enabled` read, starting with `begin.rs:37` and `mj-core/src/review/settings.rs`.

CLI (`mj-cli/src/new.rs`, `mj-cli/src/import.rs`): replace `--no-review` with `--review <off|turn|push>`. `--review-model` and `--review-effort` without `--review` imply `turn`, as they imply enablement today. `--review-tier` stays accepted and ignored. API (`mj-controller/src/server/api/`, documented in `docs/src/content/docs/api-reference.md:438-450`): `review.mode` accepts `off`, `turn`, `push`; `on` is rejected with a message naming the three values. TUI (`mj-tui/src/review_settings.rs` toggle near lines 294, 580, 872-896; golden `mj-tui/tests/golden/review-settings.txt`): a three-way selector. Web viewer: the `review_config` projection in `mj-controller/src/server_runtime/snapshot.rs` carries `trigger`; update the browser view that shows it.

Acceptance: `cargo test -p mj-core -p mj-controller -p mj-cli -p mj-tui` passes; a config with `enabled = true` and no `trigger` loads as `Turn` (new test); `mj new --review push` stores the choice and `mj review status` reports it.

### Milestone 2: turn review runs a child-session reviewer

Scope: the review host stops starting a sidecar role and instead registers a child session whose first prompt is the review prompt plus a handback instruction. The verdict path, baseline advance, forward/dismiss/cancel, notices and persistence stay as they are.

Add `pub enum SubagentKind { Worker, Reviewer }` (default `Worker`, serialized into `record_json`) to `SubagentRecord` and `RegisterSubagentRequest`. In `register_subagent`, a `Reviewer` child skips the live-children capacity check and is excluded from `list_subagents` (`mj-controller/src/server/api/subagents.rs:612+`) and from `list_agents` output. Add `SubagentKind` to the request key so a retried registration finds its record.

In `mj-controller/src/review_host/`, add `reviewer_child.rs`. When the driver issues `StartRole { role, fresh }` for the turn review's single role, the host instead calls a new `start_reviewer_child(session_id, job)` that resolves profile, model and effort through `review_selection::resolve` (unchanged Auto policy), builds `initial_prompt = review_prompt(&job) + REVIEWER_HANDBACK_INSTRUCTION`, and calls `register_subagent` with `kind: Reviewer`, `task_name: "review"`, `working_directory` empty (the parent's checkout), `report_root: None`. Store `child_session_id` in `ReviewSlot` and persist it in `TurnReviewState` so a restart can find or cancel the child. `REVIEWER_HANDBACK_INSTRUCTION` says, in plain words: send the JSON result described by the rubric as the complete text of your single `handback` call; do not edit files; do not push.

Route the child's finish to the host. In `mj-controller/src/daemon/delegation.rs` where a finished child is reconciled, a `Reviewer` child does not produce a `SubagentFinished` event; it sends `HostEvent::ReviewerFinished { session_id: parent, child_session_id, answer: handback_text_or_empty }` to the review host and is closed after the host acknowledges. The host feeds the answer to `driver.role_turn_completed(command_id, &answer)`, which already yields the verdict and the next requests. A child that fails to start, or finishes without a handback, becomes `driver.request_failed(reason)`, which already produces a `Failed` verdict that never advances the baseline. `PauseRole` (cancel) becomes `close` of the child. The `fresh` flag and the generation nonce are no longer needed for turn review.

Reviewer elicitations (a reviewer harness asking permission) arrive as the child session's own elicitations. The review view in the TUI (`mj-chat/src/chat/turn_review.rs`) and the web viewer must attach to `child_session_id` for the transcript tab and answer its elicitations there, instead of attaching to `<primary>-reviewer`. The `RoleTranscript` projection in `events.rs` and the role polling in `roles.rs:250+` are removed for turn review. `refuse_start`'s "waiting for your answer to a question" case reads the child's pending elicitation.

Leave the sidecar code in place during this milestone so the second opinion keeps compiling; Milestone 3 deletes it. `CaptureDelta` and `AdvanceBaseline` remain worker commands used by the host.

Acceptance: `cargo test -p mj-controller` passes with `review_host/tests.rs` adapted (the tests that start roles become tests that register a reviewer child and complete it with a handback). A live check: `[review] trigger = "turn"`, a Claude session edits a file, the dashboard shows a child session named "review" under it while the review runs, the verdict appears in the review view, and `list_agents` from the parent does not list it.

### Milestone 3: remove the second opinion and the sidecar

Scope: delete the plan-approval second opinion and everything that exists only to run a sidecar reviewer. At the end, the plan approval dialog offers no "Second opinion" action, the relay has no `reviewer_start`, `reviewer_attach`, `reviewer_submit`, `reviewer_status`, `reviewer_pause` or `reviewer_respond_elicitation` commands, and `review_settings` discovery reads the profile catalog.

The second opinion is a feature of the plan approval dialog: when a harness proposes a plan, the user can ask a reviewer to read it before answering. Its code: `mj-core/src/second_opinion.rs` (workflow types), `mj-core/src/acp.rs:28, 76-110` (`PLAN_REVIEW_SECOND_OPINION`, `plan_review_second_opinion`, the answer given back to the harness), `mj-core/src/storage.rs:202-206` (an open second opinion saved across UI restarts), `mj-core/src/continuation.rs:228` (`is_control_origin_prompt`), `mj-chat/src/chat/second_opinion.rs` with its tests and the dispatch, keys, pointer, events and transcript code that reference it, `mj-controller/src/database/reviews.rs` and the `second_opinion_reviews` table (`legacy_schema.rs:263-296`), `mj-controller/src/controller/lifecycle.rs:962`, and `mj-worker/src/acp/permissions.rs:287-360` (the plan-review answer path says a second opinion never reaches the worker; the branch for it goes). Remove each and the tests with them. Drop the `second_opinion_reviews` and `second_opinion_defaults` tables in a schema migration; the project keeps database names on renames, but this is a removal.

The sidecar: delete `ReviewerAction::Start/Attach/Submit/Status/Pause/RespondElicitation` and their outcomes in `mj-core/src/relay/protocol.rs:183-258, 320-328, 630+`, the worker handlers in `mj-worker/src/review.rs` and the ACP sidecar launch in `mj-worker/src/acp/`, `DaemonAction::ReviewerAction` (`mj-controller/src/daemon/actions.rs:551-559`, `serve.rs:168-182`), `ManagedSessionHandle::reviewer_as` and `client_backend.rs:151`, `mj-controller/src/controller/reviewer.rs` (profile staging) and `review_host/environment.rs` `stage_reviewer_profile`, `mj-client/src/review.rs` (`role_session_id`, reviewer question ids), and the `ReviewerLaunchConfig` in `mj-core/src/worker_launch.rs:359-399` except what `CaptureDelta` still needs. Keep `CaptureDelta`, `AdvanceBaseline` and (from Milestone 4) `RecordReviewed`; rename the remaining enum to `ReviewCaptureAction` if that reads better, with the relay command names unchanged.

Discovery: rewrite `mj-controller/src/review_settings.rs` so `discover_review_settings` reads the selected profile's `ProfileCapabilities` from the catalog (`server_runtime/profile_catalog.rs`) and returns the same model and effort choices the TUI editor consumes (`mj-tui/src/review_settings.rs`). If the catalog lacks a harness the review settings screen supported, record that in Surprises & Discoveries and either add the harness to the catalog or show "choices unavailable" for it; do not keep a sidecar for discovery.

A new daemon never sends the deleted commands. A new worker that receives one from an old daemon cannot happen: the daemon is replaced first and then upgrades workers. Bump the relay protocol in Milestone 4 (43) covers the capture change; no separate bump is needed for deletions.

Acceptance: `cargo test --workspace` passes; `grep -rn "second_opinion\|SecondOpinion\|reviewer_as\|reviewer_start" --include=*.rs mj-*/src` finds nothing outside legacy-schema history; the plan approval dialog in a live Claude session shows only the harness's own choices; Settings → Code Review still lists the models and efforts for a named Codex and Claude profile.

### Milestone 4: push review

Scope: an agent in push mode can request a review of its commits during its turn and receives the verdict by mailbox.

Tool. In `mj-core/src/subagent.rs` add `SubagentToolAction::RequestReview {}` and the tool name `request_review` to `SubagentMcpRole::Parent`'s list, but only when the server runs with a `--review-gate` flag. In `mj-worker/src/subagent_mcp.rs` add the tool schema (no required arguments) and its mapping to the action; add a sentence to the Parent instructions when the flag is set: "Before `git push` or `gh pr create`, commit your work and call `request_review`. It answers at once whether a review started; the verdict arrives later as a Mjolnir message. The repository's pre-push hook refuses a push of commits that have not been reviewed clean." The worker passes `--review-gate` in the registered server command when its launch config's trigger is `push` (`configure_claude_subagent_mcp`, the Codex equivalent, and the ACP fallback in `mj-worker/src/acp/launch.rs:252-281`). Add `review_trigger: ReviewTrigger` to the worker launch config in `mj-core/src/worker_launch.rs`.

Daemon entry. The subagent queue observer handles `RequestReview` by sending `HostCommand::StartPushReview { session_id, request_id, reply }` to the review host. New `begin_push` in `begin.rs`: refuse if the trigger is not `push`, if the session is moving, or if a review is already open; do not require idle and do not call `hold_prompts`. It runs `prepare` with `scope: CaptureScope::PushableCommits`. The reply to the tool request is sent when preparation finishes: accepted with base commit, HEAD and changed-file count, or refused with the reason. Preparation is a few git commands, well within the tool's 120 s budget.

Capture scope. Add `CaptureScope { TurnDelta, PushableCommits }` to `CaptureDelta` (relay protocol 43; a controller never sends `PushableCommits` to a protocol-42 worker and instead refuses the request with "the session's worker predates push review; restart the session"). In `mj-worker/src/review/capture.rs`, `capture_pushable_commits(root, session_start_head)`: run `git status --porcelain --untracked-files=no`; if any line, refuse with the file list ("commit or stash these tracked changes first"). Resolve HEAD.

Find `mb`. The default branch is the target of `git symbolic-ref -q refs/remotes/origin/HEAD`; if that fails, `refs/heads/main` if it exists, then `refs/heads/master`. With a default branch, `mb = git merge-base HEAD <default>`. If `mb` equals HEAD (HEAD is on the default branch) or there is no default branch, `mb` is the commit HEAD named when the session started. The worker must know that commit: the session-start review point tag already records a `head <sha>` line (`record_review_point`, `mj-checkpoint/src/archive/git.rs:688-712`); read it from there, or store the commit next to the start baseline if that line is not reliable on every path. Record which case applied; it is reported back to the agent.

Find the base. List `git for-each-ref --format='%(objectname) %(committerdate:unix)' refs/mj/reviewed/`; keep each commit for which both `git merge-base --is-ancestor <commit> HEAD` and `git merge-base --is-ancestor <mb> <commit>` succeed; take the newest by committer date. If none qualifies, the base is `mb`. Build the `RepoDelta` with `baseline_tree = <base>^{tree}` and `current_tree = HEAD^{tree}` (no synthetic tree, no untracked paths), the patch, diffstat, numstat and `files` as today, and a new field `base_commit: Option<String>` for the reply and the message. Reuse `review_prompt` unchanged; the "Run this exact command" line already takes two tree-ishes.

Verdict delivery. Add `MailboxBody::ReviewVerdict { review_id, repository, base_commit, head_commit, verdict: ReviewVerdict }` to `mj-core/src/mailbox.rs`, gated on protocol 43 with a plain-text rendering for older workers, following `SubagentFinished`. In the host's resolution for a push-scope slot: on any verdict, enqueue the mailbox event for the primary with `wake: true` through the durable producer used by `mj-controller/src/server/api/mailbox.rs:6-52`; then, for `Clean`, send `AdvanceBaseline` and the new `ReviewerAction::RecordReviewed { tree, commit }` to the worker, and close the slot; for `Findings`, close the slot without advancing; for `Failed`, close and release as today. The rendered text for `Clean`: "Review clean for commits <base7>..<head7> in <root>. Tree <tree7> is recorded as reviewed; you may push." For `Findings`: "Review of commits <base7>..<head7> in <root> found:\n<synthesis>\nFix these, commit, and call request_review again before pushing." Forward and Dismiss are disabled in the review view for a push-scope slot; Cancel closes the child and the slot.

Recording. `mj-checkpoint/src/archive/git.rs`: `record_reviewed_tree(runner, repository, tree, commit)` runs `git update-ref refs/mj/reviewed/<tree> <commit>`. The worker handles `RecordReviewed` by calling it for each repository in the capture.

Acceptance: `cargo test -p mj-worker -p mj-controller -p mj-core` passes with new tests: a dirty tracked file refuses with its name; a feature branch with no reviewed ref uses the merge base with `origin/HEAD`'s branch; a reviewed ref that descends from `mb` wins over `mb`; a reviewed ref older than `mb` (recorded before a rebase) is ignored and `mb` is used; HEAD on the default branch uses the session-start commit; a repository with no remote and no `main`/`master` uses the session-start commit; `review_output_verdict` of a clean handback produces a `ReviewVerdict` mailbox event and a `refs/mj/reviewed/<tree>` ref pointing at HEAD; findings produce the message and no ref. Live: a Claude session in push mode calls `request_review`, the tool returns `accepted` within seconds, and the verdict message arrives in the conversation.

### Milestone 5: pre-push hook and harness notice

Scope: the worker installs the hook, sets the gate variable, and refuses push mode on unsupported harnesses.

Hook file. In `mj-worker/src/review/hook.rs`, `install_pre_push_hook(repository_root)`: `hooks=$(git rev-parse --git-path hooks)` (correct for clones and worktrees); if `core.hooksPath` is set, use that directory instead, because git runs hooks only from there. If `<hooks>/pre-push` exists and does not contain the marker line `# mj-review-gate`, rename it to `<hooks>/pre-push.before-mj` (only if that name is free; otherwise refuse to install and surface a notice). Write the script below with mode 0755 using the atomic write helper (`mj_core::config::atomic_write`). Installation is idempotent: a file with the marker is rewritten in place.

    #!/bin/sh
    # mj-review-gate
    # Installed by Mjolnir. Refuses a push of commits whose tree has not been
    # reviewed clean when MJ_REVIEW_GATE=1 is set; otherwise does nothing.
    # Chains to pre-push.before-mj if Mjolnir found an existing hook.
    hooks_dir=$(dirname "$0")
    if [ "${MJ_REVIEW_GATE:-}" = "1" ]; then
      zero=0000000000000000000000000000000000000000
      while read -r local_ref local_sha remote_ref remote_sha; do
        [ "$local_sha" = "$zero" ] && continue
        tree=$(git rev-parse "$local_sha^{tree}") || exit 1
        if ! git rev-parse -q --verify "refs/mj/reviewed/$tree" >/dev/null; then
          echo "mj: $local_ref (tree ${tree%${tree#???????}}) has not been reviewed." >&2
          echo "mj: call the mj-agents request_review tool, wait for the verdict message, then push again." >&2
          exit 1
        fi
      done
    fi
    if [ -x "$hooks_dir/pre-push.before-mj" ]; then
      exec "$hooks_dir/pre-push.before-mj" "$@"
    fi
    exit 0

The hook reads the ref list on stdin; the chained hook needs the same stdin, so when a chained hook exists the script must buffer stdin to a temporary file and feed it to the chained hook (replace the `while read` with a loop over the saved file and `exec ... < "$tmp"`). Write that version; the sketch above shows the logic.

Installation point. In the worker's preparation before the harness launch (`mj-worker/src/worker_runtime/unix.rs:1661-1707`), when the launch config's trigger is `push` and the session is a primary, call `install_pre_push_hook` for every repository root the review baseline covers. Set `MJ_REVIEW_GATE=1` in the harness environment built in `mj-worker/src/worker_runtime/harness_launch.rs` under the same condition. Children (including reviewer children) never get the variable.

Harness notice. When the trigger is `push` and the primary harness is not Claude Code or Codex, the worker skips the hook, the variable and the `--review-gate` flag, and writes one notice into the transcript at session start through the same path other startup notices use (search `mj-worker/src/worker_runtime/` for the notice that says a Muse runtime continues without MCP tools): "Before-push review is not available for <harness>; this session runs without review." Nothing else changes for that session.

Acceptance: a worker test creates a repository with a remote, installs the hook, commits, and runs `git push` with `MJ_REVIEW_GATE=1`: exit code 1 and the "has not been reviewed" message; after `git update-ref refs/mj/reviewed/<tree> HEAD` the push succeeds; without the variable the push succeeds regardless; an existing `pre-push` that writes a marker file still runs after the gate passes. A test that a `core.hooksPath` repository gets the hook in that directory. A worker test that a push-mode launch config for a Muse profile installs no hook and records the notice.

### Milestone 6: documentation

Update `docs/src/content/docs/turn-review.md` (new section "Review before push" describing the tool, the message, the hook and the Claude/Codex limit; the "Automatic review" section says `trigger = "turn"`; remove the five mentions of the second opinion and the sentence that Settings → Code Review configures the plan approval dialog), `configuration.md` (`[review]` table: `trigger` replaces `enabled`; `enabled` still loads), `cli-reference.md` (`--review <off|turn|push>` replaces `--no-review`), `api-reference.md` (`review.mode` values). Keep the Plain Language style of the existing pages.

## Concrete Steps

All commands run from `/home/jonathan/Projects/mjolnir`. Rust builds use mbx for caching; run `cargo` as usual and do not redirect `target/`.

Build and unit tests for a milestone:

    cargo test -p mj-core -p mj-review -p mj-worker -p mj-controller -p mj-cli -p mj-tui

Expect every test to pass; the counts change per milestone. When a test in `mj-controller/src/review_host/tests.rs` fails after Milestone 2, read the test's name and intent first; most of them encode a restart or race case that must keep holding with a child reviewer.

Install the build for a live check (the install script hands the daemon off to the new build; do not commit while it runs):

    scripts/install.sh

Then, with `[review] trigger = "push"` in `~/.config/mj/config.toml` (or the path `mj config` reports):

    mj new --profile <claude-or-codex-profile> --repo /path/to/repo-with-remote

Ask the agent to make a small change, commit, and push. Expected transcript, in order: the agent calls `request_review` and the tool result says `accepted` with a base and head; a child session named `review` appears in the dashboard; a message "Review clean for commits … you may push" or "Review … found:" arrives; a push attempted before the clean verdict ends with `mj: refs/heads/… has not been reviewed.`

## Validation and Acceptance

Milestone 1 is accepted when a config with `enabled = true` and no `trigger` arms turn review (a new loading test), `mj new --review push` is stored and shown by `mj review status`, and the TUI golden shows the three-way selector.

Milestone 2 is accepted when all `review_host` tests pass with the child reviewer, a live turn review shows a `review` child session in the dashboard, the verdict appears in the review view, Forward sends the corrective prompt as before, and `list_agents` in the parent does not show the reviewer.

Milestone 3 is accepted when the workspace builds and tests with no second-opinion or sidecar code, the plan approval dialog has no Second opinion action, and Settings → Code Review still lists models and efforts for a named profile.

Milestone 4 is accepted when the capture tests for dirty refusal and base selection pass, a clean handback writes `refs/mj/reviewed/<tree>` (check with `git -C <repo> for-each-ref refs/mj/reviewed/`), and the verdict message arrives in a live session.

Milestone 5 is accepted when the hook tests pass and a live push before review is refused with the `mj:` message while a push after a clean review succeeds.

## Idempotence and Recovery

Configuration loading is additive: `enabled` keeps loading forever. Reviewer children that exist when the daemon restarts are found by `child_session_id` in the persisted review state; the existing restart rule (clear the in-flight marker, release the hold, leave the baseline) applies, and the host closes an orphaned reviewer child on startup. Hook installation can run on every session start; the marker line makes it a rewrite, and the `before-mj` rename happens at most once. Removing the feature from a repository is `rm .git/hooks/pre-push && mv .git/hooks/pre-push.before-mj .git/hooks/pre-push` and `git for-each-ref --format='%(refname)' refs/mj/reviewed/ | xargs -n1 git update-ref -d`. A `refs/mj/reviewed/*` ref is only ever created from a clean verdict; nothing else writes under that prefix.

## Artifacts and Notes

The user-visible refusal when a tool request finds dirty tracked files:

    request_review refused: commit or stash these tracked changes first:
      mj-core/src/config.rs
      mj-worker/src/review/capture.rs

The hook's refusal, as git shows it:

    mj: refs/heads/feature (tree 4f2c1a9) has not been reviewed.
    mj: call the mj-agents request_review tool, wait for the verdict message, then push again.
    error: failed to push some refs to 'origin'

## Interfaces and Dependencies

In `mj-core/src/config.rs`:

    pub enum ReviewTrigger { Off, Turn, Push }
    pub struct ReviewConfig { pub trigger: ReviewTrigger, pub tier: Option<String>, pub profile: Option<String>, pub model: Option<String>, pub effort: Option<String> }
    impl ReviewConfig { pub fn arms_turn_review(&self) -> bool; pub fn is_push(&self) -> bool; pub fn for_session(&self, session: Option<&SessionReview>) -> Self; }

In `mj-core/src/subagent.rs`:

    pub enum SubagentKind { Worker, Reviewer }
    pub enum SubagentToolAction { Spawn {..}, ListAgents, SendMessage {..}, CloseAgent {..}, Handback {..}, RequestReview {} }

In `mj-core/src/relay/protocol.rs`:

    pub enum CaptureScope { TurnDelta, PushableCommits }
    ReviewerAction::CaptureDelta { baselines, scope: CaptureScope }
    ReviewerAction::RecordReviewed { trees: BTreeMap<PathBuf, (String /*tree*/, String /*commit*/)> }
    pub const RELAY_PUSH_REVIEW_PROTOCOL: u32 = 43;

In `mj-core/src/mailbox.rs`:

    MailboxBody::ReviewVerdict { review_id: String, repository: PathBuf, base_commit: Option<String>, head_commit: String, verdict: ReviewVerdict }

In `mj-controller/src/review_host/reviewer_child.rs`:

    pub(super) fn start_reviewer_child(&mut self, session_id: &str, job: &ReviewJob) -> Result<String /*child_session_id*/, StartRefusal>;
    HostEvent::ReviewerFinished { session_id: String, child_session_id: String, answer: Option<String> }
    HostCommand::StartPushReview { session_id: String, request_id: String, reply: oneshot::Sender<Result<PushReviewAccepted, StartRefusal>> }

In `mj-worker/src/review/capture.rs`:

    pub fn capture_pushable_commits(runner: &dyn GitCommandRunner, root: &Path, session_start_head: &str) -> Result<RepoDelta, PushCaptureRefusal>;
    pub enum PushCaptureRefusal { DirtyTracked { files: Vec<PathBuf> }, NoHead }
    pub enum PushBase { ReviewPoint { commit: String }, MergeBase { default_branch: String, commit: String }, SessionStart { commit: String } }
    fn default_branch(runner: &dyn GitCommandRunner, root: &Path) -> Option<String>;  // origin/HEAD's branch, else main, else master

In `mj-worker/src/review/hook.rs`:

    pub fn install_pre_push_hook(runner: &dyn GitCommandRunner, root: &Path) -> Result<HookInstall>;
    pub enum HookInstall { Installed, Rewritten, RefusedExistingBackup(PathBuf) }

In `mj-checkpoint/src/archive/git.rs`:

    pub fn record_reviewed_tree(runner: &dyn GitCommandRunner, repository: &Path, tree: &str, commit: &str) -> Result<()>;

Dependencies: none new. Git is already a runtime dependency of the worker (capture and history). The hook is POSIX `sh`.

---

Revision note (2026-10-23): first version, written after the two read-only investigations named in Progress.

Revision note (2026-10-23, later): the user resolved the open points. The push-review base rule changed from an upstream/`origin/HEAD` chain to "last review point if it descends from the merge base with the default branch, else that merge base, else the session-start commit"; the second opinion is removed and the sidecar deleted (new Milestone 3, later milestones renumbered); an unsupported harness in push mode gets a transcript notice instead of a refusal. The Settings discovery dependency on the sidecar was found while scoping the removal and is recorded in Surprises & Discoveries.
