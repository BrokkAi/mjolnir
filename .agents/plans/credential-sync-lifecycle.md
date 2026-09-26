# Coordinate credential sync with worker lifecycle

This plan follows `.agents/PLANS.md` and is maintained during implementation.

## Purpose / Big Picture

Credential reconciliation must not connect to a worker that parking or another lifecycle operation has stopped. A deferred or preempted sync produces no authentication failure; real connection and credential errors remain visible.

## Progress

- [x] Traced reported session failures to refused worker sockets around successful sub-agent parking.
- [x] Found profile reconciliation retains cloned targets across sequential sessions and does not participate in the recovery gate.
- [x] Admit reconciliation through the existing background gate and validate its durable target after admission.
- [x] Remove the TUI coordinator and publish daemon sync results through its existing notice feed.
- [x] Add deterministic cancellation and stale-target regressions; all four passed.
- [x] Run Clippy with warnings denied and formatting checks.
- [x] Finish workspace tests (all passed).
- [x] Validate final deferred-notice correction (45 poller tests passed).
- [x] Complete final Clippy pass with warnings denied.
- [x] Prepare the validated change for the required commit on master.

## Surprises & Discoveries

The exact reported 20:55:35.744429 timestamp appears in the TUI log, not the daemon log. The TUI ran a second independent coordinator, so removing it is necessary for the daemon gate to own all sync.

The poller excludes Parked records already. That filter cannot protect a sync whose copied target predates the park. `reserve_recovery_or_cancel` already gives lifecycle operations exclusive access against recovery and worker replacement, but credential sync bypasses it.

## Decision Log

Use the existing RecoveryGate as the owner of per-session background access. A cancellable RAII operation releases admission on completion, cancellation, panic or task abort. Re-read the durable target under admission because the published list can lag completed lifecycle work. Keep profile serialization, since canonical credentials are shared by a profile. Remove the TUI coordinator, its result feed, and local triggers; the daemon already observes worker signals and reconciles periodically. TUI quota refreshes that change credentials are picked up by the daemon on its next periodic pass. Publish notices through the existing daemon notice feed.

## Context and Orientation

`mj-controller/src/worker_client/credential_sync.rs` reconciles each profile's workers. `mj-controller/src/recovery_gate.rs` coordinates worker background access with lifecycle reservations. `mj-controller/src/daemon/subagent_park.rs` reserves that gate before stopping a worker. `mj-controller/src/server_runtime/run.rs` constructs the daemon sync coordinator. `mj-cli/src/dashboard.rs` and its drains previously constructed and drove a duplicate coordinator.

## Plan of Work

Add an async cancellable background operation to RecoveryGate, then pass the daemon's gate to the credential coordinator. Inside admission, reload the target and skip a changed or removed target. Cancellation and deferral must be distinct from successful reconciliation so they cannot mark a login refused. Test gate exclusivity, preemption and cleanup with controlled futures, and stale-target validation with isolated fixtures.

## Milestones

First establish safe gate admission and cancellation with deterministic tests. Then wire credential sync and verify a stale target cannot start a relay. Finally complete workspace validation and commit only these changes.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir`, use normal mbx Cargo commands, without changing build storage. Run `cargo test` outside the sandbox and `cargo clippy --all-targets -- -D warnings` in the dev profile. Run `cargo fmt --all -- --check` and inspect the final diff before committing on the current branch.

## Validation and Acceptance

A reserved session cannot begin a sync. A lifecycle reservation cancels an admitted sync and waits until its guard releases the gate. A stale parked or replaced target never launches a proxy. Deferred syncs produce no successful authentication outcome, while real failures still propagate. Automated tests use temporary isolated state and no test build accesses the live instance.

## Idempotence and Recovery

No live store migration or worker restart is needed. Reconciliation repeats naturally on target publication or its periodic tick. All guard exit paths release the gate.

## Artifacts and Notes

Observed session: `87d76cb50156a9105a1e5cb19d99f21b` (`3512-implement`). Logs show `outcome=Parked` at 20:36:21, followed by sync `Connection refused (os error 111)` at 20:36:32 on 2026-09-26.

## Interfaces and Dependencies

Reuse `RecoveryGate`, `CredentialSyncTarget` equality, existing Controller loading and Tokio cancellation selection. Add no crates or schema changes.

## Outcomes & Retrospective

The daemon is now the sole credential-sync owner. Its syncs share lifecycle admission, and stale target snapshots cannot launch proxies after parking or replacement. The four new regressions and all 1,835 controller tests passed. Clippy and formatting passed. The complete workspace suite passed, including isolated upgrade and PTY tests. Final review found that the notice formatter treated an absent outcome as a completed check; it now emits no login verdict for a deferred sync. A fifth regression covers that distinction; all 45 focused poller tests passed after the final correction. The final Clippy pass and formatting checks passed. The change is ready for the required commit. No running daemon, worker, or live store was replaced.

Revision: exact-timestamp evidence established the duplicate TUI coordinator as the source of the reported notice; expanded implementation to remove that second owner.

Revision: the notice formatter must also distinguish deferral from a completed check; updated its existing fixtures to represent successful empty-action outcomes explicitly.

Final validation: `cargo test` passed for the workspace; after the notice correction, `cargo test -p brokk-mj-controller pollers::tests` passed all 45 tests. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed.
