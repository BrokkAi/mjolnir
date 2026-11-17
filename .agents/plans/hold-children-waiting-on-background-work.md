# Hold sub-agents that end their turn while waiting on their own background work

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

Mjolnir runs coding agents as sub-agents ("children") of another agent session (the "parent"). A child is supposed to finish its task by calling the `handback` tool of the `mj-agents` MCP server, which delivers its report to the parent. Many children instead start a long command in the background (a `cargo test`, a build, an evaluation script), say "Waiting for the tests to finish", and end their turn, the way they would in an interactive session where a human waits with them. Antigravity (Gemini) does this by design: its `run_command` tool moves long commands to the background and promises to tell the model when they finish. Claude does it with background Bash and Monitor.

Today Mjolnir treats the end of that turn as the end of the task. It sends one fixed `[handback reminder]` prompt; the child, still waiting, ends its turn again; Mjolnir then reports the child's last message ("Waiting for X") to the parent as its result and "parks" the child. Parking stops the child's worker and every process under it, so the test the child was waiting for is killed and its result is lost. In a scan of sessions created on 2026-11-15 and 2026-11-16, 40 of 75 reminders sent to Antigravity and Claude children were answered by another "waiting" message.

After this change a child that ends its turn while processes it started are still running is held instead: the reminder names those processes and tells the child it may wait; if the child does wait, Mjolnir sends no report and does not park it; when the processes exit, Mjolnir prompts the child to continue; after 30 minutes of waiting Mjolnir prompts it to hand back what it has. Separately, whenever Mjolnir would park a child whose processes are still running, it first waits until the parent has finished the turn in which it received the child's report, so a parent that answers "continue" does not lose a long-running test.

To see it working: in an isolated Mjolnir instance, spawn a child with the instruction to run `sleep 90` in the background and end its turn without handing back. The child's `list_agents` entry shows it `running` and "waiting on" the `sleep` process; the parent receives no report; about 90 seconds later the child receives a "[background work finished]" prompt, hands back, and only then does the parent get the report.


## Progress

- [x] (2026-11-16 05:40Z) Diagnosis, measurements, and mapping (see Surprises & Discoveries and Decision Log).
- [x] (2026-11-16 06:10Z) Shared definitions added on master: `mj_core::work_facts::WorkLiveness`, `mj_core::subagent::HOLD_GIVE_UP_MS`, `mj_core::subagent::HoldFollowUpKind`.
- [x] (2026-11-16 07:05Z) Milestone 1: worker publishes work-process liveness; relay protocol 67. Commits 625d43fee (interfaces, prompt texts) and 041c3ba66 (watcher) in the child-hold-relay worktree; `RelayOperationalState` also carries `work_liveness` and `undelivered_mailbox_event_keys` for the daemon.
- [x] (2026-11-16 08:10Z) Milestone 2: the report rule learns the hold; reminder names processes; follow-up prompts. Commits 35fa23280 and later in the child-hold-daemon worktree.
- [x] (2026-11-16 08:10Z) Milestone 3: the daemon holds, wakes, gives up, and delays parks by one parent turn. Head 3a0f5b122, fast-forwarded onto local master with both relay commits.
- [x] (2026-11-16 09:30Z) Integrated review (`.mj/agents/cc1b124b3cbf2eca4b861fe4528b8740/REVIEW.md`) and round-1 fixes, commit d66562ff2 on local master.
- [x] (2026-11-16 10:05Z) Milestone 4 live run against 3a0f5b122 (real Claude Code, isolated instance `child-hold-e2e`): hold and wake, give-up with the parent-turn grace (including a parent "continue"), and a daemon restart during a hold all passed; evidence in `.mj/agents/a7ac8fa39e300c356cbadf1ef408ca58/EVIDENCE.md`.
- [x] (2026-11-16 10:40Z) Round-2 fix 215ee42ed: once the park gate is open, the daemon stops the child's stoppable harness background tasks through the worker's existing per-task stop, then parks. Tasks the harness marks unstoppable are left, so such a child still retries its park as before.
- [x] (2026-11-16 11:05Z) Round 3 fix 534aeb391: the live recheck showed that stopping a Claude task on an idle child opened a harness turn that never settled. The worker's stop-acknowledgement match required the bare `**Task stopped by user:** ` prefix, but the adapter wraps notices in a blockquote (`\n\n› **Task stopped by user:** …`). The match now accepts that shape. The bug predates this change: a user's `mj stop-task` on an idle Claude session hit the same stuck turn.
- [x] (2026-11-16 11:25Z) Live recheck on 534aeb391: give-up, handback during the parent's turn, park held until that turn ended, then `stopped … stopped=1` and `park finished outcome=Parked` 2.4 s later with the process gone and no harness turn opened. Hold and wake rechecked on 215ee42ed.
- [x] (2026-11-16 12:20Z) Review of rounds 1-3 (`.mj/agents/b05d45905d3d6663ca62da206b490289/REVIEW.md`) and round-4 fixes, d5e71c007.
- [x] (2026-11-16 12:00Z) Full suites on 534aeb391: mj-core passed, mj-worker 854 passed (one review-hook test hung in a local `git push` under load and passed alone), mj-controller 2,674 passed. Workspace clippy clean on d66562ff2 and on every later round.
- [x] (2026-11-16 12:55Z) Full controller suite on d5e71c007 (2,675 passed) and live recheck on d5e71c007: give-up park (task stopped, `outcome=Parked` 2.5 s later, process gone, no harness turn) and the parent "continue" variant (no stop or park after the first parent turn; parked only after the second) both passed; evidence `.mj/agents/e849d939cb90fc35fd39a5193164545c/EVIDENCE.md`.
- [ ] Push and install.
- [ ] Final full validation (in progress on d66562ff2), live rerun of the give-up park on the final build.
- [ ] Milestone 4: end-to-end check in an isolated instance with a real harness.
- [ ] Integrated review, full validation, commit and push.


## Surprises & Discoveries

- Observation: the worker already sees the awaited work. Every finished turn of a managed child carries `WorkFacts` (processes the agent left running, with pid and start identity), captured by the child's own worker, for every harness. Session 4ae25d4d's parent was told "Child task podman-exec-proxy-teardown finished: completed; 4 processes still running" with the `mbx test`, `cargo` and `rustc` processes listed, and the child was parked anyway.
  Evidence: in 32 of the measured waiting cases that had a finish report, 30 listed the awaited process (94%); the two misses were a worker older than work facts and a process that had already exited.

- Observation: nothing consumes `ProcessFacts.identities`. Its doc comment promises "the park's later liveness check", which was never written, and `mj-worker/src/work_facts.rs` still mentions a `RelayRequest::WorkFacts` that does not exist. Liveness over time is new code.

- Observation: the top-level end-of-turn classifier (GLiDE) detects these waits only when told the truth about background work. Run as the worker would run it today it caught 12 of 49 real waits, because the runtime facts say "background commands: 0" for harnesses whose background work Mjolnir cannot see; with "unknown, judge from the transcript" it caught 49 of 49 with 1 false Wait in 71 (and that one had already called handback). This plan does not use the classifier; the result is recorded so a later change can.

- Observation: an idle Antigravity session never starts a turn by itself when a background task finishes; its "Task … finished" message reaches the model only with the next prompt. Its `schedule` tool (a timer prompt) was called in 13 of 17 sessions and never fired before Mjolnir parked the child. So Mjolnir must send the wake prompt.

- Observation: `RelayCommand` is `deny_unknown_fields`, so a protocol-66 worker rejects a `HandbackReminder` that carries `running`. The field is skipped when empty and `minimum_protocol` is 67 only when it is non-empty, so older workers keep receiving the plain reminder.
  Evidence: relay implementer's decode check, `.mj/agents/afbc9028d45d722ad01a6bc51c9af751/NOTES.md`.

- Observation: in the live run, a park after the grace never completed for a Claude child whose own background Bash task was still tracked: park admission (`IdleWorkspaceLease::acquire_for_upgrade`) refuses while harness background commands run, so the child retried `Busy` every 30 seconds indefinitely. This predates the change, but it defeats the give-up.
  Evidence: live run m2, child 5cb774eec25c6e6ddd3f3d94f126622d, daemon log "an idle worker is not replaced … background commands are running".

- Observation: processes Claude's Bash tool backgrounds are reparented to init, so work facts list them "detached from the harness". The liveness watcher still counts them (they carry the session marker), but a park stops only the worker's process tree, so a detached process survives the park, as the finish report already warns. Claude Code's own transient `ps -o command= -p <pid>` helpers also show up in `waiting_on` for a moment.
  Evidence: `.mj/agents/a7ac8fa39e300c356cbadf1ef408ca58/raw/list-agents-s1-1.txt`.

- Observation: measured durations of background work the agent waited for, 1057 tasks since 2026-11-01: median 7 minutes, 90th percentile 30 minutes, 99th percentile 115 minutes, longest 129 minutes (Antigravity alone: 99th percentile 18 minutes, longest 38 minutes). About 100 never-ending poll loops (`until grep -q …`, `while … sleep`) were still running at finish or park; most were legitimate watchers, the leftovers were things like detached `sleep 1000`.


## Decision Log

- Decision: detect waiting from the worker's work-fact processes, not from a per-harness task inventory and not from the classifier.
  Rationale: one mechanism for every harness; the facts already exist and covered 30 of 32 cases. A per-harness inventory has been fragile for Claude for months and does not exist for Antigravity.
  Date/Author: 2026-11-16, user and coordinator.

- Decision: hold only after the reminder turn also ends without a handback while that turn's work processes are alive. The reminder names the processes and says the child may end its turn to wait.
  Rationale: the reminder lets the agent resolve the ambiguity itself. A child that is finished but left a dev server or watcher running is told "if your task is finished, hand back now", and children that had finished answered the reminder with a handback in the measured data.
  Date/Author: 2026-11-16, coordinator.

- Decision: give up after 30 minutes of holding, measured from the reminder turn's `completed_at_ms`. At that point Mjolnir sends a give-up prompt asking for a handback with what the child has; if that turn also ends without a handback, today's fallback applies (the last message is reported and the child is parked, subject to the parent-turn grace below).
  Rationale: the user chose 30 minutes (the measured 90th percentile is 30 minutes). Agents leave infinite poll processes running, so an unbounded hold is wrong; a long legitimate run that passes 30 minutes is protected by the parent-turn grace, because the parent can answer "continue".
  Date/Author: 2026-11-16, user.

- Decision: any park of a child whose work processes are still alive waits until the parent turn that received the child's report has ended. If the parent sends the child anything before then, the child is not parked.
  Rationale: the user's rule, "give a grace period (say, 1 parent turn) before killing subs' child processes, that way if parent says 'continue' we haven't lost a long-running test". It applies to every park with live processes, including a child that handed back while its processes were running, not only to the give-up.
  Date/Author: 2026-11-16, user.

- Decision: the worker owns work-process liveness and publishes it in its relay snapshot as `work_liveness: Option<WorkLiveness>`; the daemon only reads it. Relay protocol 67. A worker below 67 publishes nothing, and the daemon then behaves exactly as today for that child (no hold, no grace).
  Rationale: process IDs are meaningful only inside the worker's process namespace (remote hosts, containers). One owner per fact (AGENTS.md, Concurrency). Old workers are upgraded when idle, so the feature arrives without user action.
  Date/Author: 2026-11-16, coordinator.

- Decision: the two new prompts ("processes exited" and "give up") are one new guarded relay command, `RelayCommand::HoldFollowUp`, admitted by the worker only while the completed turn it names is still current, exactly like `RelayCommand::HandbackReminder`. Command IDs are deterministic: `hold-wake-<reminder turn completed_ordinal>` and `hold-give-up-<same>`.
  Rationale: a plain prompt would lose the worker-side guard against a stale follow-up overtaking newer parent input, and the protocol bump is needed anyway for liveness. Deterministic IDs make retries idempotent and let the daemon recover after a restart without new store columns.
  Date/Author: 2026-11-16, coordinator.

- Decision: no store schema change. The hold is derived each time from durable inputs: the child's last turn outcome and its `work_liveness` (worker snapshot, journaled), the existing `subagent_handbacks` row, and the clock. The parent-turn gate is derived from the parent's snapshot (`delivered_mailbox_event_keys` and its last turn outcome).
  Rationale: level-triggered state survives daemon restarts without a migration, and no daemon memory holds anything the worker or database cannot give back (AGENTS.md, Control plane and data plane).
  Date/Author: 2026-11-16, coordinator.

- Decision: do not add work-fact processes to the park admission check (`activity::quiet_at`). The hold and the grace are daemon decisions made before a park is attempted; once the grace has ended, stopping those processes is intended. The one new park-time check is that the child has no undelivered parent-to-child mailbox rows, made inside the park's lifecycle operation.
  Rationale: adding processes to `quiet_at` would make every child with a leftover watcher unparkable and create a second predicate for "child busy". A parent message sent during the grace turn is enqueued in the outbox before that turn ends, so checking the outbox inside the serialized park decision sees it.
  Date/Author: 2026-11-16, coordinator.

- Decision: a parent turn that never ends keeps the child unparked; there is no timeout on the grace. If the parent leaves `Running` (stopped, closing, error, destroyed), the gate opens.
  Rationale: AGENTS.md forbids speculative timeouts; a stuck parent is visible to the user and the child costs only a slot meanwhile.
  Date/Author: 2026-11-16, coordinator.


- Decision: deviations taken during implementation. (1) The daemon reads worker state through `RelayOperationalState`, not `RelaySnapshot`, so `work_liveness` and `undelivered_mailbox_event_keys` (pending plus hook-leased mailbox events) were added there. (2) The park gate does not read a delivery ordinal from `delivered_mailbox_event_keys` (a 512-entry map not worth publishing on every change). It opens when the report's outbox row is accepted, the parent's snapshot has reached that `accepted_ordinal`, the report key is no longer undelivered, and the parent is idle with nothing queued: idle with the event delivered means the turn that received it has ended. (3) Store-based surfaces get liveness by re-asking `report_state` with the live worker's `work_liveness` inside `observe_child_resolution`, the funnel every child read passes through. (4) `remind_subagent_to_hand_back` returns `ReportNudge { NotOwed, Pending, Holding }` and sends the reminder or the due follow-up from one `report_state` evaluation, and the loop evaluates every reminder turn again on liveness changes and at the give-up time.
  Rationale: see `.mj/agents/17d9d8561ee828a3999d9cd4cc351b57/NOTES.md` in the coordinator's clone; each keeps one owner per fact.
  Date/Author: 2026-11-16, daemon implementer, accepted by coordinator pending review.


- Decision: review round 1. A failed follow-up submit is retried with the delegation loop's backoff unless the worker refuses it as stale (the completion is no longer current); the worker's guard now separates that permanent refusal from a retryable "cannot take a prompt now". `report_state` takes `Liveness { Unknown, Known(Option<WorkLiveness>) }`: a caller without a connected worker view passes `Unknown`, which on a reminder turn is not final (running, no report, not handed back); a connected worker at protocol 66 or older publishes nothing and is `Known(None)`, keeping today's behaviour. `ChildProgress` is built with its liveness so its report is never read before liveness applies. `work_liveness` is cleared when the next prompt starts, not when it is queued, so a dropped follow-up leaves the child held rather than parked with live processes.
  Rationale: the review found that any non-stale submit failure stranded a held child forever, and that "worker not reachable" was read as "no processes" by the stopped-child notice and the session wait.
  Date/Author: 2026-11-16, coordinator.

- Decision: accepted without change: the 5-second liveness scan also runs for a child that already handed back while it keeps processes (it is parked after the parent's grace turn, which ends the scan); a journal write happens under the relay lock on a tokio task, like the relay's other writes; on a remote worker the give-up time mixes the worker's `completed_at_ms` with the daemon's clock, so skew shifts it by the skew; a follow-up the user removes from the queue on purpose leaves the child held until it receives input (a dropped wake still reaches the give-up).
  Rationale: each is bounded or visible, and fixing it would add machinery for an unobserved failure.
  Date/Author: 2026-11-16, coordinator.


- Decision: review round 4. The session wait derives a child's liveness through the same `live_work` function as the store paths (a parked child is `Known(None)` everywhere). The task stop runs inside the park's lifecycle operation, after the undelivered parent-to-child outbox check and before the idle reservation, so a parent "continue" sent during the grace turn can never lose a harness-tracked task, and the stop is bounded by the park's timeout; a task that already ended is not an error and one failed stop does not skip the rest. Each delegation job carries the observation versions it judged, and a newer observation published while it ran makes the loop judge the child again at once instead of waiting (no lost wake-up). `submit_actor_command` no longer reconnects on a retryable `InvalidState` refusal ("not now"): reconnecting changes nothing about the worker's state and can drop a checkpoint barrier the connection owns; the caller retries the same command id.
  Rationale: the second review found two predicates for a parked child's liveness, a check-then-act task stop outside the park's serialized decision, and a window in which a liveness change could be missed for up to the 30-minute give-up.
  Date/Author: 2026-11-16, coordinator.

- Decision: accepted without change: a refusal from a worker started only to make a checkpoint (`checkpoint_only`) is classified as stale, so a held child in that state falls back. Harness tasks marked unstoppable are not stopped, so such a child keeps retrying its park as before this change.
  Rationale: both are rare, visible in `list_agents`, and no worse than before.
  Date/Author: 2026-11-16, coordinator.


## Outcomes & Retrospective

(2026-11-16) A child that ends its turn while processes it started are still running is now held instead of reported and parked: the reminder names the processes, the child is woken when they exit, and it is asked to hand back after 30 minutes. A park that would stop live processes waits for the parent turn that received the report, and a parent message in that turn cancels it. Detection uses the worker's existing work facts for every harness, so no per-harness inventory was built; live runs with real Claude Code showed the hold, the wake, the give-up, the grace (including a parent "continue") and a daemon restart each behaving exactly once. Two pre-existing defects surfaced on the way and were fixed: a park never completed for a child whose harness still tracked a background task, and Claude's blockquote-wrapped "Task stopped by user" acknowledgement opened a turn that never settled on an idle session (which also affected a user's `mj stop-task`). Remaining limits: a process the harness detaches from the worker's tree (Claude's `cmd &`) holds the child but survives the park; harness tasks marked unstoppable still block the park; Antigravity's own background-task timer is never used, because Mjolnir's wake replaces it. Lesson: measure the shape of the failure first; the classifier study showed the detection signal was already available and only the runtime facts fed to it were wrong.


## Context and Orientation

Terms used in this plan:

The daemon (`mj daemon-run`, crate `mj-controller`) is the control plane: it owns the SQLite store, session records, the HTTP API and the delegation loop that reacts to children finishing. A worker (crate `mj-worker`, one per session, local or remote) is the data plane: it runs the harness (the coding agent program, such as Claude Code or Antigravity), owns the relay journal (an append-only log of commands and observations) and publishes a relay snapshot (the projected state of the session) that the daemon reads. Relay types live in `mj-core/src/relay.rs` and `mj-core/src/relay/snapshot.rs`; `RELAY_PROTOCOL_VERSION` in `mj-core/src/relay.rs` is the version a worker and daemon negotiate, currently 66. A turn is one prompt-to-reply cycle of the agent; `MaterializedTurnOutcome` (`mj-core/src/state.rs`) is the record of the most recent finished turn, with `command_id`, `completed_ordinal` (its journal position), `completed_at_ms` and, for managed children, `work_facts`.

Work facts (`mj-core/src/work_facts.rs`, captured by `mj-worker/src/work_facts.rs` `capture`) are what a child's worker observed when a turn ended: the processes the agent left running and the working tree. A work process is a descendant of the worker that started after the harness was configured, or any process carrying the session's `MJ_SESSION_ID` environment variable (which catches `setsid` and `nohup` jobs). `ProcessFacts.identities` lists every such process as a pid plus start identity, so a reused pid is not mistaken for the original. Capture runs in `capture_while_open` (`mj-worker/src/worker_runtime/unix/dispatch.rs`, near line 590) when a managed non-reviewer child's prompt finishes, and the facts ride inside the turn outcome.

The report rule is `mj_core::subagent::report_state` (`mj-core/src/subagent.rs`, near line 1367). It decides, from the child's handback row (`SubagentReport`), its last turn outcome and the commands in flight, whether the child's report is `Delivered(text)`, `Pending { remind }` (a reminder is owed or on its way), or `Fallback` (report the last message). It is the single rule used by the reminder (`remind_subagent_to_hand_back`, `mj-controller/src/server_runtime/api.rs` near line 2018), parent notifications (`child_progress`, `subagent_status`, `reconcile_children` in the same file), `has_handed_back` (`mj-core/src/subagent.rs` near line 1770), `list_agents` and the session wait (`mj-controller/src/server/api/wait_policy.rs` near line 244, `mj-controller/src/server/api/wait.rs` `reminder_grace_end`). Today the reminder turn itself always yields `Fallback` (the `is_handback_reminder(&turn.command_id)` check).

The delegation loop (`mj-controller/src/daemon/delegation.rs`) observes every managed session through `DelegationObservation` (`mj-controller/src/session_manager/delegation.rs`), which carries the last outcome only while the session is connected and idle. For a child outcome it runs `complete_child` and `settle_child`: remind once per completed ordinal (durably marked in `SubagentRecord.noticed_turn`), then `reconcile_child_report` (writes the parent's mailbox report and the `reported_finish` marker in one transaction), then `park_subagent` (`mj-controller/src/daemon/subagent_park.rs`, `mj-controller/src/controller/subagent_park.rs` `stop_idle_subagent_worker`). `ChildCompletion::AwaitingReview`, used while a child waits for its own code review, is the closest existing precedent for a hold: no reminder, no notice, no park, retried later.

Generated prompts are prompts Mjolnir writes itself. Their command-ID prefixes are registered once in the `GeneratedPrompt` macro in `mj-core/src/continuation.rs` (the reminder is `handback-reminder-`). That list is mirrored by `scripts/jev-scenarios-extract.py` (pinned by the test `jev_extract_script_lists_the_same_generated_prompt_prefixes`) and by a client transcript test in `mj-client/src/transcript.rs`. `RelayCommand::HandbackReminder` carries no text: `prompt_blocks()` in `mj-core/src/relay/snapshot.rs` builds `HANDBACK_REMINDER_TEXT` from the variant, and the worker admits it only while the named completion is current and no newer input exists (`mj-worker/src/relay/commands.rs` near line 791).

The parent receives a child's report as a `SubagentFinished` mailbox event written to the daemon's `mailbox_outbox` table, delivered to the parent's worker, which hands it to the agent at the next tool boundary of a running turn or by a worker-created `MailboxWake` turn when idle. The parent's snapshot records `delivered_mailbox_event_keys: key -> journal ordinal` (`mj-core/src/relay/snapshot.rs` near line 1641); the event key is `subagent_report_key(parent, child, identity)` (`mj-controller/src/database/delegation.rs` near line 230). A parent message to a child is either a durable `subagent-message-<request>` outbox event (mailbox-capable workers) or a `subagent-input-<request>` prompt, via `deliver_subagent_input` (`mj-controller/src/server_runtime/api/subagent_input.rs`).

A detailed map with line references, written for this plan, is at `.mj/agents/64470bad562cf2be99ad956afeb27e24/MAP.md` in the coordinator's clone; everything this plan depends on is restated here.


## Plan of Work

### Milestone 1: the worker publishes work-process liveness (relay protocol 67)

At the end of this milestone a child's relay snapshot says how many of its last turn's work processes are still alive, and the count drops to zero within seconds of the last one exiting. Two relay commands also exist for the daemon to use in Milestone 3.

In `mj-core/src/relay/snapshot.rs`, add `work_liveness: Option<crate::work_facts::WorkLiveness>` to the relay snapshot (serde default, skipped when `None`). It is set when a managed child's prompt outcome is applied and that outcome's `work_facts.processes` is `Known` with a non-empty `identities` list: `completed_ordinal` is the outcome's ordinal, `alive` is `identities.len()`, `checked_at_ms` is the facts' `observed_at_ms`. It is cleared when the next prompt is accepted and when an outcome without live identities is applied. Add `RelayObservation::WorkLivenessChanged { liveness: WorkLiveness }`, applied only when its `completed_ordinal` equals that of the current `work_liveness` (a late observation for an older turn is dropped). Add `RELAY_WORK_LIVENESS_PROTOCOL = 67`, make `RELAY_PROTOCOL_VERSION` equal it, and give the new variants `minimum_protocol` arms of 67, following how earlier protocol additions are written in `mj-core/src/relay.rs`.

In `mj-worker`, add a watcher owned by the worker runtime. Whenever the snapshot holds a `work_liveness` with `alive > 0`, check which of that outcome's identities are still alive every 5 seconds, by running the existing process scan (`work_facts::scan_processes` or the code `capture` uses) and intersecting by pid and start identity. Publish `WorkLivenessChanged` only when the count changes. Stop when it reaches zero or when `work_liveness` is cleared or replaced. After a worker restart, the watcher resumes from the snapshot's `work_liveness`. The 5-second interval is the latency with which the daemon learns of an exit, not a limit; say so in a comment. Scanning is done off the relay's event loop (on a blocking task, as `capture` already is), and a scan failure leaves the last published count unchanged and is logged. Platforms where `capture` reports processes as unavailable never set `work_liveness`. Fix the stale module comment in `mj-worker/src/work_facts.rs` about `RelayRequest::WorkFacts`, and the `identities` doc comment in `mj-core/src/work_facts.rs` ("for the park's later liveness check") to name this watcher.

Also in this milestone, because the relay files are this milestone's to edit, add the relay side of the two follow-up prompts. Add `RelayCommand::HoldFollowUp { completed_command_id: String, completed_ordinal: u64, kind: HoldFollowUpKind, running: Vec<String> }`, minimum protocol 67, using `mj_core::subagent::HoldFollowUpKind`, which is already on master. The worker admits it under exactly the guard `HandbackReminder` uses (the named completion is current, no newer input, `can_submit`), and every place that matches `HandbackReminder` (`mj-core/src/relay/snapshot/apply.rs`, `mj-worker/src/relay.rs`, `mj-worker/src/worker_runtime/unix/dispatch.rs`, and others the compiler finds) handles it the same way. Add `#[serde(default, skip_serializing_if = "Vec::is_empty")] running: Vec<String>` to `RelayCommand::HandbackReminder`; first verify that a protocol-66 worker decoding a `HandbackReminder` with an unknown field fails or succeeds, and record the result in Surprises & Discoveries (the daemon will only fill the field for protocol-67 workers either way). In `prompt_blocks()`, build the texts:

    HandbackReminder, running empty: HANDBACK_REMINDER_TEXT, unchanged.

    HandbackReminder, running non-empty:
    [handback reminder] Your turn ended without delivering a report, and commands you started are still running:
    - <one line per entry of running>
    If your assigned task is finished, call the mj-agents handback tool now with your full report, then stop. If you are waiting for these commands to finish, you may end your turn: Mjolnir will prompt you when they have all exited, or after 30 minutes. Otherwise continue working, and hand back when the task is done.

    HoldFollowUp ProcessesExited:
    [background work finished] The commands you were waiting for have exited. Check their results, continue your assigned task, and call the mj-agents handback tool when it is done.

    HoldFollowUp GiveUp:
    [background wait ended] You have waited 30 minutes and these commands are still running:
    - <one line per entry of running>
    Call the mj-agents handback tool now with what you have: what finished, what is still running, and what your parent needs to decide. Your parent can tell you to keep waiting; Mjolnir leaves these commands running until your parent's next turn ends.

Render the 30-minute figure in both texts from the constant `mj_core::subagent::HOLD_GIVE_UP_MS`, which is already on master.

### Milestone 2: the report rule learns the hold

At the end of this milestone `report_state` returns a new state, `Held`, for a child whose reminder turn ended without a handback while its work processes are alive, and the daemon's reminder carries the process list.

In `mj-core/src/continuation.rs`, register two `GeneratedPrompt` variants with prefixes `hold-wake-` and `hold-give-up-`, update `scripts/jev-scenarios-extract.py` and the client transcript test to match. In `mj-core/src/subagent.rs` add `hold_follow_up_command_id(kind, reminder_completed_ordinal) -> String` and a reader that returns the kind and ordinal from a command ID, and make `MaterializedTurnOutcome::answered_ordinal` (`mj-core/src/state.rs`) treat a follow-up's ordinal as it treats a reminder's, so a follow-up turn answers the parent's prompt.

Extend `report_state` with a `liveness: Option<&WorkLiveness>` argument and add:

    ReportState::Held { since_ms: i64, give_up_at_ms: i64, waiting_on: Vec<String>, follow_up: Option<HoldFollowUpKind> }

The rules, in order, for a child with the handback tool whose last turn finished normally and has no handback answering it:

1. The last turn is a reminder turn (`is_handback_reminder`). If `liveness` names this turn's `completed_ordinal`: if `alive > 0` and `now_ms < completed_at_ms + HOLD_GIVE_UP_MS`, return `Held` with `follow_up: None`; if `alive > 0` and the deadline has passed, return `Held` with `follow_up: Some(GiveUp)`; if `alive == 0`, return `Held` with `follow_up: Some(ProcessesExited)`. If `liveness` is absent or names another turn, return `Fallback` as today. `waiting_on` is the rendered process lines of that turn's work facts (`render_process` of `shown`, plus "and N more" when `omitted > 0`).
2. The last turn is a `hold-give-up-` follow-up: return `Fallback`.
3. The last turn is a `hold-wake-` follow-up: apply the ordinary rules for a task turn (a reminder is owed), so the child can be reminded and held again with a fresh 30 minutes.
4. While a follow-up for the current reminder turn is in flight (its deterministic command ID is in `in_flight`), return `Pending { remind: false }`.

Every match on `ReportState` must handle `Held` (the map lists `mj-core/src/subagent.rs` `has_handed_back`; `mj-controller/src/server_runtime/api.rs` around `list_agents`, `child_progress`, `subagent_status`, `report_identity`, `remind_subagent_to_hand_back`; `mj-controller/src/server/api/wait_policy.rs`). `Held` is not finished: `subagent_status` reports `running`, `report_identity` is `None` so no reconcile path enqueues a report, and `has_handed_back` is false.

In `remind_subagent_to_hand_back`, fill `HandbackReminder.running` from the finished turn's work facts (the same rendered lines) when the child's worker is at protocol 67 or later and the facts list running processes.

### Milestone 3: the daemon holds, wakes, gives up, and delays parks

At the end of this milestone the delegation loop acts on `Held`, `list_agents` shows what a held child waits on, the parent's session wait wakes at the give-up time, and parks of children with live processes wait for one parent turn.

Add `work_liveness` to `DelegationObservation`, read from the child's snapshot, so the loop re-evaluates a child when its liveness changes. In `settle_child`, when `report_state` returns `Held`: with `follow_up: None`, settle as `ChildCompletion::Held { give_up_at_ms }` (no reminder, no report, no park) and arrange to re-evaluate at `give_up_at_ms` or on the next observation change; with `follow_up: Some(kind)`, submit `RelayCommand::HoldFollowUp` with the deterministic command ID, the reminder turn's command ID and ordinal, and `running` (the same lines; empty for `ProcessesExited`). A refusal because the completion is no longer current means the child has moved on; settle. A `DeliveryUnconfirmed` error is retried with the same command ID, as the reminder does.

In `list_agents`, give a held child `state: "running"` and a `waiting_on` entry with the process lines and the give-up time. In the session wait (`wait.rs`), a held child's next wake time is `give_up_at_ms`, alongside `reminder_grace_end`.

The park gate: after `reconcile_child_report` returns `Told`, if the child's `work_liveness` names its last turn and `alive > 0`, do not park yet. Read the parent's snapshot: find the delivery ordinal `D` of the child's report event in `delivered_mailbox_event_keys` (the key is `subagent_report_key(parent, child, identity)`). Park only when the report has been delivered and the parent's last turn outcome has `completed_ordinal > D`, or when the parent is no longer `Running`. Until then settle as `ChildCompletion::ParkAfterParentTurn` and re-evaluate on observation changes of the parent or the child. Inside the park's lifecycle operation (`stop_idle_subagent_worker` or its daemon caller), before reserving the worker, return `ParkOutcome::Busy` when the child has undelivered rows in `mailbox_outbox`. If the child receives input, its new turn supersedes the old outcome and the park is abandoned by the existing ordinal checks.

### Milestone 4: end-to-end check

Build the branch, start an isolated instance (`--instance child-hold`), and with a real harness login run the scenario in Purpose: a child told to start `sleep 90` in the background and end its turn. Observe the hold in `list_agents`, the absence of a parent report, the wake prompt after the process exits, and the handback. Then repeat with `sleep 100000` and a temporarily rebuilt binary whose `HOLD_GIVE_UP_MS` is 60 seconds, observe the give-up prompt, the fallback report, and that the park waits for the parent's next turn to end. Record transcripts in Artifacts and Notes. Do not commit the shortened constant.


## Concrete Steps

Work in detached worktrees, one per implementer, created from the coordinator's master:

    cd /home/jonathan/Projects/mjolnir/.mj/clones/5c0f8f9b7f6fec42a1c3da897a229a5f
    git worktree add --detach ~/Projects/mjolnir-wt/child-hold-relay HEAD
    git worktree add --detach ~/Projects/mjolnir-wt/child-hold-daemon HEAD

The relay implementer owns Milestone 1 and every edit to `mj-core/src/relay.rs`, `mj-core/src/relay/**` and `mj-worker/**`. The daemon implementer owns Milestones 2 and 3: `mj-core/src/subagent.rs`, `mj-core/src/continuation.rs`, `mj-core/src/state.rs`, `scripts/jev-scenarios-extract.py`, `mj-client/src/transcript.rs` and `mj-controller/**`. The daemon implementer starts with the parts that do not need the new relay types (the `Held` rule, prompt IDs, `list_agents`, the wait, the park gate's parent-snapshot logic) and merges the relay implementer's commit (`git merge <sha>` in its worktree) once it exists. Each commits on its detached HEAD and does not push.

Focused test commands while working (from the worktree root, outside the sandbox):

    cargo test -p brokk-mj-core subagent
    cargo test -p brokk-mj-core relay
    cargo test -p brokk-mj-worker work_facts
    cargo test -p brokk-mj-worker relay
    cargo test -p brokk-mj-controller delegation
    cargo test -p brokk-mj-controller subagent

Final validation, once, after integration on master: `cargo clippy --all-targets -- -D warnings` for the workspace and `cargo test` for `brokk-mj-core`, `brokk-mj-worker`, `brokk-mj-controller` and `brokk-mj-client`.


## Validation and Acceptance

Tests to add, following the repository's "Which tests to add" rules (each is a tricky condition or an integration seam, not a happy-path unit example):

In `mj-worker`, a test that starts real processes (for example `sleep`), records them as work facts of a finished turn, and observes `work_liveness.alive` fall as they are killed, including a pid-reuse case where a new process with the same pid but a different start identity does not count as alive. In `mj-core` relay tests, a decode test that a protocol-67 snapshot with `work_liveness` and a journal with `HoldFollowUp` replay correctly, and that a `WorkLivenessChanged` for an older ordinal is dropped.

In `mj-core/src/subagent.rs` tests, transition tests for `report_state`: reminder turn with live processes is `Held`; the same after the deadline is `Held` with `GiveUp`; with zero alive is `Held` with `ProcessesExited`; a give-up turn without handback is `Fallback`; a wake turn without handback owes a reminder; liveness for another ordinal or absent gives today's `Fallback`. Update `a_child_gets_one_reminder_and_then_reports_its_last_message` only as far as the new argument requires; it must still pin today's behaviour when no liveness is given.

In `mj-controller/src/daemon/delegation.rs` tests, modeled on `a_child_with_an_open_review_is_not_reminded_noticed_or_parked`: a held child is not reported or parked; it gets exactly one `hold-wake-` command when liveness reaches zero; after the deadline it gets exactly one `hold-give-up-`; a restarted loop (fresh in-memory state, same store and snapshots) does not resend either. Park gate tests: a told child with live processes is not parked until the parent's turn after delivery ends; a parent message enqueued before that turn ends prevents the park; a child with no live processes parks as today; a parent that leaves `Running` opens the gate.

Acceptance is the Milestone 4 scenario: the parent receives no "Waiting for …" report, the child is woken and hands back, and in the give-up variant the long process survives until the parent's next turn ends.


## Idempotence and Recovery

Every new decision is recomputed from durable state, so re-running the delegation loop or restarting the daemon repeats no action: follow-up command IDs are deterministic and the worker refuses one whose completion is no longer current. No schema migration is added. The worktrees can be removed with `git worktree remove` after their commits are merged. If the relay protocol number 67 is taken on master before this lands, take the next free number at merge time and update `RELAY_WORK_LIVENESS_PROTOCOL`.


## Artifacts and Notes

Evidence for the decisions, from the coordinator's clone: `.mj/agents/a10fbb4c7bf415aef31784376a56eb80/` (classifier corpus and outputs, coverage and duration measurements) and `.mj/agents/64470bad562cf2be99ad956afeb27e24/MAP.md` (code map).


## Interfaces and Dependencies

Already on master:

    // mj-core/src/work_facts.rs
    pub struct WorkLiveness {
        pub completed_ordinal: u64,
        pub alive: u32,
        pub checked_at_ms: u64,
    }

    // mj-core/src/subagent.rs
    pub const HOLD_GIVE_UP_MS: i64 = 30 * 60 * 1000;
    pub enum HoldFollowUpKind { ProcessesExited, GiveUp }   // serde snake_case

Milestone 1 adds, in `mj-core/src/relay/snapshot.rs` and `mj-core/src/relay.rs`:

    RelaySnapshot.work_liveness: Option<WorkLiveness>
    RelayObservation::WorkLivenessChanged { liveness: WorkLiveness }
    RelayCommand::HoldFollowUp { completed_command_id: String, completed_ordinal: u64, kind: HoldFollowUpKind, running: Vec<String> }
    RelayCommand::HandbackReminder { completed_command_id, completed_ordinal, running: Vec<String> }  // running is serde default
    pub const RELAY_WORK_LIVENESS_PROTOCOL: u32 = 67;

Milestone 2 adds, in `mj-core/src/subagent.rs`:

    pub fn hold_follow_up_command_id(kind: HoldFollowUpKind, reminder_completed_ordinal: u64) -> String;
    ReportState::Held { since_ms: i64, give_up_at_ms: i64, waiting_on: Vec<String>, follow_up: Option<HoldFollowUpKind> }
    pub fn report_state(handback_tool: bool, report: &SubagentReport, last_turn: Option<&MaterializedTurnOutcome>, liveness: Option<&WorkLiveness>, in_flight: &[&str], now_ms: i64) -> ReportState;
