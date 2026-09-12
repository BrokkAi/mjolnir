# Preserve goal work across worker restarts

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

An active Codex goal must not be killed by an automatic worker upgrade merely because its initial ACP prompt finished. Automatic recovery must observe and continue the existing goal without resetting its accounting or duplicating an already-running native continuation. Explicit Restart or Resume must pause an active goal before native opening and ask through the existing Mjolnir elicitation flow whether to resume it.

## Progress

- [x] Investigated the reported bifrost5 session and agreed on recovery behavior.
- [x] Fix and validate session-owned adapter observation and goal opening policy.
- [x] Persist goal/execution facts and enforce replacement/checkpoint safety.
- [x] Implement automatic recovery and explicit restart/resume question.
- [x] Validate UI/API activity and process-restart recovery.
- [x] Preserve session-scoped native goal state through checkpoint restore into a fresh home.
- [x] Release adapter, synchronize pins, validate and commit Mjolnir.
- [x] Merge concurrent upstream ownership refactor and validate the resolved workspace.

## Surprises & Discoveries

The extra fresh-home checkpoint probe failed: Codex rollouts do not restore the authoritative goal. Codex 0.153.4 stores goals, accounting, and continuation deferrals in goals_1.sqlite. Mjolnir previously archived only Codex rollout roots. Add a versioned JSON native artifact containing only the selected thread's goal row and deferral, and restore it transactionally into the target native database using the two supported Codex migration schemas/checksums. Preserve goal_id, all millisecond timestamps, budget, and counters. Keep other sessions untouched, capture a cleared-goal tombstone when the source database exists, and reject unknown schemas. The worker checkpoint layer owns bundled rusqlite; the shared core archive layer accepts a native-state hook without depending on SQLite.

The daemon log for session a460eb82ca61a947c5e634abf72dab06 records an automatic worker upgrade at 2026-09-12T20:27:11Z. Its native turn was interrupted at 20:27:07, but Codex autonomously started another turn at 20:27:10. The relay recorded no subsequent output until the user sent another goal prompt at 20:34. CodexAcpServer installs its event and interaction handlers only in prompt(), leaving a newly resumed native goal unobserved. Mjolnir persists neither goal state nor Codex autonomous execution, and only enables HarnessTurnPolicy for Claude.

## Decision Log

User chose automatic continuation for recovery restarts only. Explicit Restart or Resume asks using the existing model-replacement exclamation/question flow. Decline/cancel keeps the goal paused. Already paused, blocked, limited, completed and cleared goals do not automatically resume. Use existing native goal resume rather than setting its objective again. Active goal state blocks routine replacement even between turns. Unknown older Codex state fails closed for automatic replacement. An already-running native continuation is observed, never duplicated.

## Outcomes & Retrospective

Implementation is in place. Adapter unit tests and packaged guardian/yolo probes pass. A real fresh-process goal probe confirmed automatic native output without an ACP prompt, pause before explicit resume, no duplicate continuation, and preserved identity/budget/accounting. Adapter 1.11.3 was committed as 6bb3b84 and published by successful workflow 34720127991. The npm lock and GitHub artifact match the live-tested SHA-256 96c588e5f0ac48b000266866eeb242673b28e1e01eef0478fbdc7aa1b5b14aaf. The initial full merged Rust regression suite and Clippy passed. The fresh-home probe passed through the actual Rust checkpoint collector/restorer and packaged adapter. Focused native checkpoint tests cover accounting, goal identity, deferrals, session isolation, cleared goals, and rejection of unknown schemas. The expanded full regression run passed with zero failures across 22 test groups. The final focused goal run passed 10 tests; its one credential-dependent live hook was separately exercised by the fresh-home probe. Strict Clippy, rustfmt, diff checks, and documentation checks passed. The latest origin/master change (f23c1196) was merged before final validation. Mjolnir implementation was committed as 73dfe169. Push encountered concurrent upstream commits; merging origin/master at 90e87eb3 and revalidating before the final merge commit and push.

## Context and Orientation

The adapter repository is /home/jonathan/Projects/codex-acp. CodexAcpServer.ts owns ACP sessions and prompt handlers; CodexAcpClient.ts subscribes native events; CodexEventHandler.ts maps native events into ACP updates. GoalExtension.ts defines existing neutral goal metadata and control. Its AGENTS.md and docs/RELEASES.md require typecheck, tests, build, packaged live guardian/yolo tests, and BrokkAi-only publication.

Mjolnir mj-worker/src/acp.rs translates ACP messages into runtime events and owns its model-replacement elicitation. mj-worker/src/relay.rs and mj-core/src/relay/snapshot.rs journal and project operational state. mj-worker/src/worker_runtime/unix.rs dispatches worker commands. mj-controller/src/controller/worker_restart.rs upgrades/restarts workers, and checkpoint.rs owns barriers. Explicit session resume comes through mj-controller/src/controller/resume.rs. mj-core/src/worker_launch.rs holds persistent launch configuration. mj-client/src/usage_format.rs supplies shared UI/API activity facts.

## Plan of Work

First make adapter observation session-owned, including events and permission/elicitation lifetimes, retaining per-prompt cancellation/accounting. Observe native events before a resume can start work. Add authoritative execution metadata with turn identity and initialized goal state to session-open snapshots and ongoing updates. Separate history replay from live events. Add an advertised opening policy to pause an active stored goal before thread resume, returning enough information to ask about that goal; preserve native budget/usage. Default adapter behavior preserves native goal behavior.

Then parse shared goal/execution facts once in Mjolnir and persist goal snapshot plus synchronization support in the relay. Model native Codex turns without tying them to the initial ACP prompt. Active goals and unknown goal tracking block automatic replacement and unsafe routine checkpoint operations. Use shared state in activity displays and change restart readiness to allow a resumed active session.

Carry explicit opening intent to the worker. On automatic recovery, reconcile current goal and execution before resuming a quiescent active goal. On explicit Restart/Resume, pause before native open and emit a durable, deduplicated Resume this goal question. Resume only a matching goal on affirmative answer, retaining paused state on decline/cancel. Pending model recovery is resolved first. Closed/checkpoint-only sessions never start work. Recovery errors remain visible and bounded; stale answers cannot act on replaced goals.

## Concrete Steps

Work on current branches without branching/rebase/PR. Keep unrelated adapter tarball untouched. Run adapter typecheck, tests and build, then npm pack and live guardian/yolo checks per .claude/skills/run-codex/SKILL.md. Run focused Rust tests during milestones and final cargo test -q -- --test-threads=1 and cargo clippy --all-targets -- -D warnings outside sandbox. Keep build logs in target. Run cargo fmt --all --check and git diff --check. Release a tested adapter patch through its standard workflow and synchronize all exact Mjolnir adapter pins. Commit coherent validated changes.

## Validation and Acceptance

Tests must show an autonomous Codex turn remains running after its ACP prompt finishes; gaps between goal turns never admit upgrades; an old unsynchronized worker cannot be killed automatically; and checkpoints defer rather than restart active goal work. A fresh ACP session must forward goal output and interactions without any initial prompt. Recovery must distinguish a native continuation from a quiescent goal and preserve budgets/usage. Explicit opening must perform no goal work before a response, with affirmative, decline, cancellation, stale-answer, repeated restart, and multiple-client coverage. Paused/blocked/limited/completed/cleared goals remain inactive. Test replay/live ordering and pipe streams greater than 64 KiB.

## Idempotence and Recovery

Explicit answers are journaled before native control through a supervised blocking persistence callback. The durable decision carries the goal identity and action, so a crash before or after acknowledgement is reconciled against the existing goal. A newer explicit opening supersedes an older decision. Paused checkpoint-only recovery never opens the harness. Execution snapshots carry a monotonic adapter-session revision so earlier startup reads cannot overwrite later native transitions.

A later pause or a replaced/completed/cleared goal invalidates a saved resume decision. Native database opens refuse symlinks.

Use durable goal identity and recovery decisions. Reconnect is not a new resume request. An uncertain control acknowledgement requires state reconciliation, not blind resubmission. Pause intent must survive startup failure and cancellation. Session closure terminates supervised process groups before file removal. Tests use disposable goals/workspaces, never the reported live session.

## Artifacts and Notes

Evidence was read from the relay journal, native rollout and retained daemon log. Do not embed credentials or whole live session transcripts in this plan. No implementation mutations were made during planning. Live test evidence is retained in target/goal-adapter-live.log and target/goal-native-recovery-live.log; disposable native goals were cleared.

## Interfaces and Dependencies

Extend existing ACP goal metadata/capability and neutral execution metadata; maintain compatibility for clients that omit the new opening policy. Add optional serializable Mjolnir goal state and live synchronization facts with fail-closed replacement semantics for old Codex workers. Reuse ElicitationRequest/ElicitationResponse and the existing shared activity machinery rather than new UI dialogs. No new crates are required.

## Upstream ownership merge

The concurrent upstream refactor moved pure contracts to mj-core, ACP/relay execution to mj-worker, controller operations to mj-controller, and activity formatting to mj-client. Goal state now lives in mj-core/src/goal.rs; recovery lives in mj-worker/src/acp/goal.rs. SQLite goal checkpoint handling lives in mj-worker/src/checkpoint/codex_goal.rs, injected into generic core archive operations through NativeCheckpointState. Core retains no SQLite dependency. Two source files omitted upstream because target*/ also ignored targets directories were recovered from the refactor workspace, with narrow ignore exceptions. Crate boundary, package asset, standalone core/worker builds, rustfmt, and strict all-target Clippy checks passed. The full Rust run passed core/client/controller/chat/TUI suites; one worker assertion retained its old expectation in a binary already compiling when corrected. The rebuilt worker and CLI suites passed completely, and the remaining documentation tests passed. The new worker checkpoint entrypoint test verifies actual archive export and restore preserve native goal accounting. The packaged adapter fresh-home probe passed again using the worker-owned collector/restorer; disposable goals were cleared. Final evidence is in target/goal-merge-tests.log, target/goal-merge-remaining-tests.log, target/goal-merge-doc-tests.log, target/goal-merge-clippy.log, and target/goal-merge-native-checkpoint-live.log. The resolved merge is ready for its required commit and push to origin/master.
