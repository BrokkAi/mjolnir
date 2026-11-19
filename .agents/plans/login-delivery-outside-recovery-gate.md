# Deliver renewed logins without waiting on unrelated session work (#1600)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

Mjolnir renews each Claude login about every four hours. Renewal makes the provider reject the old access token within seconds, so every session that uses the profile must receive the new login before its next model request. Today a session does not receive it while certain other background work holds that session: a review of the session's work, a worker upgrade, relay recovery or worker observation. On 2026-11-19 a worker upgrade held several remote sessions for up to eight minutes, and a review held another. All five sessions that then failed with authentication errors were sessions whose login delivery had been refused for that reason.

After this change, a renewed login reaches every connected session as soon as the daemon learns of it, whatever other background work holds the session. A session that loses a request to the old login in the few seconds before delivery continues by itself, through the worker's existing authentication retry, and the user sees no notice for it. A notice appears only when the user must act: when the provider rejects the profile's current login, which needs `mj login`.

To see it working after installation, wait for a Claude renewal (the daemon log line `refreshed the Claude login profile_id=<profile>`) while a review or worker upgrade runs. The newest daemon log under `~/.local/share/mjolnir/logs/mj-daemon-*.log` contains no `credential sync admission unavailable` lines, every connected session of that profile has a `synchronized a harness login direction="pushed"` line, and no `Auth failure on profile` notice appears for a session whose turn continued.


## Progress

- [x] (2026-11-19 04:10Z) Diagnosis from live logs; design from the Advisor session; read-only code map of the gate, actor, coordinator, classification and tests.
- [x] (2026-11-19) Milestone 1: credential sync leaves the RecoveryGate; the actor owns the "login owed" wake; gate-only deferral and backoff code deleted; hard-won integration test passes.
- [x] (2026-11-19) Milestone 2: a daemon that finds a login renewed by another process syncs at once (commit `14c323648`).
- [x] (2026-11-19) Milestone 3: the refused-or-stale decision reads the host login under the native login lock.
- [x] (2026-11-19) Milestone 4: notices reduced to the cases that need the user; code made unreachable by Milestones 1-3 deleted.
- [x] (2026-11-19) Clippy on touched crates (core, controller, daemon, store) is clean; `cargo fmt --check` differs only in files this change does not touch (`mj-core/src/state/tests.rs`, `mj-store/src/database/sessions.rs`, `mj-worker/src/relay/journal/tests.rs`).
- [ ] Review; push (the parent does both).


## Surprises & Discoveries

- Observation: renewal makes the old access token fail within seconds, although it still has hours of nominal validity. No provider document states this.
  Evidence: daemon log `mj-daemon-20261118T131541.092Z-1920668.log`. The new login was issued at about 17:51:09.7 (eight hours before its `expiresAt`), and a session's request with the old token failed with an authentication error at most about 4.4 seconds later (notice at 17:51:15.270).

- Observation: the set of sessions whose delivery the gate refused is exactly the set that reported authentication failures.
  Evidence: `mj-daemon-20261119T032855.385Z-2682749.log`. At 03:30:44 the s50 daemon renewed claude7. The default daemon adopted the new login 78 ms later and pushed it to 34 sessions. It logged `reason="credential sync admission unavailable"` for 18 more. The five sessions with `Auth failure on profile claude7` notices between 03:30 and 03:37 are all among the 18. One upgraded session received the login one second after its worker swap finished:

      03:37:36.330 replaced the session worker with the current build session_id=d1c8bd55...
      03:37:37.240 synchronized a harness login direction="pushed" profile_id=claude7 session_id=d1c8bd55...

- Observation: pinned Claude Code (2.1.326 with claude-agent-acp 0.108.0) ends a turn at once on a 401 when its login file has no refresh token, and reads a replaced login file on its next request. Session copies never carry a refresh token (#1490). So no delivery scheme can prevent every authentication failure: a request already in flight with the old token fails. The worker's authentication retry is the part that recovers, and it stays.
  Evidence: the Advisor's inspection of the pinned bundle (minified handler for OAuth 401 returns "no retry" for a file login without a refresh token).


- Observation: the relay client's per-call cancellation flag (`RelayClient::cancellation`, `set_cancellation`) was set only by the credential sync job. With the gate gone nothing sets it, so Milestone 1 removed it and the cancel case of `project_memory_queueing_has_no_deadline_and_remains_cancellable` (which set it by hand).
  Evidence: `cargo check` warned that `set_cancellation` was never used after the job stopped calling it.

- Observation: a non-current session in `aws_cycle` is answered (`Ok(None)`), not retried, so a renewal task's deferred-cycle retry has no source left in tests with a direct relay. `a_deferred_renewal_cycle_leaves_the_wake_for_the_next_one` was removed; deferral now comes only from an actor's lease or reconnect, which an actor-based test must drive.

## Decision Log

- Decision: credential sync leaves the RecoveryGate completely. This covers every step of the job (harness login, AWS, skills, GitHub token, slopcop login) and the per-session role-scoped AWS renewals, which use the same gate kind.
  Rationale: the gate was added in commit c8d27f3c4, when credential sync opened its own proxy connection to workers and could reach a worker that a sub-agent park had just stopped (see `.agents/plans/credential-sync-lifecycle.md`). Sync now runs only as a job on the session actor's own connection. The gate protects none of the data these steps touch; its only effect today is refusing delivery while unrelated work holds the session. One rule for the whole job is simpler than deciding step by step.
  Date/Author: 2026-11-19, parent session, approved by the user.

- Decision: the session actor owns the fact "this session is owed a credential sync". When it cannot run a credential job because a lifecycle operation leases its connection or the connection is reconnecting, it refuses the job as today and remembers that the session is owed. When its connection is usable again, it wakes the coordinator, which runs a fresh reconciliation for that session. The actor does not hold the job itself.
  Rationale: the Advisor proposed a latest-wins job held on the actor. A held job keeps its reply open, and the coordinator's profile pass awaits that reply, so a long lease (a Move, a checkpoint barrier) would keep the whole profile's pass open. Avoiding that needs a second, asynchronous result path. A wake gets the same single owner without that path. The coordinator rereads the host login when it reconciles, so the delivered login is always the newest one, which is what "latest wins" was for. Today the lease case keeps this state in the gate (`deferred_credentials`, `credential_wakes`) and the reconnect case keeps it in the actor (`wake_credentials_on_connect`). After this change both live in the actor.
  Date/Author: 2026-11-19, parent session.

- Decision: a foreground lifecycle operation no longer cancels an in-flight credential sync.
  Rationale: the cancellation came from the gate. Without it, an operation that needs the actor's connection (its lease) waits for the job already running on that connection to finish. That job is a few relay requests, each bounded by the relay connection's request timeout (`RelayClient::call`, `mj-controller/src/worker_client/exchange.rs`). A worker that hangs would delay the operation by at most that timeout, and the operation's own requests to such a worker would hang as well. No failure of this kind has been observed. A spurious failure notice from a sync that reaches a worker being stopped by an operation that does not take the lease (destroy teardown, suspend preparation) is possible but not observed. The coordinator already discards results whose target changed or ended.
  Date/Author: 2026-11-19, parent session.

- Decision: credential sync does not check whether a daemon handoff is draining.
  Rationale: AGENTS.md lets work that is short, safe to stop and started again by the next daemon run without holding handoff admission, and the handoff cancels it. Credential sync never held handoff admission. The worker installs a login atomically, so stopping mid-request leaves either the old or the new file. The next daemon's coordinator queues every session when it first publishes its targets.
  Date/Author: 2026-11-19, parent session.

- Decision: for renewals by another daemon, the daemon's policy remembers, per Claude profile, the renewal due time of the last grant check. A check whose due time differs from the remembered one means the login changed, and the policy calls `sync_profile_now` at once. There is no host-wide renewer and no file watch.
  Rationale: every daemon sharing a profile schedules its renewal check for the same due time, because they all read the same login expiry. The daemon that loses the race to the native lock already gets `Renewed` and syncs. A daemon that checks just after the renewal finished gets `Waiting` with the new due time and does nothing until its 60-second tick. Remembering one number closes that gap. The policy is daemon memory that rebuilds itself after a restart, and a new daemon's coordinator queues every session at start anyway.
  Date/Author: 2026-11-19, parent session.

- Decision: the decision "was the rejected login the host's current login?" reads the host login while holding the native login lock (`CredentialWriteGuard`). If the lock cannot be taken within its budget, the answer is "unknown", never "refused".
  Rationale: the renewer holds that lock for its whole exchange with the provider. A plain read during a renewal sees the old login, matches the rejected token and wrongly concludes that the provider refused the profile's login. That blocks new sub-agents on the profile and tells the user to run `mj login`.
  Date/Author: 2026-11-19, parent session.

- Decision: keep the #1581 child hold (`ChildState::Deciding` and `ChildState::RetryOwed` in `mj-daemon/src/server_runtime/api.rs`, the delivery registry and the worker's published held login). Delete only classification code that Milestones 1-3 make unreachable. Do not reduce classification to a single comparison in this plan.
  Rationale: those states make a parent wait while a child's worker still owes its authentication retry. They are not gate machinery. Deleting them would bring back the 2026-11-17 incident, in which a parent was told that its child had failed during a renewal (see `.agents/plans/renewed-login-child-failures.md`).
  Date/Author: 2026-11-19, parent session.

- Decision: user-facing notices remain only for a refused login (the user must run `mj login`) and for a reconciliation that failed with an error. "Stale login, renewed login pushed", "failed on a stale login, since renewed" and "refusal not established" become INFO log lines. The refused notice no longer advises setup tokens.
  Rationale: the user reported "a lot of auth failures" from these notices, and none of them needed action. The worker already writes "Claude's login changed after an authentication failure. Automatically continuing the interrupted work." into the transcript when it retries. On 2026-11-19 the user ruled that one-year setup tokens are mostly orphaned code and that login handling must work with the normal renewing login, which quota checks need anyway.
  Date/Author: 2026-11-19, parent session, setup-token ruling by the user.

- Decision: the worker upgrade holding its exclusive gate slot through preparation and upload is out of scope here. It is tracked as #1601.
  Rationale: after Milestone 1 that slot no longer blocks login delivery. It still blocks delegation delivery and recovery copies, which is a separate fix with its own cancellation and deduplication questions.
  Date/Author: 2026-11-19, parent session.

- Decision: the acceptance test is a Rust integration test in `mj-controller/src/worker_client/tests.rs` with a real session actor and fixture worker, not an end-to-end run.
  Rationale: no end-to-end harness runs a Claude session through an mj instance against a fake API, and nothing can pause a real review or upgrade at a known point. The integration test can hold the real gate states directly and is deterministic.
  Date/Author: 2026-11-19, parent session.

- Decision: `CredentialWakes` is created with the session manager's update channel and reached through `SessionManagerControl::credential_wakes` (public, next to `session_gate`) and `CoalescedUpdateSender`. The coordinator reads it from its `SessionRelays::Actors(manager)`; the daemon's `install_credentials` takes `Option<&CredentialWakes>`.
  Rationale: the coordinator is spawned after the manager and receives the manager's control, so no new wiring crosses crates.
  Date/Author: 2026-11-19, implementer (Milestone 1).

- Decision: a refused job that is neither leased nor reconnecting (no relay actor yet, a stale target) is not retried by a timer. The coordinator's own target-change publication queues the session again. The retry map and `backoff` for login passes are deleted as the plan says.
  Date/Author: 2026-11-19, implementer.

- Decision: `aws_cycle` always checks `target_is_current` before minting (it used `gate.is_some()` as a proxy for "a daemon is present"). The test thread-local `SESSIONS_ARE_CURRENT` now defaults to true; tests that need the on-disk check set it false.
  Date/Author: 2026-11-19, implementer.

- Decision: `CredentialSyncCoordinator::spawn_guarded` keeps an unused `_gate` argument for now, because its only caller is `mj-daemon/src/daemon/delegation/policy.rs`, which Milestone 2 owns. Remove the argument after Milestone 2 merges.
  Date/Author: 2026-11-19, implementer.

- Decision: tests removed because they only pinned gate behaviour: `credential_sync_preempted_by_lifecycle_does_not_report_a_login_result` (c8d27f3c; a sync no longer opens its own connection), `the_aws_install_waits_for_its_sessions_login_step_instead_of_being_skipped`, `a_cycle_that_cannot_reach_the_session_is_deferred_and_changes_nothing`, `a_queued_job_releases_its_hold_as_soon_as_admission_is_cancelled`, `a_running_credential_job_stops_when_its_admission_is_cancelled`, `a_deferred_renewal_cycle_leaves_the_wake_for_the_next_one`. Their text is in the implementer's report directory (`removed_tests.txt`). No reconnect-wake phase was added to the lease test: the reconnect branch sets the same `credentials_owed` flag that the lease branch sets and the same loop check clears.
  Date/Author: 2026-11-19, implementer.
- Decision (Milestone 3): `mj-core` offers `read_login_under_native_lock` (runs a reader while holding the native lock, `None` if the lock times out) and `rejected_login_is_current` (the plan's function, for file-stored Claude logins). The readers that can produce a refused verdict take their reading through the first: the sync pass re-reads the login under the lock for the session whose failure triggered it (replacing the pass's cached `current_access_token`), and `mj_controller::worker_client::current_claude_access_token`, which `current_profile_login` (and so the child hold in `api.rs`) uses for every Claude profile, file or Keychain. `sync_conclusion` and `failure_verdict` stay pure, because the policy's tick calls `conclude` on a daemon loop and must not wait for a lock. Reason for not calling `rejected_login_is_current` directly there: those sites need the current fingerprint itself (to compare with the failed turn's, and to hand to `wake_verdict`), and macOS Keychain logins cannot be read from `mj-core`. `rejected_login_is_current` has no production caller yet; it is covered by its test and is the form to use where only a yes or no is needed.
  Date/Author: 2026-11-19, implementer.

- Decision (Milestone 3): `RejectedLogins::refusal` keeps its plain file read. It only re-checks a refusal that reconciliation already established, and its callers hold a mutex on async threads, so it cannot wait for the lock. During a renewal it can only keep an existing refusal a moment longer, never create one. `CredentialWriteGuard::acquire_within` takes the Claude lock budget as a parameter (`acquire` passes `CLAUDE_REFRESH_LOCK_WAIT`).
  Date/Author: 2026-11-19, implementer.
- Decision (Milestone 4): `leveled_notice` keeps its `harness` parameter as `_harness`, because its caller is in `delegation/policy.rs` (Milestone 2's file). Drop the parameter after Milestone 2 merges. `setup_token_advice` is deleted. The test that pinned the setup-token advice (marked Hard-won c94c68c0) is renamed `only_a_refused_login_or_a_failed_reconciliation_reaches_the_person` and no longer carries that marker, because it now pins the opposite behaviour. The notice-naming test uses a `Refused` verdict, since the stale verdicts no longer produce a notice.
  Date/Author: 2026-11-19, implementer.
- Decision (review fixes): `read_login_under_native_lock` and `rejected_login_is_current` are async, not blocking as the plan said. Taking the locks queues blocking work, so waiting for them from a `spawn_blocking` thread could starve the pool; the locked read itself runs on the pool with the lock held. A new session actor starts owing a credential sync, so a sync the coordinator asked for before the actor existed is not lost (the retry timer is gone). `leveled_notice` reports a reconciliation failure before the renewed-login cases. Declined: a lock timeout leaves a failed turn unknown and is not rechecked (no daemon retry state for an unobserved case).
  Date/Author: 2026-11-19, implementer.
- Decision (Milestone 2, supersedes the earlier "policy remembers the due time per profile" decision): the renewal loop in `pollers/quota.rs` remembers the due time, not the policy. Rationale: the loop forwards only `Renewed` checks and checks with rejected logins, so the policy never saw a passive `Waiting` check and could not compare due times. Only a check that carries a due time updates the remembered value (a check without one leaves it alone).
  Date/Author: 2026-11-19, parent session, integrated by implementer.

- Decision (integration): removed the leftover `_gate` argument of `spawn_guarded`, the `_harness` argument of `leveled_notice` (the test helper `notice` keeps an ignored one to avoid editing about forty call sites), and `RuntimeState::worker_background_gate`, which nothing used afterwards. Deleted `rejected_login_is_current`: it had no production caller. Its lock-ordering test now goes through `read_login_under_native_lock`, which production uses; the timeout case is kept. This supersedes the Milestone 3 decision that kept the function for yes-or-no callers.
  Date/Author: 2026-11-19, parent session request.
- Decision (review of the integrated range): one login snapshot per pass. The sync pass used to deliver the login it read at pass start (A) and then reread under the lock for the triggering session (B), so the delivery registry could settle A while the verdict compared B, and `failure_verdict` could release the #1581 child hold early. Now a pass with an authentication-failure trigger reads a Claude login once, at pass start, under the native lock (`ProfileLogin::read`), delivers that snapshot and judges the failure against it. A lock timeout falls back to a plain read for delivery (the worker keeps the newer login) and leaves the verdict unknown, because the snapshot carries `read_under_lock: false`. Passes without that trigger keep the plain read.

- Decision (child-status path, supersedes the Milestone 3 decision that `current_profile_login` reads under the lock, and the per-batch reading): only the locked pass creates a Refused conclusion; later re-checks (`failure_verdict`, `wake_verdict`, `refusal`, `current_profile_login`) use plain reads because they can only lag during a renewal, never invent a refusal. Reason: a locked read on the child-status path waited up to 10 seconds per parent in mailbox maintenance and stalled unrelated questions and reports, and a timeout there became `Unrefused`, so a parent was told to resend (the #1581 incident). `BatchLogins` and the concurrent resolution were removed because nothing on that path waits any more.
  Date/Author: 2026-11-19, parent session request, implemented by implementer.
- Decision (final review round): (1) `ProfileLogin::publish` keeps `read_under_lock` when a new reading has the same bytes as the stored locked one, so a routine pass cannot erase the lock evidence of an auth-triggered pass; different bytes replace the reading, flag included. Readings are taken and published under `decide`, so a slower pass cannot publish an older login over a newer one; a pass that already holds an older snapshot may still deliver it, which the worker's newer-wins install makes harmless. (2) An AWS install now reports the new `CredentialSyncAction::AwsCredentialsPushed` instead of `Pushed`, so `pushed_to` and the login conclusions count only harness-login pushes (before, an AWS-only push silenced the unknown-cause notice and counted as a login delivery). `CredentialSyncAction` derives no serde and appears in no session view, API, store or relay type, so adding a variant changes no wire or persisted shape.
  Date/Author: 2026-11-19, parent session request, implemented by implementer.

## Outcomes & Retrospective

A renewed Claude login now reaches every connected session without waiting on a review, a worker upgrade or a recovery copy. The session actor owns "this session is owed a credential sync" and wakes the coordinator when its connection is usable. A daemon notices another daemon's renewal at its next check and syncs at once. The refused-or-stale decision reads the login under the native lock, so a renewal in progress is never judged a refusal. Notices appear only for a refused login or a failed reconciliation. The acceptance test `a_renewed_login_reaches_a_session_whatever_background_work_holds_it` failed on the base source (the review case) and passes now.

Remaining: the live check after installation (a renewal while a review or upgrade runs, with no `credential sync admission unavailable` lines and no `Auth failure on profile` notice for a continuing turn). No test drives the reconnect wake. The review declined six findings, recorded in the Decision Log.

Lessons: ask where each decision is made before choosing where to store its input (the policy never saw passive checks). Deleting a mechanism also removes the tests that pinned it and the flags that existed only for it, so search for those. A blocking helper that waits for async-backed locks can starve the blocking pool; make it async.


## Context and Orientation

Read this section before touching code. Terms are defined as they appear.

The daemon (`mj daemon-run`, crates `mj-daemon` over `mj-controller`) is the control plane. It owns the database, session records and lifecycle operations. A worker (`mj-worker`, one per session, local or remote) runs the agent's harness (here, Claude Code through the claude-agent-acp bridge). Several daemons can run on one host as separate named instances (the default instance, s50, s50-judge and so on). They share each harness profile's native home directory, so they share each profile's login file. A profile is a configured account for one harness, such as `claude7`.

The host login is the profile's login file in that native home, for Claude `<home>/.credentials.json`. Each session has its own copy of it in its staged home on the target. The copy omits the refresh token, so only the daemon can renew (#1490). Renewal happens in `mj-accounts/src/claude_usage.rs` (`refresh_grant_if_due`), driven by the quota poller (`mj-daemon/src/pollers/quota.rs`). The results are handled in the daemon's policy (`mj-daemon/src/daemon/delegation/policy.rs`, around lines 505-530), which calls `sync_profile_now` when the result is `ClaudeGrantCheck::Renewed`. A renewal runs while holding the native login lock, which is two lock directories next to the home (`.oauth_refresh.lock` and `<home>.lock`). The `CredentialWriteGuard` type in `mj-core/src/credentials/locking.rs` takes them, waiting at most `CLAUDE_REFRESH_LOCK_WAIT` (10 seconds).

Credential sync delivers the host login (and AWS credentials, skills, a GitHub token and the slopcop login) to sessions. Its coordinator is a task in `mj-controller/src/worker_client/credential_sync.rs`, started around line 380. It keeps a list of targets: the sessions with a worker connection. It runs passes. A pass reconciles every target of one profile. It is triggered by a 60-second tick (`SYNC_INTERVAL` in `mj-core/src/credentials.rs`), by `sync_profile_now`, by a change in the target list, or by a wake. For each target, `reconcile_target` (around line 1480) queues a `CredentialSyncJob` (around line 3100) on the session's actor and awaits its reply. The job asks the worker for its credential state and installs what is missing or older, using the relay requests `CredentialState` and `InstallCredentials`. The worker installs atomically under the same native lock and keeps the newer login, so a late or repeated delivery can never install an older login.

The session actor (`run_session_actor` in `mj-controller/src/session_manager/actor.rs`) is the task that owns a session's one relay connection, the daemon's long-lived connection to the worker. It handles commands one at a time. `ActorCommand::RelayJob` (around line 1108) runs a `RelayConnectionJob` (trait in `mj-controller/src/session_manager/actor_types.rs`, around line 80) on that connection. If a lifecycle operation holds a lease on the connection (it has borrowed the connection for a Move, a checkpoint barrier, a worker swap and so on), or the connection is gone and being re-established, the actor refuses the job with `RelayJobDeferred` and the reason `RELAY_JOB_LEASED` or `RELAY_JOB_RECONNECTING`. Today the lease case records the session in the gate (`RelayLeaseAdmission::defer_credentials`), and the gate publishes a wake when the lease is returned. The reconnect case sets the actor's `wake_credentials_on_connect` flag (actor.rs around lines 109 and 141-144), which publishes the same wake through the gate once connected. The coordinator listens to those wakes (`lease_returns`, credential_sync.rs around lines 419-454) and queues the session again.

The RecoveryGate (`mj-store/src/recovery_gate.rs`) is a per-session admission table for background work. `BackgroundWork` (around line 90) lists the kinds. `runs_beside` (around line 135) says which kinds may run together. `session_refusal` (around line 577) refuses a kind while an incompatible holder, a foreground reservation or a review reservation exists. Credential sync enters it as `BackgroundWork::CredentialSync` through `admitted_with_reason` (credential_sync.rs around line 1424). When refused, the job is never queued, the coordinator records `credential sync admission unavailable` and retries with a backoff from 1 to 60 seconds (`PassEvent::Attempted`, the `retries` map, `backoff`). This refusal is the defect.

An authentication failure is classified after the fact. The worker records the failed turn with a fingerprint (hash) of the access token it used. The daemon's tracker (`mj-daemon/src/pollers/credential_sync.rs`) starts a focused sync for that session. The conclusion (`sync_conclusion` and `failure_verdict` in `mj-core/src/credentials.rs`, around lines 1408-1620, built on `classify_authentication_failure` around line 717) decides whether the rejected token is the host's current one (refused: the user must run `mj login`) or an older one (stale: the session missed a renewal). Notices are produced by `leveled_notice` in `mj-daemon/src/pollers/credential_sync.rs` (around lines 196-250). The worker's authentication retry (`authentication_retry_intent` and `submit_authentication_retry` in `mj-worker/src/relay/commands.rs`, around lines 1743-1822) resubmits the interrupted turn once a login with a different identity is installed.

Wire and persisted shapes that must not change: `CredentialSyncSignal` (`mj-core/src/credentials.rs` around line 660, serialized into the session view with `deny_unknown_fields`), the relay field `held_login_fingerprint` and the worker's retry intent (relay protocol 69 and 70), and `WakeFailure::Authentication` text stored in a session's `last_error`. This plan changes no database schema and no relay protocol.


## Plan of Work

The work has four milestones. Milestone 1 is the fix. Milestones 2 and 3 close two smaller windows. Milestone 4 removes noise and dead code. Milestone 2 touches only the daemon policy and can be built in parallel with the others. Milestones 1, 3 and 4 touch the same files and are done in order.


### Milestone 1: credential sync without the gate

At the end of this milestone a login reaches a connected session while a review reservation, a worker upgrade or a recovery copy holds it, and the only things that can delay delivery are the actor's lease and reconnect.

In `mj-store/src/recovery_gate.rs`, delete `BackgroundWork::CredentialSync` and every arm that mentions it: its label, its pairs in `runs_beside` and the paragraph about it in the doc comment, and its case in `needs_lease` (keep `RecoveryCopyTransferring` there). Delete the credential wake machinery that lives in the gate: the `credential_wakes` field, the `deferred_credentials` set, `RelayLeaseAdmission::defer_credentials`, the wake published in that type's `Drop`, `credentials_deferred_for_test`, `wake_credentials`, `credential_wakes`, `take_credential_wakes`, and the comment near line 397 about cancelling syncs. Delete the gate tests that exist only for these (around lines 1256-1343, 1489-1500, 1595 and 1689-1736). Keep any assertion in them that covers another kind.

Create a small owner for credential wakes next to the coordinator, in `mj-controller/src/worker_client/credential_sync.rs`, named `CredentialWakes`. It holds the same watch of a set of session ids that the gate held, with `wake(&self, session_id: &str)`, `subscribe(&self)` and `take(&self) -> BTreeSet<String>`. Move the existing logic; do not write a new design. The coordinator owns it and selects on it where it selects on `lease_returns` today. Pass it to session actors through `CoalescedUpdateSender` (`mj-controller/src/session_manager/channels.rs`, the struct that already carries `session_gate`, `model_policies` and `cpu`). The caller that wakes after a snapshot revalidation (`owner.rs` around line 222) uses it too.

In `mj-controller/src/session_manager/actor_types.rs`, replace `RelayConnectionJob::deferred_by_lease` and `deferred_by_reconnect` with one method, `fn wake_when_usable(&self) -> bool { false }`. It means "if you refuse me, tell my owner when your connection is usable again". `CredentialSyncJob` returns true. In `actor.rs`, rename `wake_credentials_on_connect` to `credentials_owed`, and set it in both refusal branches of `ActorCommand::RelayJob` when `job.wake_when_usable()` is true. At the top of the loop, where the flag is checked today, wake through `CredentialWakes` when `credentials_owed` is set, `connection.is_some()` and `!lifecycle.is_leased()`, then clear the flag. A lease returned with a connection, a lease returned without one followed by a reconnect, and a worker swap's replacement connection all reach that check with no other code.

In `credential_sync.rs`, remove the gate from every path. `reconcile_target` and the role-scoped AWS renewal (`aws_job`, around line 1885) call their work directly instead of through `admitted_with_reason`. The GitHub token lookup stays before the actor job and simply runs unadmitted. Delete `admitted_with_reason`, `admitted` and `defer_as_none`. In `CredentialSyncJob::run`, keep one `target_is_current` check at the start of the job, which today runs only in the `hold.gated()` branch. Remove `hold.start()`'s cancellation, `gated()`, `set_cancellation` and the "cancelled by a lifecycle operation" result. Reduce `JobHold`, `HoldState`, `HoldPhase`, `HeldSlots` and `QueuedJob` (around lines 65-208) to what remains needed: the AWS install lock and session turn held until the actor job has run, and released if the awaiting task stops first. If nothing remains, delete them. `SessionSteps` exists only to keep the login and AWS steps from competing for the gate slot (see its comment around line 210). Delete it, except the `installs` instance that orders role-scoped renewals and `profile.deliveries`, which orders delivery results. Delete `PassEvent::Attempted`, the coordinator's `retries` map and retry timer, and `backoff`. After this milestone a refused job is refused only for lease or reconnect, and those are woken, never polled. Keep `RelayJobDeferred`, which other code uses.

Rewrite or delete the controller tests that pin the gate's behaviour. These are in `mj-controller/src/worker_client/tests.rs` around lines 3435, 4115, 4230, 4308, 4336, 4800 and 4835; find them by searching for `BackgroundWork::CredentialSync`, `credentials_deferred_for_test` and `admission`. `a_cycle_that_cannot_reach_the_session_is_deferred_and_changes_nothing` asserts the refusal that is now the defect. The tests about cancelling a queued or running job by admission have nothing left to test. Keep `a_login_deferred_by_an_actor_lease_is_pushed_when_the_lease_returns` (around line 5564) passing unchanged in intent. It now covers the actor-owned wake for leases. If no test covers the reconnect wake, extend that test with a second phase in which the connection drops while a sync is owed and the login arrives after reconnect.

Add the acceptance test described under Validation and Acceptance. Fold `a_new_login_reaches_a_session_while_its_recovery_copy_runs` (the #1564 test, around line 4835) into it as one of its holders, and keep its hard-won comment beside that case.


### Milestone 2: sync at once after another daemon's renewal

At the end of this milestone, a daemon whose renewal check finds a login that changed since its previous check syncs that profile at once instead of at its next 60-second tick.

The per-profile renewal loop `renew_claude_grant_when_due` (`mj-daemon/src/pollers/quota.rs`) remembers the renewal due time (`due_at_ms`) of its last check that carried one. A check whose due time differs from the remembered one means another process renewed the login, so the loop sends the policy a `QuotaUpdate::ClaudeGrant` with `login_changed: true`, and the policy calls `sync_profile_now` at once (`claude_grant_requires_sync` in `policy.rs`). Only a check that carries a due time updates the remembered value, so the first check after a daemon starts only stores one. The loop already sends `Renewed` checks and checks with rejected logins and drops the rest; `login_changed` adds the passive case. Do not change `mj-accounts`.

The test is `another_daemons_claude_login_change_is_forwarded_to_policy` in `mj-daemon/src/pollers/tests.rs`. It feeds the loop a `Waiting` result, then one with a later due time, and asserts that the policy update carries `login_changed: true`.


### Milestone 3: decide refused or stale under the native lock

At the end of this milestone, an authentication failure during a renewal is never judged a refused login.

Add to `mj-core/src/credentials.rs` an async function, `read_login_under_native_lock`, that runs a reader while holding `CredentialWriteGuard` for the profile's harness kind and native home (for macOS Keychain profiles, the same lock source that `claude_usage.rs` uses), and returns `None` when the lock could not be taken within its budget. The only place that creates a refused conclusion is the sync pass: for an authentication-failure trigger it reads the host login once, at pass start, under that lock (`ProfileLogin::read`), delivers that snapshot and judges the failure against it. A lock timeout leaves the verdict unknown. `failure_verdict`, `refusal`, `current_profile_login` and `current_child_profile_login` only re-check whether the login changed since that conclusion; they read plainly, because a plain read can only lag during a renewal and never invents a refusal, and a locked read there stalls mailbox maintenance (the review finding that ended it). The daemon's policy and the TUI must never wait on the lock.

Test the ordering in `mj-core`. Write a host login A, take `CredentialWriteGuard` in the test as a renewer would, and start the decision for a failure whose fingerprint is A's on another thread. Then write login B and drop the guard. The decision must return `Some(false)`. Before the change, the equivalent decision returns "refused". If the lock budget can be passed in without production code gaining a test-only knob, also show that a held lock yields `None`; otherwise leave that path to review.


### Milestone 4: notices and dead code

At the end of this milestone the user sees authentication notices only when they need to act, and the classification code has nothing left that only served gate refusals.

In `leveled_notice` (`mj-daemon/src/pollers/credential_sync.rs`, around lines 196-250), keep as user notices the refused case ("the provider refused the login checked during reconciliation. Run `mj login --profile …`") and the reconciliation-failed case. Remove the setup-token advice from the refused text. Turn the "stale login, renewed login pushed", "failed on a stale login, since renewed" and "refusal not established" cases into `tracing::info!` lines with the same wording, the profile id and the session id. The per-step failure notices and routine summaries in the same file (around lines 252-375) are unrelated and stay. Then delete code that no longer has callers after Milestones 1-3, such as reasons and branches that existed only for gate refusals. Use the compiler and clippy to find it. Do not touch the #1581 child hold or the wire and persisted shapes listed in Context and Orientation. Update the notice tests in that file to match.


## Concrete Steps

Implementers work in detached git worktrees so that their builds do not collide. They commit on the detached HEAD and do not push. From the main checkout:

    git worktree add --detach ~/Projects/mjolnir-wt/login-delivery HEAD
    git worktree add --detach ~/Projects/mjolnir-wt/login-renewal-notice HEAD

Milestones 1, 3 and 4 are built in `~/Projects/mjolnir-wt/login-delivery`, and Milestone 2 in `~/Projects/mjolnir-wt/login-renewal-notice`. Run every `cargo test` outside the restricted sandbox, because the tests use loopback sockets. Use the dev profile, which is the default. Do not redirect the Cargo target directory: builds go through mbx. Run only the tests for what changed. Do not run whole workspace suites.

After Milestone 1, in the `login-delivery` worktree:

    cargo test -p brokk-mj-store --lib recovery_gate
    cargo test -p brokk-mj-controller --lib worker_client::tests::
    cargo clippy -p brokk-mj-store -p brokk-mj-controller --all-targets -- -D warnings

The new acceptance test and the lease-return test must pass. If `worker_client::tests::` is slow, run the new test, the rewritten tests and the lease and reconnect tests by name instead.

After Milestone 2, in the `login-renewal-notice` worktree:

    cargo test -p brokk-mj-daemon --lib delegation::policy
    cargo clippy -p brokk-mj-daemon --all-targets -- -D warnings

After Milestones 3 and 4, in the `login-delivery` worktree:

    cargo test -p brokk-mj-core --lib credentials
    cargo test -p brokk-mj-daemon --lib credential_sync
    cargo test -p brokk-mj-daemon --lib server_runtime::api -- child
    cargo clippy -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-daemon --all-targets -- -D warnings
    cargo fmt --all --check

Any test named `Hard-won: #1581` must still pass, since it protects the child hold.


## Validation and Acceptance

The acceptance test is a Rust integration test in `mj-controller/src/worker_client/tests.rs`, modelled on `a_login_deferred_by_an_actor_lease_is_pushed_when_the_lease_returns`. It uses an isolated store, a real session actor and the fixture worker. Name it `a_renewed_login_reaches_a_session_whatever_background_work_holds_it`. For each holder in turn, it places that holder on the session through the real gate: a review reservation (`gate.reserve_when_idle(session_id)`), a worker upgrade (`gate.try_start(session_id, BackgroundWork::WorkerReplacement)`) and a recovery copy (`gate.try_start(session_id, BackgroundWork::RecoveryCopyCapturing)`). A review reservation and a worker upgrade refuse each other, so they run as separate cases, not together. With the holder in place, the test replaces the host login while holding `CredentialWriteGuard`, as another daemon would, and calls `sync_profile_now`. It asserts that the fixture worker reports the new login's identity while the holder is still held. It drops the holder only after the assertion. Mark it:

    // Hard-won: #1600: a review or worker-upgrade hold kept a renewed login from reaching the session

Before Milestone 1, the review and upgrade cases fail, and the coordinator logs `credential sync admission unavailable`. After it, all three pass. Keep the `Hard-won: #1564` comment on the recovery-copy case.

Behaviour on a live instance is checked after installation, which happens in a separate step. At the next Claude renewal, the newest `~/.local/share/mjolnir/logs/mj-daemon-*.log` contains no `credential sync admission unavailable`. Every connected session of the renewed profile has `synchronized a harness login direction="pushed"` within a few seconds of `refreshed the Claude login`, including sessions with a review running and sessions waiting for a worker upgrade. A session that reports an authentication failure in the first seconds after the renewal continues its turn (the transcript shows "Automatically continuing the interrupted work."), and no `Auth failure on profile` notice is shown for it.


## Idempotence and Recovery

All changes are code. There is no migration, no relay protocol change and no change to stored or wire formats, so an older daemon or worker interoperates unchanged and a revert is a plain revert. Worktrees can be removed and created again at any time (`git worktree remove ~/Projects/mjolnir-wt/<name>`). Tests use isolated stores and never touch the default instance.


## Artifacts and Notes

The 2026-11-19 refusals and failures, from `mj-daemon-20261119T032855.385Z-2682749.log` (session ids shortened):

    03:30:44.586 refreshed the Claude login profile_id=claude7 ... refreshed_by_this_daemon=false
    03:30:44.592 credential sync session disposition profile_id=claude7 session_id=06f468be... reason="credential sync admission unavailable"
    ... 17 more refusals for claude7 ...
    03:30:44.602 synchronized a harness login direction="pushed" profile_id=claude7 session_id=b2826633...
    ... 33 more pushes ...
    03:30:48.874 Auth failure on profile claude7 (session ...): a refusal of the profile's current login was not established: credential reconciliation did not reach this session.

The earlier maps used to write this plan are in `.mj/agents/3e0abc7228711c2bff6d59633db6b862/login-delivery-gate-removal-map.md` (this checkout) and in the Advisor session's report `login-delivery-map.md`. They are working notes and may be deleted. This plan does not depend on them.


## Interfaces and Dependencies

No new crates or dependencies.

In `mj-controller/src/session_manager/actor_types.rs`, the trait becomes:

    pub trait RelayConnectionJob: Send + Sync + 'static {
        /// If the actor refuses this job, it tells the job's owner when its
        /// connection is usable again.
        fn wake_when_usable(&self) -> bool { false }
        fn run<'a>(self: Box<Self>, client: &'a mut RelayClient) -> futures::future::BoxFuture<'a, ()>;
        fn refuse(self: Box<Self>, error: anyhow::Error);
    }

In `mj-controller/src/worker_client/credential_sync.rs`:

    pub struct CredentialWakes { /* watch::Sender<BTreeSet<String>> */ }
    impl CredentialWakes {
        pub fn wake(&self, session_id: &str);
        pub fn subscribe(&self) -> tokio::sync::watch::Receiver<BTreeSet<String>>;
        pub fn take(&self) -> BTreeSet<String>;
    }

In `mj-core/src/credentials.rs`:

    /// Runs `read` while holding the native login lock. `None`: the lock could
    /// not be taken within `lock_wait`. Async, so a waiter never occupies a
    /// blocking-pool thread that the lock's own work needs.
    pub async fn read_login_under_native_lock<T: Send + 'static>(
        kind: HarnessKind,
        store: &HarnessLoginStore,
        lock_wait: Duration,
        read: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T>;

In `mj-daemon/src/pollers/types.rs`, `QuotaUpdate::ClaudeGrant` gains `login_changed: bool`, set by the renewal loop when a check's due time differs from the previous one.

`mj-store/src/recovery_gate.rs` loses `BackgroundWork::CredentialSync` and all credential wake members. Nothing outside the files named above may depend on them after Milestone 1.

Note: the Cargo package commands above were corrected to the `brokk-` prefixed package names.
