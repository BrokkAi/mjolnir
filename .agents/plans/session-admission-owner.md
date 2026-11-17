```md
# One owner decides which work may use a session's worker (#1562, #1564)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

The daemon runs many kinds of work against one session's live worker: a person's Move, close, suspend, resume or restart; automatic recovery copies, worker upgrades, credential syncs, quota recovery and delegation result delivery. Today four independent per-session gates decide whether each may run, and none of them knows what the others are doing, so one kind of work can starve or block another.

Two failures on 2026-11-17 show it. In #1564 a renewed Claude login (renewal revokes the previous access token) never reached session 85e2d8a9 because its recovery copies, each about ten minutes long, ran back to back and held the only background slot; the credential sync was turned away every minute for half an hour and every turn failed with "Authentication required". In #1562 a person's manual `mj move --profile codex4` for session 70506548 failed at once with "session already has a lifecycle operation" because some other operation held the session actor's relay lease; meanwhile automatic quota recovery retried every 15 to 50 seconds for 15 minutes under a 15-second cap that counted queue time, and finally moved the session to a profile the person had not asked for.

After this change one owner per session decides. A person's lifecycle operation announces itself first, stops automatic work that can be safely dropped, waits for the rest and names it while waiting, and then runs; it never fails "busy". Automatic work tries admission, is refused with the name of what holds the session, and retries on a state change or with backoff. Kinds of work that do not conflict, such as a credential sync and a recovery copy's transfer, run together. The relay lease is handed out by the same owner, so the actor never has to refuse a duplicate.

How to see it working: in an isolated instance, start a long recovery copy (or hold one in a test), renew a login, and observe the login pushed while the copy runs; with a delegation delivery or quota recovery in flight, run `mj move` and observe "Waiting for <named operation>" followed by the Move completing, not a refusal.


## Progress

- [x] (2026-11-17 04:20Z) Repaired session 85e2d8a9 by hand (installed the current claude6 login, refresh token stripped, into its container); its turns resumed.
- [x] (2026-11-17 04:30Z) Diagnosed #1564 and filed it; mapped the four admission mechanisms and their callers for this plan.
- [x] (2026-11-17 05:10Z) Milestone 1: `RecoveryGate` holders carry a `BackgroundWork` kind with one compatibility rule; credential sync runs beside a recovery copy. Regression `a_new_login_reaches_a_session_while_its_recovery_copy_runs` (fails with the rule disabled, passes with it) and gate test `lifecycle_reservation_waits_for_every_holder_it_preempts`. Crate clippy clean; 50 focused tests pass.
- [ ] Milestone 2: labelled holders and foreground intents in the owner.
- [ ] Milestone 3: the relay lease is granted by the owner.
- [ ] Milestone 4: quota recovery and delegation delivery as droppable automatic work.
- [ ] Milestone 5: fold `WorkerPermit` admission into the owner.
- [ ] Final validation, review, commit, push.


## Surprises & Discoveries

- Observation: the recovery copy holds two gates, at different times. It holds the background slot for its whole run (about ten minutes for 85e2: 400 MB native state and a 220 MB repository packaged on an EC2 target and transferred), and the actor relay lease only while it captures (10 to 64 seconds), after which it releases the lease and keeps dispatching (`LatchExclusivity::ReleaseAfterLatch` in `mj-controller/src/controller/checkpoint/latched.rs`).
  Evidence: daemon log 2026-11-17 03:24 to 03:46, "checkpoint target state captured; releasing ACP dispatch ... barrier_held_ms=64113", then "archive packaged ... pack_ms=103076", "transferred ... transfer_and_checksum_ms=395091"; delegation deliveries to 85e2 failed with "session already has a lifecycle operation" during the captures.

- Observation: `AUTO_CHECKPOINT_INTERVAL` (10 minutes, `mj-controller/src/recovery.rs`) is measured from the previous checkpoint's `created_at`, and a failed turn is a completed turn, so a session whose copy takes ten minutes is copied continuously. The gap between copies was 18 seconds.

- Observation: renewing a Claude login revokes the old access token at once (85e2 held a token whose `expiresAt` was 07:22 and was refused at 03:44). So every session must receive the push before its next request.

- Observation: worker replacement (`upgrade_session_worker`, `mj-controller/src/controller/worker_restart.rs`) does not restage the login. It keeps the session's harness home, so a push that has landed survives; with credential sync no longer starved the push reaches a replaced worker within one sync interval (60 seconds).


## Decision Log

- Decision: generalize `RecoveryGate` (`mj-controller/src/recovery_gate.rs`) into the single per-session owner rather than add a fifth mechanism.
  Rationale: it already owns per-session background admission, lifecycle reservations, preemption by cancel flag, daemon-handoff draining, and the busy publication waiters use. AGENTS.md: one owner decides each fact; a new gate beside the old ones would be another predicate for the same question.
  Date/Author: 2026-11-17 / parent session.

- Decision: work is classified by kind, and one compatibility rule decides which kinds may hold a session together. A credential sync may run beside a recovery copy; a worker replacement runs alone.
  Rationale: credential relay requests are served by the worker's live relay transport regardless of ACP dispatch or a checkpoint barrier (`mj-worker/src/relay/requests.rs`, "connection-only requests"), and checkpoint archives leave login files out (`mj-worker/src/checkpoint_tests.rs`, "credentials entered the archive"). Mutual exclusion between them protected nothing and starved logins.
  Date/Author: 2026-11-17 / parent session.

- Decision: a person's lifecycle operation (and every daemon lifecycle operation that today calls `reserve_recovery_or_cancel`) is a foreground intent. Registering it is the first step of the operation, before preparation. From that moment no automatic work is admitted for the session, droppable holders are cancelled, and the operation waits, without a timeout, for the remaining holders, publishing a notice that names them. Foreground intents for one session run in arrival order.
  Rationale: #1562 expects "a manual Move reserves priority before preparation, pre-empts automatic work that can safely be dropped, and waits for the named lease". AGENTS.md: never leave the user stuck; failing visibly means reporting, not refusing when the system can wait. No timeout because the wait ends when the named work ends or is cancelled, and the person can cancel their operation.
  Date/Author: 2026-11-17 / parent session.

- Decision: an automatic Move that quota recovery has started is foreground work and is not droppable once it has committed (the lifecycle control's `is_committed()`); before it commits it is cancelled when a person's lifecycle request for the same session arrives. Quota recovery's scheduling before the Move starts is droppable automatic work.
  Rationale: a committed Move may have stopped the source; dropping it would strand the session. Before it commits, nothing durable has happened that a later attempt cannot redo.
  Date/Author: 2026-11-17 / parent session.

- Decision: the daemon's lifecycle registry (`state.owner().lifecycle`, `start_or_join_lifecycle_controlled` in `mj-controller/src/daemon/lifecycle.rs`, `check_request` in `mj-controller/src/daemon.rs:630`) is where foreground intents queue; the gate does not keep a second queue. A request that differs from the running lifecycle no longer fails with "another lifecycle ... is already running": a person's request cancels a running automatic lifecycle that has not committed, and otherwise waits for the running one to finish, with a notice naming it, then starts. Identical requests still join. The registry takes the gate's foreground admission when a lifecycle starts, before its work runs, instead of each work closure calling `reserve_recovery_or_cancel` part-way through.
  Rationale: the registry is already the one per-session owner of foreground operations and their visibility; a second FIFO in the gate would be another predicate for "who runs next". Taking the gate admission at lifecycle start is "acquire after the decision that needs it" with the decision made once.
  Date/Author: 2026-11-17 / parent session.

- Decision: the session actor's relay lease is granted only to a holder admitted by the owner whose kind needs the connection to itself. Among admitted holders that need it, the lease is handed out in admission order (waiting), never refused as a duplicate. The actor's "session already has a lifecycle operation" branch becomes an invariant violation that names the current holder.
  Rationale: two gates for one session (background slot and lease) let each decide on its own snapshot, which is how the manual Move lost a race it could not see. One serialized decision point makes the race inexpressible.
  Date/Author: 2026-11-17 / parent session.

- Decision: a holder can narrow its kind through an explicit transition, used by the recovery copy: it is admitted as a capturing copy (needs the lease, excludes delegation delivery) and becomes a transferring copy (no lease, compatible with delegation delivery and credential sync) when it releases the capture lease.
  Rationale: the copy needs exclusivity only during capture; holding the stronger kind for its whole run starves delivery for minutes. A single state machine with explicit transitions, not a second flag.
  Date/Author: 2026-11-17 / parent session.

- Decision: quota scheduling loses its 15-second cap. Each relay request keeps only the bound it already has for its own exchange; queue time never counts. A failed attempt is retried after a backoff or on a state change (new assessment revision, profile change), never immediately. After a caller gives up, acceptance is reconciled by command ID before a new attempt.
  Rationale: AGENTS.md forbids speculative caps and timeouts that include queue time, and requires retry loops to carry a backoff or a state change that ends them. Submits are already keyed by command ID (`quota-schedule-<completed ordinal>`), so reconciling acceptance is a lookup, not a replay.
  Date/Author: 2026-11-17 / parent session.

- Decision: `WorkerPermit` (`mj-controller/src/worker_lifecycle.rs`) keeps its capability role (task-local nesting, worker-restart intent bookkeeping) but stops deciding admission: its `try_acquire` and `acquire` become infallible for an operation that holds an owner admission, and callers that acquire it without an admission are converted to take one. This is the last milestone so the earlier ones ship without it.
  Rationale: today it is a third per-session admission predicate. Folding it last limits risk: Milestones 1 to 4 fix both reported failures.
  Date/Author: 2026-11-17 / parent session.


## Outcomes & Retrospective

(To be written at milestone completions.)


## Context and Orientation

The daemon (`mj daemon-run`, crate `brokk-mj-controller`, directory `mj-controller/`) is the control plane; one worker process per session (crate `mj-worker`) runs the coding agent. The daemon talks to a worker over a "relay" connection. Each live session has a "session actor" (`mj-controller/src/session_manager/actor.rs`), a task that owns the session's relay connection, runs queued relay jobs on it, and can lend the connection out exclusively ("lease") to one caller at a time (`ActorCommand::Lease` near line 1115; `ActorLifecycle` in `mj-controller/src/session_manager/actor_types.rs` holds `active_lease: Option<u64>`). A second lease request while one is active is refused with "session already has a lifecycle operation". Callers obtain a lease with `ManagedSessionHandle::lease_connection` or `lease_idle_connection` (`mj-controller/src/session_manager/handle.rs` lines 308 and 313).

The four per-session admission mechanisms today:

`RecoveryGate` (`mj-controller/src/recovery_gate.rs`) is created once per daemon runtime and shared by the recovery coordinator (`mj-controller/src/recovery.rs`, starts recovery copies), the worker upgrade coordinator (`mj-controller/src/worker_upgrade.rs`, replaces workers with the current build), and the credential sync coordinator (`mj-controller/src/worker_client/credential_sync.rs`, function `admitted`, pushes logins, AWS credentials, GitHub tokens and skills). Background work calls `try_start`, which returns `None` if the session is held or reserved. Foreground lifecycle operations call `reserve_recovery_or_cancel` (`mj-controller/src/pollers/lifecycle.rs` line 132), which reserves, cancels in-flight holders, and polls until they release; callers are in `mj-controller/src/daemon/` (`session_move.rs:670`, `close.rs:41,523`, `resume.rs:99,835`, `restart.rs:263`, `subagent_park.rs:212,308`). A turn review calls `reserve_when_idle` (`mj-controller/src/review_host/environment.rs:425`).

The actor relay lease, described above. Takers: Move (`mj-controller/src/controller/move_session.rs` lines 612, 636, 816, 2276), close recovery (`mj-controller/src/controller/lifecycle.rs:657`), checkpoint capture (`mj-controller/src/controller/checkpoint/barrier.rs`, `relay.rs:135`, `latched.rs`, `workspace_lease.rs`), and delegation result delivery (`mj-controller/src/daemon/delegation.rs:678`, which already retries with backoff).

`WorkerPermit` (`mj-controller/src/worker_lifecycle.rs`) is a process-global per-session slot for mutating worker processes, with a label (`reason`) and task-local nesting so nested calls reuse the permit. Foreground callers `acquire` (wait, with a notice), background callers `try_acquire`.

Move's own ownership map (`move_ownership()` in `move_session.rs` line 32), which records an in-flight Move for command refusal; it is bookkeeping, not admission, and is left alone.

The daemon lifecycle registry (`state.owner().lifecycle`, admitted in `start_or_join_lifecycle_controlled`, `mj-controller/src/daemon/lifecycle.rs` around line 92) holds at most one foreground lifecycle per session (Move, Suspend, Resume, Restart, Park, Unpark, ForceDestroy, UpgradeContainer, ...). An identical request joins the running one; any other request fails in `check_request` (`mj-controller/src/daemon.rs:630`) with "another lifecycle request with different selections is already running" or "another lifecycle operation is already running". This is what a person's Move meets while an automatic quota Move runs.

Quota recovery: the continuation loop (`mj-controller/src/daemon/continuation.rs`, around lines 640 to 790) acts once per worker assessment revision; it resolves a plan (`(env.quota)`) and then calls `quota::schedule` (`mj-controller/src/daemon/continuation/quota.rs` around line 406), which wraps lookup, checks, `SetQuotaRecovery` submit, sync, Move admission (`start_quota_move`) and fallback in `tokio::time::timeout(Duration::from_secs(15), ...)`. `quota::resume` has the same shape. The worker rejects a schedule whose cursor moved (`mj-worker/src/relay/commands.rs` around line 709, "quota recovery evidence is stale").

Daemon handoff admission (`gate.enter_unless_draining` in the continuation loop and `crate::upgrade::is_draining()` in `RecoveryGate::try_start`) is a different, daemon-wide gate for upgrades; it is not changed, and automatic work keeps refusing while it drains.


## Plan of Work

The owner keeps the type name `RecoveryGate` until Milestone 5 to keep diffs readable; the file gains the kinds, labels and intent queue. Each milestone compiles, passes its focused tests, and is committed on its own.

Milestone 1 (kinds). Holders carry a `BackgroundWork` kind: `WorkerReplacement`, `RecoveryCopy`, `CredentialSync`. `BackgroundWork::runs_beside` is the one compatibility rule; a copy and a credential sync may hold a session together, nothing else may share. `try_start(session_id, kind)`, `run_background(session_id, kind, work)` and `run_background_owned(session_id, kind, work)` take the kind. `cancel_busy` and `close` cancel every holder; `is_busy` is true while any holder remains; `finish` removes only the holder whose identity matches. Production call sites pass their kind (`recovery.rs`, `worker_upgrade.rs`, `credential_sync.rs::admitted`). This alone fixes #1564.

Milestone 2 (labels and foreground intents). Every holder and reservation has a `&'static str` label ("recovery copy", "worker upgrade", "credential sync", "Move", "close", ...). Replace `reserve` plus `cancel_busy` plus the polling loop in `reserve_recovery_or_cancel` with one async owner method, `RecoveryGate::enter_foreground(session_id, label, cancelled, notices) -> Result<ForegroundAdmission>`, which in one locked transition records the reservation (from then on `try_start` refuses), cancels holders, and then waits on the busy publication until the session has no holder; while waiting it publishes a notice through the operation's executor naming the remaining holders ("Waiting for recovery copy to stop"). A caller-cancelled wait drops the reservation. `reserve_when_idle` becomes the same entry without cancelling holders. `try_start` refusals return the holder labels so callers can log them. Then move the call into the lifecycle registry: when `start_or_join_lifecycle_controlled` starts a lifecycle's task, the task first awaits `enter_foreground` with the lifecycle kind's label and holds the admission until the lifecycle completes; delete the per-closure `reserve_recovery_or_cancel` calls (`session_move.rs:670`, `close.rs:41,523`, `resume.rs:99,835`, `restart.rs:263`, `subagent_park.rs:212,308`). Check each for a reason it reserved late (for example `subagent_park.rs:298` waits only after `begin_resumable_work`) and keep that ordering by having the registry call `begin_resumable_work`-style hooks first where needed. Finally change `check_request`: a request with a different kind or selections, from a person, cancels a running automatic lifecycle that has not committed and otherwise waits for the running one (registry waits on the existing result watch, notice names the kind and stage via `LifecycleHold`), then starts; automatic requests keep refusing so automatic work never queues behind people. Add an `origin` (person or automatic) to the lifecycle request, set to automatic by `start_quota_move` and the other daemon-initiated starts.

Milestone 3 (lease from the owner). Add `BackgroundWork::DelegationDelivery` and split the copy into `RecoveryCopyCapturing` and `RecoveryCopyTransferring` with `RecoveryAttempt::narrow(kind)` as the only transition (allowed from capturing to transferring). Kinds that need the relay lease: foreground admissions, `DelegationDelivery`, `RecoveryCopyCapturing`; their compatibility rule makes them mutually exclusive, so at most one lease-needing holder exists. `lease_connection` and `lease_idle_connection` take the admission (`&ForegroundAdmission` or `&RecoveryAttempt`) and pass its label to the actor; the actor records the label with `active_lease` and, on a second request, logs an error naming both labels and refuses (this must be unreachable; a test proves the owner prevents it). Convert the lease takers listed in Context: Move and close recovery already hold a foreground admission from Milestone 2; checkpoint capture holds its copy admission (manual checkpoints take a foreground admission labelled "checkpoint"); delegation delivery takes `DelegationDelivery` with `try_start` and keeps its existing backoff on refusal. The latched copy calls `narrow(RecoveryCopyTransferring)` at the point where it releases the capture lease (`latched.rs`, the `ReleaseAfterLatch` release).

Milestone 4 (quota recovery). Add `BackgroundWork::QuotaScheduling`, droppable and compatible only with `CredentialSync` and `RecoveryCopyTransferring`. `quota::schedule` and `quota::resume` take it with `try_start` before doing anything; a refusal records the holder labels and defers. Remove both 15-second `tokio::time::timeout` wrappers; the 5-second `wait_for_session` and each relay exchange keep their own existing bounds. Before a new attempt for the same completion, read the worker's view for the command ID `quota-schedule-<ordinal>` (it is in `handled_commands`) and treat an accepted earlier submit as done. In the continuation loop, a failed or refused attempt sets a per-session backoff (reuse `crate::recovery::backoff_delay`) and is retried only after it or on a new assessment revision. Log the worker's stale-evidence rejection with the expected and actual cursor. `start_quota_move` takes a foreground admission labelled "automatic Move" (not droppable once admitted), so a person's Move waits for it rather than failing.

Milestone 5 (WorkerPermit). Every operation that acquires a `WorkerPermit` already holds an owner admission after Milestone 4, except relay recovery (`session_manager/recovery.rs:158`), actor readiness observation (`actor.rs:1687,1738`), environment launches (`environment.rs:81,93,762`), container upgrade (`container_upgrade.rs:68`) and park (`subagent_park.rs:886`). Give each an owner admission of the right kind, then make `WorkerPermit::acquire`/`try_acquire` require the caller's admission (passed in) and stop waiting or refusing on their own. Keep task-local nesting and the restart-intent helpers.


## Concrete Steps

Work from the repository root (`/home/jonathan/Projects/mjolnir`, or the session clone). Build output stays in the normal target directory. Run tests outside the sandbox.

Milestone 1:

    cargo fmt -p brokk-mj-controller
    cargo test -p brokk-mj-controller --lib -- recovery_gate recovery::tests worker_upgrade a_new_login_reaches_a_session_while_its_recovery_copy_runs a_deferred_renewal_cycle_leaves_the_wake_for_the_next_one cancelled_operation_leaves_the_running_recovery_copy_alone credential_sync_preempted_by_lifecycle
    cargo clippy -p brokk-mj-controller --all-targets -- -D warnings

Each later milestone: the focused tests named in its acceptance plus the tests of the modules it edits, then clippy on the crate. After the last milestone, `cargo clippy --all-targets -- -D warnings` for the workspace and the full `brokk-mj-controller` suite once.


## Validation and Acceptance

Milestone 1: `a_new_login_reaches_a_session_while_its_recovery_copy_runs` pushes a renewed login through `reconcile_profile_guarded` while a recovery copy holds the session, defers it while a worker replacement holds it, and leaves the copy uncancelled. It fails before the change (the sync is deferred) and passes after.

Milestone 2: a test holds a recovery copy and a credential sync, starts `enter_foreground` for "Move", and observes that a new `try_start` is refused with the labels of the holders, both holders' cancel flags are set, the admission arrives only after both drop, and a second foreground intent queued behind it is admitted after the first is dropped. A cancelled wait leaves no intent behind.

Milestone 3: a test with a fake relay actor shows a foreground admission obtaining the lease while a delegation delivery holds `DelegationDelivery`: the delivery is cancelled, returns the lease, and the Move gets it (no "already has a lifecycle operation" error). A second test narrows a copy and shows delegation delivery admitted beside the transferring copy but not the capturing one.

Milestone 4: a test drives quota scheduling against a relay fixture whose exchanges take longer than 15 seconds in total and observes the schedule complete; another refuses admission and observes no retry before the backoff; another shows that a person's Move started while quota scheduling holds the session preempts it and completes. End to end in an isolated instance (`--instance admission-test`): a session on a profile with exhausted quota and a manual `mj move` started during quota recovery completes to the requested profile.

Milestone 5: existing worker restart, park and environment tests pass with the permit taking an admission; a test proves a background `try_start` is refused while an environment launch holds the session.


## Idempotence and Recovery

No schema or protocol change is planned; the owner is daemon memory that is rebuilt empty at startup, which is correct because every holder is daemon work that does not survive a daemon exit. If a milestone proves to need a protocol change (for example, reconciling quota acceptance requires a new relay request), stop and record it in the Decision Log first. Each milestone is committed separately and can be reverted alone.


## Artifacts and Notes

Evidence for #1564: daemon log `~/.local/share/mjolnir/logs/mj-daemon-20261117T024806.784Z-232639.log`; renewal at 03:22:53 ("refreshed the Claude login profile_id=claude6"), pushes to 12 sessions at 03:23:28 to 03:23:40 without 85e2, copies of 85e2 at 03:24 to 03:34, 03:35 to 03:45 and from 03:46, worker replacement at 03:35:26. The session's login file had mtime 01:39 and token fingerprint 5bcf8100…, the host's c46f2e58…. Evidence for #1562: the issue body and the H11 diagnosis session e66ac5c693f347dc1128c8b49435672b.


## Interfaces and Dependencies

At the end of Milestone 4, in `mj-controller/src/recovery_gate.rs`:

    pub enum BackgroundWork { WorkerReplacement, RecoveryCopyCapturing, RecoveryCopyTransferring, CredentialSync, DelegationDelivery, QuotaScheduling }
    impl BackgroundWork { fn runs_beside(self, other: Self) -> bool; fn label(self) -> &'static str; fn needs_lease(self) -> bool; }
    impl RecoveryGate {
        pub fn try_start(self: &Arc<Self>, session_id: &str, kind: BackgroundWork) -> Result<RecoveryAttempt, Refusal>;
        pub async fn enter_foreground(self: &Arc<Self>, session_id: &str, label: &'static str, cancelled: &AtomicBool, notices: &impl CommandExecutor) -> anyhow::Result<ForegroundAdmission>;
    }
    impl RecoveryAttempt { pub fn narrow(&self, kind: BackgroundWork) -> anyhow::Result<()>; }
    pub struct Refusal { pub holders: Vec<&'static str> }

and in `mj-controller/src/session_manager/handle.rs`, `lease_connection(&self, admission: &impl HoldsSession)` where `HoldsSession` is implemented by `RecoveryAttempt` and `ForegroundAdmission` and yields the label. No new crates.


Revision note (2026-11-17): the lifecycle registry, found after the first draft, is a fifth per-session gate; Milestone 2 now makes it the place foreground intents queue and the taker of the gate's foreground admission, instead of a second queue in the gate.

Revision note (2026-11-17): initial plan, written after the #1564 repair and diagnosis, at the user's request to fix #1562 in the same design.
```
