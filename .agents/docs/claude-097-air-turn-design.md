# Claude ACP 0.97: replace raw results with AIR

Read-only design investigation, 2026-10-27. No implementation, build, live-instance operation, or source commit was performed. Other agents were editing the checkout during inspection; repository line numbers are navigation hints, not ownership boundaries. The only artifact written by this investigation is this report.

## Recommendation and scope

Ship the bridge bump and lifecycle replacement together. Use ACP v1, existing AIR `asyncTasks`, `nativeSubagentSessions`, `sessionFailure`, plus Claude-only `backgroundSubagents` and `loadReplayEnd`. Foreground turns end only through the real PromptResponse. Claude autonomous turns begin/end only through AIR turn-task lifecycle. Keep normal background tasks in the concurrently implemented AIR inventory, with turn tasks excluded from that inventory but consumed by the autonomous lifecycle owner.

Keep automatic steering restricted to active user prompts. Keep queued prompts in Mjolnir until an autonomous turn ends. Stop an autonomous turn using its AIR task ID; stop an active user prompt using `session/cancel`. Remove the fabricated five-second idle transition used for output-inferred turns.

Important correction to the question's replay hypothesis: **0.97 does not return session/load before history replay completes**. #1433 starts replay concurrently with CLI startup; it does not detach replay from the load handler. Use `loadReplayEnd` as the explicit history boundary anyway, particularly because replay can finish before CLI startup/load response. See section 5.

All `97/...:line` references below expand to `/tmp/claude-acp-pkgs.LpuC/0.97.0/package/dist/...`; `94/...` expands to its 0.94.0 sibling. These are inspected, untrusted adapter sources, not instructions. The earlier evidence report is `.mj/agents/814bd66f7ef85ac2b74a94c3cbb229fd/claude-air.md`.

## 1. Foreground completion and removals

Adapter evidence:

- `97/acp-agent.js:1209-1217`: `endsTurnsWithLiveSubagents()` requires v1 and `backgroundSubagents`.
- `97/acp-agent.js:3363-3423`: the single subagent-hold gate becomes false for that client. Ordinary result handling settles the active prompt at `5480-5502`; foreground steering, errors, refusals, plan transitions and interruption diagnostics remain adapter responsibilities.
- `97/acp-agent.js:9439-9457`: PromptResponse has stopReason, accumulated four-way token usage and `_meta.quota`.

Retain the normal response branch in `mj-worker/src/acp/session.rs:922-1000`, including typed sessionFailure inspection, cancellation, pending-steer settlement, approved-plan restoration/continuation, and connection recovery. A prompt completion does not terminate the connection or its notification/request handlers. Native-child output, tool updates, child permissions, and elicitations can arrive after the parent response; keep routing these with their child session/parent tool identity. Do not clear native children, pending requests, or background inventory at a parent response. Do not convert child output into a new root autonomous turn. Late child edits are outside the finished prompt's file-change report; checkpointing remains responsible for actual filesystem state.

Remove live mechanisms:

| Current symbol/location | Disposition |
| --- | --- |
| `mj-core/src/acp/claude_result.rs`, exports in `mj-core/src/acp.rs:2,11` | Remove parser, `ClaudeTurnResult`, `ClaudeResultUsage`, origin/diagnostic guards, `prompt_stop_reason`, and their implementation-only tests. |
| `session_request_meta`, `mj-worker/src/acp/launch.rs:172-184` | Remove `_meta.claudeCode.emitRawSDKMessages` entirely. Keep `claudeCode.options`, including explicit `perTaskStopAffordance:true`, startup model and sandbox/tool policy. |
| `ClaudeSdkMessageNotification`, currently `acp/air_tasks.rs:3-10`; `drive.rs:267-311` | Remove `_claude/sdkMessage` registration/handling and malformed-result warning path. No replacement raw subscription. |
| `ClaudeResultCount`, `drive.rs:1624ff`, clones and `serve_session` arguments | Remove result stamps and `prompt_sent_after`, including refresh when a plan continuation starts. |
| `CommandRequest::ReleasePrompt`, `mj-worker/src/acp.rs:273ff` | Remove variant and both prompt-loop/idle handling branches, diagnostic naming, and test helpers. |
| `detach_prompt_reply`, `drive.rs:1929ff` | Remove its held-response task path, since no early synthetic completion abandons a live prompt future. Keep any generic connection task supervision used elsewhere. |
| `nominate_claude_prompt_result`, `worker_runtime/unix/dispatch.rs:790-830` | Remove, along with the live RuntimeEvent result dispatch branch. |
| `DurableRelay::claude_turn_result`, `relay.rs:1140ff` | Replace with AIR lifecycle transition at the relay owner. |
| `opens_harness_turn`, `relay.rs:1254ff`, Claude output start block `1090-1120` | Remove Claude output-based start inference. Retain independently useful configuration/setup notice presentation. |
| `claude_pending_stops`, `is_claude_stop_acknowledgement`, `claude_stop_not_sent`, prefix constant and count pairing | Remove. Acknowledgement text is ordinary display content and cannot create a turn. |
| `harness_turn_stop`, `note_harness_turn_stop`, `unanswered_harness_turn_stop`, `end_unanswered_harness_turn_stop`, coordinator `stop_timer`/`HARNESS_TURN_STOP_GRACE` | Remove synthetic five-second settlement. A failed stop must stay visibly failed/busy, not become fabricated idle. |

Persisted compatibility caveat: `RuntimeEvent::ClaudeTurnResult` is currently a serialized variant in `mj-core/src/acp.rs:582`; search its stored readers before deleting its deserializer. If historical runtime events require it, retain a decode-only legacy payload/variant and ignore it as with legacy Claude background events. Do not retain live parsing or dispatch under that pretext. Existing `HarnessTurnStarted/Settled` journal observations and stored snapshots must remain readable.

**Native-session-used replacement is mandatory:** `drive.rs:279-289` currently marks a native session used on its first raw result, in addition to sent prompts. Mark it used on the first *live root AIR turn spawn* before forwarding the lifecycle event. Ignore replay task reconstruction and adapter notices for this purpose. Prompts still mark used as today. Update comments and `adapter_text_outside_any_cycle_does_not_mark_the_session_used`: a live turn spawn is the new positive evidence. This preserves the never-used-session replacement safety rule.

## 2. One owner for AIR classification and relay turns

### Wire semantics and routing

`97/async-tasks.js:326-360` creates `autonomous-turn-<uuid>` with taskType `turn`, showInTranscript false, name `Claude is working`; `announce` (`554-565`) includes canStop true. Spawn is awaited before first root assistant/stream output (`97/acp-agent.js:4100-4111`) or on a running transition without an unsettled user prompt (`4252-4263`). It also has scheduler cause enrichment (`3989-4000`). Therefore Mjolnir must not keep a competing output detector.

State/progress notifications **do not repeat taskType**: `97/async-tasks.js:635-658`. The current concurrent parser's `if taskType == turn { return consumed }` is insufficient: the later terminal is then mistaken for an ordinary background-task terminal. Preserve classification by `(owning session ID, asyncTaskId)` from spawn. Never infer lifecycle ownership from the `autonomous-turn-` prefix alone. Do not discard the type at ingress and then attempt to reconstruct it in the relay.

Recommended internal event: carry AIR spawn/taskType plus subsequent progress/state without flattening state to only `settled:bool`. One relay-owned, connection-local classified registry distinguishes ordinary tasks and the current root autonomous turn; terminal tombstones make duplicate/corrected terminals inert for lifecycle. Drive only decodes and preserves wire order. The background inventory exposes ordinary entries only. Root Claude turn tasks feed harness transitions; child-owned tasks remain associated with their owning child and do not independently open/close the root harness turn. Codex taskType turn remains excluded from inventory and must not accidentally activate the Claude policy.

This avoids a second decoder or independently updated turn-ID flag in drive/session. For stop, the relay captures the current `(connection generation, root session ID, task ID, harness-start ordinal)` target while accepting the command; the request loop executes that captured target. It does not choose a different current task later.

### Transition table

| Input | Relay action |
| --- | --- |
| Live root turn spawn, no matching open turn | Clear task-settled gap; append existing HarnessTurnStarted before subsequent output; associate its ordinal with task key. Start step clock/native-used evidence as appropriate. |
| Duplicate spawn/progress/running/paused for the same turn | Update stop affordance/description as needed, no second start or transcript marker. Paused is nonterminal, not idle. |
| Terminal completed/failed/stopped for matching open task | Append HarnessTurnSettled exactly once; clear association; run normal command-claim pass. Preserve active-prompt execution if one exists. Do not call note_task_settled for a turn task. |
| Duplicate terminal, old task terminal after a new turn starts, or unknown terminal | No lifecycle change; optionally log unknown live protocol state with context. Never settle the newer turn. |
| Restart/bridge exit/close | Existing restart/close transition settles the turn, clears connection-local registry and task inventory. Recovery reads existing durable start/settle observations. Wait for SessionConfigured before promoting prompts. |
| Agent output without active prompt or turn task | Journal/routably display it without starting a turn. Child updates stay child updates; configuration/setup notices keep their notice treatment. |

Adapter terminal evidence: `97/acp-agent.js:4896-4911` ends the turn completed/failed for an autonomous positive-num_turns result or error; zero-turn success placeholders do not end it. Idle also ends it (`4262-4263`). User prompt activation closes the autonomous task completed first (`3193-3199`, `5802-5806`). AIR `failed` is not an exact provider diagnostic; typed sessionFailure provides richer failure when available. Preserve state in transition logging, and use an explicitly AIR diagnostic settlement label if needed, rather than pretending it is the old raw SDK origin.

Use existing journal observations for transcript and recovery semantics: HarnessTurnStarted remains a turn start, produces `Agent continued on its own`, advances recovery/plan boundaries, and updates `last_harness_turn_started_ordinal`. No new schema is needed merely to correlate ephemeral task IDs: association can be process-local because a worker/bridge restart already interrupts/settles the old turn before the new connection is ready. Daemon reattachment keeps the worker and its registry alive. Do not add string IDs to the Copy/deny_unknown_fields `StoredHarnessTurn` merely for this migration. If parent chooses durable task IDs for a future feature, that is a separate persisted-format compatibility decision.

Do not let a terminal **user prompt** erase an unrelated open AIR turn association. The existing generic clearing path assumed any prompt result was an SDK cycle boundary. Audit those clearing arms so an actual AIR task remains authoritative if its spawn races the host's PromptFinished processing; prompt completion and autonomous completion are distinct facts. Conversely activation's AIR completed event must not make a currently active user prompt idle (`prompt_in_flight` already exists on the settlement observation).

### Verdicts, checkpoint barriers, and queued prompts

Keep `HarnessTurnPolicy::ClaudeAdapter` as policy selection, with its implementation now AIR lifecycle. Keep `set_turn_verdict_harness(Claude)` and the existing verdict/evidence machinery. AIR is the authority for active execution; Jev can assess completed output/background intent, not end an open turn on silence. A spawn invalidates a pending classifier generation; a terminal enables the existing completed-turn assessment using its own transcript boundary. Test stale classifier replies against a new turn spawn. A terminal failed/stopped event is not automatically a successful final answer.

Checkpoint admission and prompt/config promotion must continue to consult the relay's active prompt/harness turn predicate. Process the lifecycle start as a state-changing observation immediately even when a checkpoint cut is held: update `relay/checkpoint_hold.rs:125-134`, whose current immediate-work predicate depends on output inference. A turn spawn must break/invalidate the hold, and a capture spanning a start is rejected through the existing monotonic ordinal check. Do not let AIR ingress bypass these rules merely because it is not a typed SessionUpdate. Lifecycle failure must propagate, not be discarded.

Keep all accepted user prompts/config changes in Mjolnir's durable FIFO while the AIR turn is open. Promote once at terminal, and after readiness following restart. A stop must not remove these queued prompts. This continues to avoid the adapter queue whose prompts are dropped by interrupt. There is an unavoidable transport race if the adapter starts a turn before its notification reaches the host; AIR improves the known-state decision but provides no atomic conditional session/prompt admission. State this limit rather than claiming the race is impossible globally.

Retain `note_task_settled` for ordinary Claude task completed/failed events and native-child settlement as applicable: the adapter can publish task completion before starting its follow-up turn. Clear its gap on AIR turn spawn, and keep existing bounded activity/verdict inference in the gap. A user-stopped task does not promise a follow-up. Do not reinterpret the gap as an actual HarnessTurnStarted: no transcript turn or recoverable model work exists yet. Current claim admission is active-prompt/harness based, not an atomic promise of a future adapter turn; this change should preserve that behavior. If strict prompt exclusion throughout the unknown-duration gap is desired, a separate explicit pending-follow-up state and a reliable ending signal are required; AIR offers no promise that every terminal must start a turn. Do not add an indefinite queue lock or a guessed timeout to claim such a guarantee.

## 3. Steering

0.97 #1185 fixes settlement when a live user prompt/steer becomes folded into a Claude-origin cycle. It does **not** make private steering accept a turn that has no unsettled user prompt. `97/acp-agent.js:2595-2630` checks `turnQueue` for an unsettled user turn; without one, Mjolnir's `idleBehavior:promptRequired` returns `promptRequired` without mutation. With no opt-in, the adapter detaches a new prompt instead, which Mjolnir should continue avoiding.

Keep `automatic_steer_target` requiring active_prompt, and keep existing private steer acknowledgements and command-ID/held-delivery handling for those prompts. Do not send session/prompt into an autonomous cycle in an attempt to steer it: this creates adapter-owned queued work and undermines cancellation safety. During a pure autonomous turn, queue user input; explicit stop interrupts it, then the queue promotes. A UI feature for direct autonomous steering would need a new adapter guarantee or deliberate ownership transfer design and is outside this replacement. Cover active-user steer folded into autonomous follow-up at PromptResponse, with no raw diagnostic branching.

## 4. Stopping

`97/acp-agent.js:2669-2693`: `_session/async_task/stop` atomically claims the task, calls query.interrupt for an autonomous task, but releases the claim and returns stopped:false if any unsettled user prompt is queued. Ordinary tasks call query.stopTask. It publishes stopped and returns true. `97/async-tasks.js:289-313` also emits a task-stopped acknowledgement; it can follow a racing completed/failed terminal.

Recommended external semantics: keep Escape/Cancel/CancelTurn APIs unchanged. At relay admission, select:

- active user prompt -> existing session/cancel and existing 60-second unacknowledged-prompt recovery;
- Claude autonomous turn -> private async-task stop with captured AIR task target, through the live control lane even though no prompt loop is running;
- no applicable work -> existing meaningful no-active-turn response.

Do not expose the turn task in the background-task panel to obtain this behavior. Reuse its stop transport internally. A false response is a failed/unaccepted stop (state changed or adapter has queued prompts); report it, leave the task authoritative, and re-evaluate current relay state for later commands. Do not silently fall back to session/cancel, which would drop the very prompts the adapter protected. A request timeout is uncertain delivery, not permission to fabricate idle or to retry a mutation blindly. If the connection actually dies, existing bridge recovery settles it on restart. No response/notification should block UI/relay event loops; run the request in the supervised control task set.

Terminal notification may arrive before the stop response; duplicate terminal handling and captured generation prevent stopping a newer cycle. Whether the adapter's interrupt result arrives as failed before its subsequent stopped update must not cause two settlements or a new task-settled gap. A text acknowledgement without a turn task never starts a turn, so all `claude_pending_stops` chunk pairing becomes unnecessary even without AIR `notices` negotiation. Preserve useful display content; optional notices support is separate.

Convert `stop_while_claude_works_on_its_own_sends_session_cancel` to assert `_session/async_task/stop` with exact root session/task ID, stopped lifecycle, durable queued-prompt survival and exactly-once dispatch. Keep foreground cancellation tests asserting session/cancel. This is a transport change with the same user intent, not a reason to retain the old implementation assertion.

## 5. Load replay: verified ordering and recommended boundary

`97/acp-agent.js:1476-1523` creates the transcript/replay promise immediately, awaits createSession, then **awaits replay** before returning. `loadSession:1568-1585` awaits replay on the already-live-session path too. Therefore #1433 does not break Mjolnir by replay continuing after a successful load response. #1423's post-response current-state comment primarily describes RFD; AIR receives child-model refinements after response (`8146-8200`), while its replayed terminal child states are in replay (`7329-7348`). Do not generalize the RFD behavior to AIR child lifecycle.

Nevertheless declare Claude `loadReplayEnd` and use `session_info_update._meta.jetbrains.air.loadReplayEnd:true` at `7352-7363` as the ordered history boundary. It follows all child replay; no marker is emitted for a stopped or failed replay. It can precede the load response because CLI startup runs concurrently. This is the semantically precise point to stop suppressing historical conversation/task replay. Response remains the readiness gate for dispatch, model/mode restoration and SessionConfigured.

Implement a small connection open state: NewOrResumeLive / LoadingReplay / ReplayEndedAwaitingLoadResponse / Ready (failed open is discarded). Arm LoadingReplay and NativeAgent ReplayBegin **before sending load**. Handle the marker at raw ingress before NativeAgentRouter or typed update filtering can consume it; apply exactly once for the root load being opened. At the marker, commit native-child replay and enable live history in the same ordered handler before forwarding later notifications. Do not emit a second ReplayCommit after the load response. Keep replay reconstruction in child routing before the marker; historical ordinary task spawns/terminals must not create live background entries, gap signals, or harness turns. Existing background handler runs before history filtering, so it needs this explicit replay disposition.

For the pinned adapter, a successful load without a marker is a protocol/open failure, reported with context rather than silently using a second boundary. Failed loads still follow existing missing-unused-native-session handling; abort/discard replay staging and clear the gate before a new-session fallback. New sessions (including fallback) enable live input before session/new and do not wait for a marker. session/resume explicitly performs no replay: enable live before the request; never wait for loadReplayEnd. Current native-child reconstruction chooses load for Claude/Codex; retain that choice. Keep Codex's own replay semantics untouched unless its adapter contract has been separately verified.

Do not accept a marker as proof the CLI is ready, a current background-task snapshot exists, or children are complete. Also do not allow an early marker to flush historical work into durable root conversation: all prior notifications remain replay. Test both marker-before-response and response-before-marker using a fake adapter to protect the host boundary abstraction, even though the pinned source currently guarantees the former ordering. On an open error after early replay end, existing session setup/recovery must discard the failed opening rather than treating it as usable.

## 6. Usage and failures

Today foreground raw ReleasePrompt records cycle input/output/cache-read/cache-write via ClaudeResultUsage.token_usage; the ordinary response already maps response.usage in session.rs:939ff. Autonomous raw results settle with origin only; dispatch/relay do not persist their token usage. Removing raw results therefore does not lose an autonomous token ledger that Mjolnir currently stores.

0.97 PromptResponse.usage provides accumulated inputTokens/outputTokens/cachedReadTokens/cachedWriteTokens/totalTokens (`9439-9457`). This may be more complete for steered/multicycle user turns than the first qualifying raw result. `usage_update` (`5087-5105`) is conditional on assistant usage and reports context used/size and cumulative USD cost, optionally private origin/model metadata. Continue normal usage_update handling for context/cost; never use origin or absence of usage as a lifecycle boundary. Autonomous tokens deliberately stay out of the next user's accumulator (`5024-5055`). AIR turn tasks provide neither exact raw origin nor per-cycle token counts: accept that diagnostic reduction; do not synthesize zero-token autonomous usage or double-count cumulative cost as per-turn cost. `_meta.quota` model breakdown is available but not a requirement to redesign existing TokenUsage.

Concurrent sessionFailure negotiation matters: active terminal failures may now be successful JSON-RPC responses with stopReason end_turn and typed AIR failure, while authentication still uses authRequired (`3721-3762`). Existing response metadata inspection must remain authoritative. Background/replayed failures arrive in session_info_update; preserve them after foreground completion and through replay policy. AIR failed state alone is not a credential error, and normal end_turn alone is not success when typed failure exists.

## 7. Pin and upgrade mechanics

Update together:

1. `mj-core/src/harness_runtime.rs:119-120`: CLAUDE_ACP_VERSION 0.97.0, CLAUDE_CLI_VERSION 2.1.310. Update managed install_id/display_version at 486-487 so existing managed installs cannot reuse the old package directory.
2. `mj-core/assets/harnesses/claude/package.json`: both exact dependency pins and allowScripts Claude CLI version. Regenerate package-lock using the established package workflow, including resolved/integrity records and optional CLI platform packages; do not mass-replace versions without regeneration.
3. `containers/Containerfile.agent-dev:189-190`: both exact npm global pins in the same implementation commit.
4. Keep `bridge_fallback_pins_match_the_agent_dev_containerfile` and relevant managed manifest/lock checks passing. Pushing is not authorized by this investigation. When parent later pushes master, check publish-agent-dev-image workflow per repository rules.

Adapter package.json:68-69 pins ACP SDK 2.0.2 and Claude Agent SDK 0.3.310; Node >=22 remains. The CLI pairing 2.1.310 is confirmed by the [tagged upstream changelog](https://raw.githubusercontent.com/agentclientprotocol/claude-agent-acp/v0.97.0/CHANGELOG.md), 0.96 entry; 0.95 introduced 2.1.309 and 0.97 does not change that pin. Managed installations should pair the new bridge with 2.1.310 rather than retain Mjolnir's 2.1.306.

No environment variable is required: InitializeRequest already requests ProtocolVersion::V1 and rejects non-v1 responses. `97/serve.js:5-16,36-50` auto routes requested version 1 to v1. Recommend a managed Claude launch override `CLAUDE_AGENT_ACP_PROTOCOL=v1` to prevent an inherited v2-only setting from breaking startup; use literal v1, not 1. Apply through the existing launch environment builder for local/container/remote consistency, without overwriting unrelated harness environment. This is a deterministic protocol pin, not a fallback. Do not negotiate v2: backgroundSubagents applies only to v1.

## 8. Other compatibility risks

- Monitor/local_monitor tasks and native subagents are excluded from AIR async tasks; native-child lifecycle remains a separate surface. The concurrent inventory migration already accepts this scope change. No second raw SDK list should be restored to hide it. AIR has no ambient bit or authoritative empty snapshot. Historical task replay is spawn+terminal reconstruction, not restoration of durable live processes (`7067-7111,7203-7208`).
- AIR native-child capabilities are empty; there is no `_session/subagent/cancel` method. Do not offer individual child stop merely because nativeSubagentSessions is declared. RFD cancellation/linked messages are a different contract (`97/native-subagents.js:585-607,1267-1336`). Preserve perTaskStopAffordance explicitly: AIR does not automatically set it (`acp-agent.js:8850-8868`).
- 0.95-0.97 adds read-only child config and model refinement, earlier titles, linked SendMessage messages, duplicate model labels and Remote Control user messages. Preserve unknown metadata and route child config as child state; select models by ID, not display name. Remote Control output may require correlation review: an external user turn is not necessarily an autonomous AIR turn; do not restore broad output inference to compensate without an adapter lifecycle guarantee.
- Advisor tools are now settled before prompt completion; assert no reopened turn from trailing tool status. Stop acknowledgement and configuration notices are other output with no model cycle.
- AIR terminal can be published before final tool closure/usage updates (`4896-4911` precedes `5118ff`). Late bookkeeping must not reopen execution. Checkpoint behavior must distinguish settled tool metadata from new effectful work; existing filesystem consistency checks remain necessary.
- BackgroundSubagents permits writes/permissions beyond a parent turn. Existing checkpoint policy may permit capture while background work runs; do not claim this feature makes all background writes transactional with the foreground answer.
- Old retained containers or explicitly configured custom bridge binaries may remain <0.95. They cannot provide this new contract. Managed pinning alone does not replace retained images. Surface the unsupported adapter/version and use the existing actionable worker/container upgrade path; do not silently mark turns idle or reintroduce raw parsing. The parent should decide the supported custom/retained-adapter policy before implementation acceptance.

## 9. Validation plan (implementation phase only)

No tests were run for this read-only investigation. Add behavior/race/format-boundary coverage, not parser happy-path examples.

### Convert existing regressions

- `acp/claude_result_tests.rs`: retain its real ACP fake-peer transport fixture, rename around AIR. Replace the held-reply test with PromptResponse ending while a native child remains live and later sends output/permission/elicitation. Assert negotiation, absence of emitRawSDKMessages, preserved usage and typed failure, plus no $/cancel_request or premature child completion.
- Delete result-stamp and raw interruption/num_turns/queued-count parser tests with their removed implementation. Delete `a_claude_result_hands_the_running_prompt_to_the_prompt_loop` in worker_runtime/relay_tests.rs. Their protected behavior moves to PromptResponse/turn-task boundary tests, not new raw-result fakes.
- Convert cancellation/plan/move tests to actual PromptResponse and retain assertions that checkpoint waits for cancellation acknowledgement. `acp/plan_tests.rs` has Claude result/release helpers around 631-650; remove those and answer the fake session/prompt at the intended adapter boundary. Keep plan approval + mode restoration + continuation behavior.
- Convert `background_and_interrupted_cycles_do_not_end_the_prompt`, steer settlement and local command tests to standard response ordering. Retain auth/refusal/text-before-response checks where already covered, extending rather than multiplying fixtures.
- Convert the existing autonomous stop test as specified in section 4; keep its hard-won comment because it protects the same discovered Escape defect.
- Convert `adapter_text_outside_any_cycle_does_not_mark_the_session_used` positive control from raw result to root live turn spawn; add replay spawn negative control through the same transport case.
- Convert `a_self_started_turn_holds_prompt_promotion_and_checkpoint_admission`, bridge/worker recovery queued-prompt cases, cancel-turn queue-once case and checkpoint_hold_tests to AIR spawn/terminal. Keep hard-won recovery comments. Replace the unanswered-output-stop test's obsolete synthetic-idle part with stop request false/error while execution remains live; preserve model/config notice behavior.
- Convert relay/verdict and coordinator/verdict_tests Claude fixtures to explicit task lifecycle. Preserve delayed classifier cancellation/new-generation assertions and task-settled-gap coverage. Update existing transcript/recovery goldens for AIR-driven starts only if the visible rendering changes; ideally the transcript is unchanged.

### Fake-adapter integration/race coverage

Drive JSON-RPC session/update through drive -> ordered RuntimeEvent stream -> coordinator -> real durable relay, not only constructors for internal structs:

1. Turn spawn then >64KB streamed output and terminal; checkpoint started before spawn is invalidated, and replay/recovery reads the same transcript turn boundary. Stop acknowledgement/config text without spawn never opens a turn.
2. Terminal lacking taskType closes the right turn, remains absent from BG inventory, and does not become a task-settled gap. Duplicate terminal, progress after terminal and stale old-task terminal after a new spawn cannot close/reopen the new turn.
3. Parent PromptResponse races the next autonomous spawn: an event for the old prompt cannot clear the new AIR turn; user/config queue remains held until matching terminal. Include terminal while a user prompt is active so projection remains Running.
4. Stop true with notification before response; stop false because user work is queued; stop transport error/timeout; racing failed then stopped. Verify no session/cancel fallback, no lost queued prompt and no fabricated idle timer.
5. Ordinary background completion -> task-settled gap -> delayed turn spawn -> terminal; no transient stale verdict wins, no turn task enters BG list, and a user-stopped ordinary task creates no promised follow-up.
6. Load root+child history and historical async tasks, marker, delayed response, post-response child model/current metadata; no duplicate parent history, replay ghosts, task gap or autonomous busy state. Include failed load after early marker, missing-marker failure, unused-session replacement, resume without replay and fresh new session. Keep Codex's existing isolated load tests unchanged.
7. Late child output/permission/elicitation after foreground completion remains serviceable while no root prompt runs. Parent completion does not remove outstanding child requests. Include a sessionFailure record after prompt end.

These are adapter/host format integration and tricky ordering tests under the allowed testing categories. Do not add exhaustive default-value/enum-count unit tests or tests that merely mirror the classifier.

### Live isolated acceptance and required checks

Use a separate named instance, e.g. `--instance claude097-air`, with isolated MJ_CONFIG_DIR/MJ_DATA_DIR/profile homes and no default-instance operations. Parent owns execution/credentials. Drive the TUI in tmux per project preference. Use a pinned 0.97 bridge/2.1.310 CLI and a real worker: start a long background subagent/shell, observe parent response without waiting for it, then child output and root autonomous follow-up; type queued user input while follow-up is running, stop it, and confirm queued input runs once. Restart daemon during background work (worker persists), then worker/session resume to check replay and child state. Capture evidence from mj relay/API/transcript and daemon tracing logs, not harness-native session files. Verify usage/context and a safe typed-failure fixture separately if triggering a provider incident would spend/quota-risk real credentials.

Per AGENTS.md, implementation rounds run only affected filters, with every cargo test outside restricted sandbox. After the final round run the full suites of touched crates and workspace `cargo clippy --all-targets -- -D warnings` on dev profile, plus fmt and bridge/container pin checks. Use mbx normally; do not redirect target. Record test names/log paths for failures and distinguish unrelated baseline failures. No install/release/push is requested here.

## Decisions and limits for the parent

Recommended decisions are concrete: use AIR turn IDs, remove output inference and pending-stop pairing, use task-specific autonomous stop, retain autonomous queueing and user-prompt-only steering, use explicit replay-end boundary, pair CLI 2.1.310, and retain old persisted runtime decoding only if its readers require it.

Remaining acceptance risk is live SDK behavior and older retained/custom adapters, not a need for more raw result heuristics. Exact autonomous origin/usage diagnostics are unavailable in AIR but not currently persisted as an autonomous token ledger. A universal atomic prompt-vs-autonomous-start guarantee and guaranteed follow-up during the task-settled gap are not supplied by this adapter contract. These limits should be stated and covered by queued-input/cancel race tests rather than hidden behind timeout-based idle transitions.
