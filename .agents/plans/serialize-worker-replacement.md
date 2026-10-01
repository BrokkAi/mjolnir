# Serialize worker replacement per session


This living ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current.

## Purpose / Big Picture


A checkpoint and an automatic upgrade must never replace the same session's worker concurrently. Each session will have one controller owner for process mutation, and each worker will exclusively lock its durable root before touching diagnostics, sockets, or its journal. Other sessions continue independently. The damaged production session is outside scope and remains untouched.

## Progress


- [x] Investigated the incident and agreed prevention-only scope.
- [x] Implement exclusive worker root ownership and startup/log handling (cross-process and re-exec tests passed).
- [x] Introduce controller ownership permits and route process mutation and barriers through them.
- [x] Centralize durable replacement reconciliation and conditional admission, including cold starts and resumes.
- [x] Add deterministic concurrency, cancellation, process, and handoff regressions; the quiet-worker handoff regression passed.
- [ ] Run dev-profile tests, clippy, formatting, isolated upgrade checks, and commit.

## Surprises & Discoveries


The incident journal contains two different records for every ordinal 10946 through 10953. At 19:57:21 UTC checkpoint recovery restarted the worker, while an automatic upgrade reported replacement at 19:57:30. The resulting Codex active-writer failure was followed by journal validation failures. The existing target mutex covers upgrade's swap and actor recovery but not checkpoint restarts. Startup tests only a socket connection before opening the journal, which cannot exclude simultaneous boot. Launch shells clear shared diagnostics and truncate worker.log before the worker can establish ownership.

## Decision Log


The owner uses opaque reference-counted permits with async task-local and blocking thread-local scopes to borrow the same admission in nested lifecycle calls. Low-level stop/start take explicit permits and verify the selected root belongs to the permit's session. Blocking executor work retains a clone after its waiter disappears. The existing bare target mutex is removed. Full process mutation remains serialized for each operation; preparation uses separate uniquely named files and promotions happen under bounded swap admission.

Use one instance-and-session ownership service with opaque permits, acquired before relay leases. Background operations defer on contention; foreground operations wait cancellably. Existing recovery-copy admission continues to govern disposable background preparation, not process replacement. Reuse the existing durable restart table and phases, changing claims to conditional insertion and completion to owner-mediated reconciliation. Add a process-held file lock with a stable inode and preserve its descriptor across login re-exec. No schema or wire-format change is intended. These choices exclude overlap at both the controller decision point and the durable writer. The user chose prevention only on 2026-10-01.

## Outcomes & Retrospective


Implementation is complete; final workspace validation and commit/push remain. The dev-profile controller suite passed 2,129 tests, with 10 existing ignored tests, and workspace Clippy passed. A workspace run reached the worker suite but hit four ACP timeouts and a checkpoint-state assertion under heavy concurrent host builds. A rerun with eight test threads exposed a missing generated fake worker in the shared build cache; individual Move and resume reruns passed. No cache configuration or target layout was changed. The new quiet-worker regression initially lacked the journal required by checkpoint-only startup; the fixture now creates that journal before starting the relay and passed in the workspace build. The eight-thread workspace rerun passed the quiet-worker regression but reported 13 missing generated-fixture failures. Cargo output contains the stamped fake worker, but the reused test artifact embeds a removed stable mbx OUT_DIR path. A one-command `MBX_SHARE_OUT_DIR=0` validation override was proposed to the user because AGENTS.md requires checking before changing Cargo build behavior; no target layout or saved cache settings have been changed. Final workspace Clippy passed after the last Rust edit. The current worker test binary is being checked independently; its six root-ownership tests passed. The implementation checkpoint will be committed and upstream integrated before final normal-configuration validation. The temporary cache override has not been used or approved. Focused ownership tests passed (four controller admission tests and six worker root tests, including process exit without Rust cleanup). Controller-wide validation revealed debug stack growth from wrapping large futures. Admission now boxes the operation before constructing its future, which also keeps the new contention regression within the normal test-thread stack. Readiness observation now acquires ownership before reading a worker and checks for a pending intent before competing for ownership. This prevents stale snapshots from clearing a new boot and avoids delaying ordinary relay sync. Initial broad validation also identified fake placements that did not belong to their session and fixtures that bypassed lifecycle admission; those fixtures were corrected without relaxing process ownership.

## Context and Orientation


`mj-controller/src/controller/worker_restart.rs` performs upgrades and checkpoint restarts. `mj-controller/src/session_manager/recovery.rs` executes actor recovery plans. `mj-controller/src/recovery_gate.rs` formerly exposed the target mutex and now only schedules disposable recovery copies. `mj-controller/src/database/worker_restart.rs` stores prepared, swapping, and awaiting_readiness intents, formerly overwrote another intent and allowed actors to clear them independently. Conditional insertion and owner-mediated readiness observation replace that behavior. Creation, resume, Move, park, and destroy have additional mutation paths that must share the owner. Checkpoint/workspace leases must retain ownership for the lifetime of their barrier.

`mj-worker/src/main.rs` writes startup records before login-environment re-exec. `mj-worker/src/worker_runtime/unix.rs` opens the durable relay and serves control.sock. Ownership must precede all root writes and survive until socket cleanup and exit reporting finish. Existing std::fs file locking supports a stable worker.lock; Unix descriptor inheritance must be limited to re-exec, never ACP children. Shell launch plans in controller worker_binary/process.rs must use unique attempt logs and leave shared root cleanup to the owning worker.

## Plan of Work


First add worker root ownership and behavioral tests. Acquire at the binary entrypoint, keep across login bootstrap, and pass ownership into the daemon runtime; library tests acquire through the same helper. Losing contenders cannot change incumbent diagnostics or sockets. Preserve legacy socket detection for workers that predate the lock. Remove launcher-side shared cleanup and make launch logs attempt-specific while preserving worker.log as the owner's diagnostic path.

Then introduce one keyed ownership service in the controller. Only it constructs permits. Carry permits through process helpers, recovery plans, and checkpoint/workspace barriers; borrow for nested operations instead of reacquiring. Include creation/resume, Move, park, cleanup, and destruction. Acquire before relay leases to avoid deadlock, revalidate durable placement after acquisition, and retain permits in executing tasks when a caller drops. Preserve bounded daemon swap admission and independent sessions.

Centralize restart intent transitions and reconciliation under this ownership. Claims cannot overwrite another operation. An actor reports an observation; only the owner decides completion. Pending detached startup is reconciled after daemon handoff rather than killed on handshake timeout. Failure, confirmed death, and stale operation IDs are handled explicitly. Preparation stages belong to individual operations and cannot overwrite live files.

## Milestones


Worker ownership is established before the first diagnostic, socket, or journal write. The worker binary acquires `WorkerRootOwner`, passes it through login re-exec and the runtime, and retains it through exit reporting. Run `cargo test root_owner` from the repository root outside the sandbox: independent roots proceed, same-root contenders fail without modifying the incumbent, re-exec retains ownership, and process exit releases it even without Rust destructors.

Controller ownership serializes foreground lifecycle commands and makes background recovery defer immediately. `WorkerPermit` is an opaque capability: only the keyed admission service creates one, and nested calls borrow it. The key combines the instance's data directory and session ID. Run `cargo test worker_lifecycle` and `cargo test checkpoint_restart_waits_for_upgrade`: cancellation must not release executing blocking work, one session's checkpoint waits for its upgrade, actor recovery issues no commands during that ownership, and another session still progresses.

Durable replacement survives controller handoff. A durable intent is the database record that remembers an accepted replacement after the daemon exits. Conditional claims prevent overwriting it; a live boot is observed rather than restarted. Run `cargo test durable_worker_restart` and `cargo test a_ready_unchanged_worker_settles_an_abandoned_boot_on_the_next_sync`. The session-manager regression covers all stored phases, stale completion, live unready workers, and confirmed death; the quiet-worker regression proves readiness completion even when no new transcript event arrives. Existing isolated Move, resume, and daemon upgrade tests remain the broad behavior checks.

## Concrete Steps


Work in `/home/jonathan/Projects/mjolnir3` on its current branch. Use existing subprocess helpers and the normal mbx Cargo configuration without changing target directories. Run focused controller and worker tests after each coherent change, then `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all -- --check`. Every cargo test runs outside the restricted sandbox. Use the existing isolated upgrade tests and named instances, never the host default store. Commit only changed files, never git add -A; push to the configured upstream after validation, as authorized by the user.

## Validation and Acceptance


Deterministic tests pause one replacement while triggering checkpoint recovery and actor recovery, and prove no second stop/start runs. A second session must still progress. Test stale completion, Move/teardown races, and dropping waiters while subprocess execution retains ownership. Start two workers before either binds a socket: one is refused and shared journal, diagnostics, pidfile, and socket remain unchanged. Test descriptor inheritance across re-exec and release on process exit. Handoff during detached startup must reconnect or resume the same intent without restarting a live process. All existing isolated upgrade regressions must continue passing.

## Idempotence and Recovery


No production store or session repair is authorized. Keep the lock inode stable while any worker owns it; remove roots only after their process trees stop. Persist accepted replacement before mutation and retain it across daemon exit. Failed or cancelled disposable preparation can restart; a lost acknowledgement does not authorize replay. Preserve shipped migrations and compatibility.

## Artifacts and Notes


Incident evidence: duplicate ordinals 10946–10953 and `relay event gap: expected 10947, found 10946`; successful regression must produce a single monotonic journal with one writer.

## Interfaces and Dependencies


Use existing crates, a keyed admission service with Tokio notifications and cancellable waits, std::fs file locks, the serialized SQLite writer, and shared subprocess helpers. A controller ownership permit is opaque and keyed by instance data directory plus session ID. Process mutation and durable restart completion require it. Worker root ownership holds a file descriptor for the entire root-writing lifetime. No new workspace crate, schema shape, or public wire protocol is needed.
