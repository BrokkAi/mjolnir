# Detect Kimi background agents before worker replacement

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept current while the work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir currently mistakes a Kimi session for idle after the parent turn ends even when Kimi left a detached Agent task running. An automatic worker upgrade can then replace the worker, kill the child agent, and truthfully print `[session restarted]`. After this change, Kimi's detached Agent tasks appear through the existing background-work projection and keep replacement, checkpoint preparation, and session moves from treating the session as quiet until Kimi records a terminal lifecycle event.

## Progress

- [x] (2026-09-09 15:00Z) Reproduced the replacement and identified the missing Kimi child-liveness fact.
- [x] (2026-09-09 15:20Z) Verified Kimi 0.41.0's durable `task.started` and `task.terminated` records and the ACP adapter's immediate completed launcher card.
- [x] (2026-09-09 16:05Z) Implemented and tested the native Kimi task follower, including partial appends, journal replacement, legacy events, and path validation.
- [x] (2026-09-09 16:20Z) Integrated provisional ACP launch evidence and authoritative Kimi task levels into the relay.
- [x] (2026-09-09 16:30Z) Gated automatic upgrade and both move preparation checks on Kimi tracking certainty.
- [x] (2026-09-09 16:55Z) Passed `cargo test` and `cargo clippy --all-targets -- -D warnings`; updated this plan for the completed design.
- [x] (2026-09-09 17:00Z) Reviewed the final diff and prepared the validated change for commit and push.

## Surprises & Discoveries

- Observation: Kimi's ACP Agent card completes when the detached child is launched, rather than when the child exits.
  Evidence: the affected session contains a completed `Launching background coder agent` card while the native main wire contains a later-unmatched `task.started` record.
- Observation: Kimi's native event stream has the exact stable correlation data Mjolnir needs.
  Evidence: `task.started.info` contains `taskId`, `description`, `startedAt`, `kind`, `detached`, and `parentToolCallId`; `task.terminated.info` repeats the task ID and terminal state.
- Observation: the native session's top-level `agentId` identifies the emitting main agent, while `info.agentId` identifies the detached child.
  Evidence: the incident journal uses `agentId: "main"` beside an `info.agentId` such as `agent-1`; filtering both as emitters would discard every real child launch.

## Decision Log

- Decision: Detect Kimi background agents instead of disabling Kimi live upgrades.
  Rationale: a blanket policy avoids the crash but permanently removes useful upgrades and does not make other replacement paths understand the live work.
  Date/Author: 2026-09-09 / Codex
- Decision: Use Kimi's versioned native v2 wire plus provisional ACP launcher evidence.
  Rationale: Kimi ACP does not publish child lifecycle, while the native wire has durable start/termination records. The ACP launcher evidence closes the short interval before the durable append becomes readable.
  Date/Author: 2026-09-09 / Codex
- Decision: Fail closed whenever Kimi tracking is unavailable or an older worker cannot report tracking certainty.
  Rationale: uncertainty must not be interpreted as proof that replacing the worker is harmless.
  Date/Author: 2026-09-09 / Codex
- Decision: Keep the Kimi monitor in the worker relay coordinator instead of adding Kimi-specific variants to the shared ACP runtime protocol.
  Rationale: the native journal and configured Kimi home exist only on the target worker. Reading them through Tokio's blocking pool lets the coordinator publish live state directly without expanding `LaunchSpec` or `RuntimeEvent` with provider-specific file details.
  Date/Author: 2026-09-09 / Codex

## Outcomes & Retrospective

Kimi workers now replay and follow the native main-agent journal, retain positive ACP launch evidence until the journal correlates it, and expose whether that provider-owned task level is synchronized. A running detached Agent remains visible as background work after its parent prompt finishes. Automatic upgrades and moves defer while such work exists or while its state is unknown; termination makes an otherwise-idle current worker replaceable again. Older Kimi workers omit the certainty field and therefore fail closed for replacement, while older workers for other harnesses keep their previous behavior.

The focused tests cover native modern and legacy lifecycle records, path confinement, partial writes, replacement/truncation, ACP-to-native reconciliation, unavailable-state behavior, old-worker compatibility, and the target worker integration boundary. The full default-member test suite and clippy with warnings denied pass.

## Context and Orientation

`src/hel_acp.rs` owns the ACP bridge process and emits runtime events in protocol order. `src/hel_worker.rs` folds those events into a durable relay and derives `background_commands`, while `src/hel_worker/snapshot.rs` exposes the live `RelayOperationalState` used to decide whether replacement is safe. `mj-worker/src/hel_worker_runtime/unix.rs` connects ACP runtime events to the relay. The daemon, upgrade controller, and move controller consult that operational state before replacing or relocating a worker.

Kimi stores a native session index at `<KIMI_CODE_HOME>/session_index.jsonl`. Its matching row identifies a session directory whose main-agent journal is `agents/main/wire.jsonl`. A detached agent begins with `task.started` and ends with `task.terminated`; older compatible journals spell these `background.task.started` and `background.task.terminated`.

## Plan of Work

Add a small Kimi module below `src/hel_acp/` that safely resolves the native session path, validates that it stays under the configured Kimi home, replays the main wire to reconstruct live detached Agent tasks, and follows appends without losing partial lines. It must detect replacement or truncation and request a full replay. Malformed complete records are errors so the caller can retain the last known busy state.

The worker relay coordinator owns one monitor for each Kimi ACP connection. It starts unsynchronized, publishes an authoritative replacement level after a successful scan, polls every 500 ms, and performs a catch-up before `PromptFinished`. File work runs through Tokio's blocking pool. A read, parse, or path error publishes unavailable state and one warning, preserves prior tasks, and retries with bounded backoff.

Teach the relay a `KimiTasks` background-work policy. Kimi Agent tool calls that explicitly start in the background, or whose result says `task_id` and `status: running`, become provisional background work immediately. Native `parentToolCallId` replaces the provisional entry with the durable task ID. A terminal native record clears the task. Unmatched positive evidence has no timeout; it remains busy until Kimi state or process teardown proves it ended.

Add an optional live `background_work_known` field to `RelayOperationalState`. New Kimi workers initialize it false and set it true only after watcher synchronization. `is_quiet` rejects an explicit false. Add a harness-aware replacement predicate that also rejects `None` for Kimi, then use it in automatic upgrade and move preparation/revalidation. This makes an older Kimi worker require one explicit stop/resume to acquire the new tracker without changing compatibility for other harnesses.

## Concrete Steps

Work from `/home/jonathan/Projects/hel2`. Implement the watcher and its unit tests, then wire its runtime events into the relay and add controller behavior tests. Format with `cargo fmt`. Run focused tests while iterating, followed by `cargo test` and `cargo clippy --all-targets -- -D warnings` outside the restricted sandbox. Review the final diff, commit only the changed files on the current branch, incorporate the upstream commit without rebasing if necessary, and push the current branch to its configured upstream.

## Validation and Acceptance

A scripted Kimi fixture must emit a background Agent launch and end its parent prompt. The resulting relay must have no active prompt, one background command, and `safe_to_replace(HarnessKind::Kimi) == false`. Appending the matching terminal event must remove the background command and make a synchronized otherwise-idle relay safe to replace.

Tests must also prove that an ACP launcher hint protects the pre-flush interval, tracker failure retains prior work and marks the state unknown, old Kimi snapshots do not auto-upgrade, known-empty current Kimi snapshots do, and non-Kimi behavior is unchanged. Full tests and clippy must pass before the change is committed and pushed.

## Idempotence and Recovery

The watcher only reads Kimi state and publishes replacement levels, so retries are safe. Duplicate start and terminal records are idempotent. A replaced or shortened wire causes a validated full replay. If tracking cannot recover, the session remains usable but cannot be considered safe for automatic replacement; an explicit stop/resume remains the recovery path.

## Artifacts and Notes

The affected native start record had this essential shape:

    {"type":"task.started","agentId":"main","info":{"taskId":"agent-f7ob7bgk","description":"Fix #3142 Scala query memory","status":"running","detached":true,"startedAt":1788959525801,"endedAt":null,"kind":"agent","agentId":"agent-1","parentToolCallId":"tool_ef77..."}}

No matching terminal record existed before Mjolnir replaced the worker.

## Interfaces and Dependencies

`hel_acp` exposes the Kimi journal resolver and incremental follower to the target worker. `hel_worker::BackgroundWorkPolicy` gains `KimiTasks`. `hel_worker::RelayOperationalState` gains `background_work_known: Option<bool>` and the harness-aware `safe_to_replace` method. The worker coordinator derives the configured Kimi home from its existing credential endpoint and publishes task levels directly to the relay. No shared ACP event variant, launch-spec field, third-party dependency, database migration, configuration key, or UI command is required.
