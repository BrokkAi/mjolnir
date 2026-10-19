# Let delegated parents wait for child notifications without model polling

This ExecPlan follows `.agents/PLANS.md` and is a living document. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture

A delegated Codex parent that has exhausted independent work should end its current turn while its children keep running. Mjolnir should resume it when a child report becomes available or the user steers it. The parent then collects reports through the existing delegation tools. Waiting must not cause repeated empty model requests, automatic continuation, suspension of children, lost reports, or duplicate completion prompts.

## Progress

- [x] (2026-10-16) Read and claim issue #1302; inspect delegation instructions, report notifications, and continuation entry points.
- [x] (2026-10-16) Establish relay-owned durable wait intent and protocol-39 admission; new prompts and report collection clear it.
- [x] (2026-10-16) Implement notification-driven instructions and lifecycle behavior, preserving bounded blocking waits; compilation and runtime checks are in progress.
- [x] (2026-10-16) Add and execute regressions for continuation admission, report delivery, notification coalescing, steering, hooks, restart and migration compatibility.
- [x] (2026-10-16) Validate a real Codex parent with two delayed children, steering and daemon restart in an isolated instance; retain evidence in `.agents/docs/1302-parent-wait-validation.md`. The final repeat with parent c77b95c94e4d31e669f43a397423e430 passed, including verified cleanup.
- [x] (2026-10-16) Run full affected crate suites, targeted failure reruns, workspace Clippy, formatting and static checks.
- [x] (2026-10-16) Prepare validated changes for delivery on hel4 to its configured upstream origin/master.

## Surprises & Discoveries

The issue describes Codex instructions that poll a yielding wrapper. Commit `c66c5a664` already changed those instructions to direct tool calls, but still directs parents to renew timed-out waits. The remaining task includes notification-driven idle waiting and its supported lifecycle, rather than simply deleting the historical wrapper wording.

`mj-controller/src/server_runtime/api.rs::ensure_parent_wait_prompt` checks uncollected report identities and queued reminders under a per-parent lock. Its existing `finished_child_queues_one_wait_prompt_for_an_idle_parent` test explicitly allows another queued reminder while the first reminder is active. A later child finish must still be able to queue a notification during that active turn. Distinguish same-report duplicates from genuinely new finishes.

## Decision Log

- Decision: Preserve the existing report collection tools and bounded blocking wait behavior. Add only the waiting contract required to stop inference when there is no independent work.
  Rationale: An MCP timeout increase cannot stop a code-mode wrapper from requesting more inference; the daemon already has completion notifications.
  Date/Author: 2026-10-16 / Codex.
- Decision: Use isolated instance `issue-1302-waits` and isolated configuration/data directories for live validation, with the existing mbx Cargo setup unchanged.
  Rationale: The new worker and daemon must not replace the default instance or disturb live sessions.
  Date/Author: 2026-10-16 / Codex.
- Decision: Add quick MCP `await_children`, distinct from bounded report-collecting `wait`. The worker pre-arms relay-local `SetSubagentWait { active_prompt_id, waiting }` for a live originating turn and durably retires older collecting waits under the relay/queue locks. After their receipts settle, the daemon replays the same command identity and checks whether reports are ready or unfinished children remain. The relay rejects stale turns and queued input, journals the wait, and owns `ContinuationState.awaiting_children_since_ms`. All prompt admissions clear the marker, including generated report reminders; explicit collection clears it at worker admission. Protocol 39 gates the new relay command; the controller still supports older workers until normal replacement. The optional nested continuation field preserves old snapshot decoding, and schema revision 84 is a breaking compatibility boundary: persisted delegation requests can contain the new AwaitChildren enum and older writers discard the new continuation intent. The migration raises the read/write minimum in the same transaction; no existing migration is rewritten. New workers are not downgraded.
  Rationale: An explicit operation lets the model stop immediately without a blocking outer wrapper. Worker admission makes stale continuation and steering races impossible; assessment and activity readers use this same durable fact rather than interpreting final prose.
  Date/Author: 2026-10-16 / Codex.
- Decision: Derive each report-ready prompt's command identity from the sorted outstanding child finish identities, instead of a fresh random identifier. The existing relay command ledger is the single deduplication owner, while the per-parent lock continues coalescing queued reminders. Report collection returns all ready finishes together; a later finish produces a new batch identity and can queue during an active reminder.
  Rationale: Repeated reconciliation or daemon restart must not turn the same outstanding batch into multiple model turns. Collection remains a separate durable operation and report output never enters the notification.
  Date/Author: 2026-10-16 / Codex.

## Outcomes & Retrospective

The explicit worker wait, MCP operation, instruction updates, protocol gate, schema compatibility boundary, and regressions are implemented and validated. The final real Codex parent completed exactly four turns, stayed between turns while waiting, survived daemon restart, handled steering, and collected both child reports. Required Rust/static checks are complete, and the changes are ready for Git delivery.

## Context and Orientation

Mjolnir's daemon is the control process that owns session records and delegation. Workers run harness turns and retain their relay journal across daemon restarts. A parent starts children through the mj-agents MCP server in `mj-worker/src/subagent_mcp.rs`; its shared delegation guidance is also supplied from `mj-core/assets/subagent-delegation.md`. A report-ready notification is a normal parent prompt, `mj-core/src/continuation.rs::PARENT_WAIT_PROMPT_TEXT`, and does not contain the report itself. Calling `wait` durably collects each child finish once.

`mj-controller/src/server_runtime/api.rs` implements report snapshots, durable execution, collection, and notification reconciliation. `mj-controller/src/daemon/delegation.rs` observes child completions and reconciles parents after startup. `mj-controller/src/daemon/continuation.rs` decides automatic continuation from worker-owned continuation and assessment state. Worker relay command admission and event handling are under `mj-worker/src/relay/`. Existing tests live next to these modules. `tests/e2e/delegation.py` runs a real daemon and worker against a recorded harness, with temporary configuration and data.

## Plan of Work

First trace worker admission, durable continuation state, and report notification identities to determine the smallest explicit waiting lifecycle. Prefer an existing state owner over reconstructing child busy predicates in multiple consumers. The chosen contract must survive daemon restart, clear on new user or report input, and reject stale automatic continuation at the worker's serialized admission point. Record the chosen interface and compatibility consequences here before implementing it.

Next revise parent instructions so an idle parent can use that contract and end its turn without suspending its session. Explain that the notification contains no report, collection consumes each finish once, and user steering can resume independent work. Keep bounded blocking waits useful and never instruct polling an outer wrapper.

Then extend existing relay and daemon integration/race tests, including same-report deduplication and a later child completion arriving during an active notification turn. Mark defect regressions with `Hard-won: #1302`. Add or extend a real-harness validation script if needed rather than relying only on instruction string assertions.

Finally build the CLI and worker through normal Cargo/mbx, run the isolated real Codex experiment, inspect mj session transcripts and durable events (not harness-native logs), and capture request/turn counts, report identities, and steering behavior. Finish required validation and push the committed result.

## Concrete Steps

Work from `/home/jonathan/Projects/mjolnir4`. Read relevant source with `rg` and focused file reads. Use `cargo test -p <touched-crate> <affected-filter>` outside the restricted sandbox while iterating. After the final code changes, run the full suites of every touched crate and `cargo clippy --all-targets -- -D warnings` on the dev profile, also outside the sandbox. Run `cargo fmt --all -- --check`.

For the live experiment, read the mj skill's `SKILL.md`, its `references/configuration.md`, and the CLI's `--help` for exact commands. Every new binary invocation that can touch a daemon or store must select `--instance issue-1302-waits`; set isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`. Copy only required profile/authentication setup using established test procedures, never alter the user's live configuration. Start a real Codex parent, have it delegate a delayed child, and observe the parent's turns and reports through mj APIs. Also steer the waiting parent and verify children remain alive. Stop only the isolated test processes and preserve useful evidence.

## Validation and Acceptance

The parent uses no repeated empty-poll model requests after independent work ends. It remains a live session with children running, receives one notification per outstanding report generation, collects reports once, handles a second child completing during report collection, and resumes on user steering. Automatic continuation does not override intentional waiting or label it task completion. Bounded `wait` calls still return reports or their established timeout result. Restart and race tests protect whatever durable state and serialized admission interfaces change.

## Idempotence and Recovery

All live test state belongs to the isolated named instance and temporary directories. Never test a build against the default store. Retain durable report collection identities and retry accepted actions only using their existing command/request identities. A failed live run can be inspected and resumed or stopped within the isolated instance. Do not create branches or rebase; commit on `hel4`, push to its configured upstream, and merge upstream only if a push is rejected because it advanced.

## Artifacts and Notes

Issue: https://github.com/BrokkAi/mjolnir/issues/1302. Live evidence will be recorded under `.agents/docs/` with session IDs and concise observations. Do not expose bearer tokens or credentials.

## Interfaces and Dependencies

Add `SubagentToolAction::AwaitChildren` and expose MCP `await_children` only to parents. Add relay-local `RelayCommand::SetSubagentWait { active_prompt_id: String, waiting: bool }`, requiring `RELAY_SUBAGENT_WAIT_PROTOCOL = 39`, and optional `ContinuationState.awaiting_children_since_ms: Option<i64>`. `eligible()` refuses continuation while the marker exists. Completed-turn assessment chooses `Action::Wait` from the marker without another classification request, and activity exposes expected continuation rather than completed work. Keep existing report identities and all request/command retry guards. Do not fork Codex, add dependencies, or change Cargo target configuration.

Revision note: Initial plan records the issue, current partial instruction fix, notification deduplication concern, and required ownership investigation before implementation.

Revision note: Established the explicit MCP/relay waiting contract, optional snapshot compatibility, protocol gating, and canonical report-batch notification identities before implementing regression tests and live validation.

Revision note: Persisted AwaitChildren requests require breaking database revision 84 and adds a notification epoch column. Notification waiting also interrupts any outstanding bounded MCP waits under the existing relay/queue lock order, with negative delivery receipts preserving their reports. AwaitChildren uses the reserved control lane and must not serialize behind a blocking report collection wait.

Revision note: A receipt race requires arming the wait at worker tool admission, before daemon dispatch. This immediately withdraws any outstanding blocking wait. The daemon dispatches AwaitChildren after that wait's delivery receipt settles, so tentative report markers have either been confirmed or restored before it decides reports_ready versus awaiting_children. It replays the same relay command identity, and disarms the intent if reports are ready or nothing remains. This ordering replaces a potentially long lock wait with a worker-owned state transition and bounded receipt settlement. The current opt-in live scenario is `tests/e2e/parent_report_notifications.py`; it uses copied credentials, isolated configuration/data, two externally released children, user steering, a daemon restart, and parent turn-count assertions.

Revision note: Serialize report-collection commits and notification submission under the existing per-parent prompt lock. Derive notification IDs from report identities plus a database epoch incremented atomically only when a rejected delivery restores a consumed report. Ordinary reconciliation remains idempotent; rollback can reissue a withdrawn notice without losing a wake. Ordinary state upserts preserve the epoch. Extend the existing undelivered-result regression to cover repeated rollback and renewed notification admission. Explicit waiting also permits mailbox wakeups through the same relay-owned activity facts.

Revision note: The first executed relay regressions found a missing SetSubagentWait/NoticeRecorded completion pairing; the snapshot validator now accepts it. Running the built MCP tests independently found Claude's 2,048-character instruction limit; condensed the shared contract to 1,983 characters without dropping the new wait lifecycle. Delivered tool-hook mailbox input now clears prior wait intent on acknowledgement; extended the existing hook/prompt/wake integration case. Workspace `cargo clippy --all-targets -- -D warnings`, formatting, whitespace checking and Python syntax checking pass. Final crate tests and the live experiment remain pending.

Validation environment note: mbx wrappers have repeatedly waited in shared scheduler locks and ZFS truncate operations, with no compiler subprocess. Superseded focused jobs were stopped, leaving the final build and full touched-crate suites. Asked for optional approval to use per-command MBX_SCHED_DISABLE=1 with two build jobs, retaining caching and the existing target directory, because AGENTS.md requires checking before changing Cargo build behavior. No scheduler bypass was used. Builds and checks completed through the normal mbx setup.

Revision note: The real Codex experiment passed: parent aa8ceea643cc7ea51722928e8a52be33 remained between turns for 30 seconds, survived daemon restart, handled steering, and collected both externally released child reports in exactly four turns with no automatic continuations. Cleanup now explicitly waits for suspension; the first run's prematurely stopped operation was retried and its stopped state verified.

Revision note: Final review found that clearing the transient wait intent with steering could precede an older blocking collector's next poll. AwaitChildren admission now records those collectors' interrupted results and negative delivery decisions durably while holding the relay and queue locks. New input can clear the intent afterward without undoing their retirement. Extended the existing ordering test with await admission followed by input before the waiter runs. Final rebuild and lint are queued; the full suites are still compiling.

Revision note: The full controller run reached 1,986 passing tests, with 16 migration-fixture failures and four host-timeout failures. Historical fixtures reset their revision without dropping notification_epoch, causing duplicate-column errors on replay; their existing rewind helpers/SQL now remove it. Production migration 84 remains strict. All four unchanged checkpoint/discovery timeout failures passed serially. The final collector-admission/steering regression also passed on the rebuilt worker test executable.

Revision note: A second real parent completed initial waiting, daemon restart and steering, then issued a second bounded wait after collecting A (daemon wait calls were three seconds apart). The test withheld B until an explicit waiting turn, so it timed out; cleanup released B and both reports were collected without loss. Clarified that one wait collects every ready report and that an otherwise idle parent should await_children again with unfinished children. The shared Claude contract remains below its limit at 2,037 characters. The live driver now explicitly requests one collecting call per notification. Final migration checks, binaries and lint are rebuilding from this frozen source.

Final validation note: The frozen-source controller suite passed 2,006 tests (12 existing ignores), core passed 502, and worker passed 680 with 11 existing ignores plus one mailbox-fixture failure. The waiting fixture had left ACP readiness false, so the fail-closed wake guard correctly refused it. Added SessionConfigured to represent a ready parent and reran only that failure successfully. All remaining binary/doc tests and final workspace Clippy passed; formatting and static checks passed. The final live run under issue-1302-live-4 passed with parent c77b95c94e4d31e669f43a397423e430 and both child reports, exactly four turns, no automatic continuations, and verified suspension/daemon cleanup. Evidence is in .agents/docs/1302-parent-wait-validation.md.

Merge note: The first push of d5566de32 was rejected because origin/master advanced to 45125bb40. Merged upstream as required. Its installed protocol 38 introduces the incremental delegation feed and private acknowledged reply bodies. Preserve that protocol and assign the new child-wait command protocol 39. Resolve queue admission by checking upstream backpressure and mutation gates before changing wait intent, then retain durable interrupted replies through upstream's store_result path. Validate the combined queue/reply ownership and rerun the isolated real parent scenario before pushing the merge.

Final merge outcome: Preserved the upstream protocol-38 feed, assigned explicit waits protocol 39, and kept backpressure checks before relay intent mutations. The merged wait regressions passed across all five touched crates. The automatic daemon upgrade integration fixture also needed its notification_epoch column removed when rewinding to revision 43; its corrected targeted and full-suite runs passed. Final full suites passed: controller 2,007, core 502, worker 685, chat 412 and CLI 212 active unit tests, plus integration and doc-test targets. Workspace Clippy and static checks passed. The final real Codex parent e1b538b263d64cc2fb59fe1266c30d2e passed all four-turn, restart, steering, report and cleanup assertions under protocol 39. The resolved merge is validated for delivery.
