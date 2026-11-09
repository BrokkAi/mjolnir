```md
# Cut the daemon's remaining steady CPU: orphan sweep, unchanged relay state, relay polling

This ExecPlan follows `.agents/PLANS.md` and remains a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.


## Purpose / Big Picture

The Mjolnir daemon (`mj daemon-run`, crate `mj-controller`) used 43% of a CPU core at steady state on 2026-11-09 with about 50 running sessions, after earlier fixes brought it down from 219–265%. A profile with call stacks attributed the rest mainly to three things. First, every five seconds a maintenance sweep deletes mailbox and GitHub rows whose session is gone, scanning every row of four tables on the database writer thread (about 10% of samples). Second, each session actor asks its worker for the whole relay operational state on every sync and decodes it even when nothing changed. Third, the actor syncs on a timer, every 150 ms while the session has work in flight and every 2 s otherwise, so most syncs find nothing new (together about 15% of samples, plus decode).

After this work, orphan rows cannot exist, so the sweep is gone; a worker omits the operational state from a sync reply when the daemon already has that exact state; and a session actor no longer polls a modern worker on a timer but waits on a dedicated connection that the worker answers when its events or state change. The observable result is lower daemon CPU at the same session count, measured the same way as before (a 60 s `pidstat` window and a `perf` profile 15+ minutes after a handoff, so post-install worker preparation does not distort it), with no change in what users see.


## Progress

- [x] (2026-11-09) Profile and locate the three costs; record evidence under `/mnt/optane/tmp/mj-cpu-20261106/` (`after5.data`, `after5.script`).
- [x] (2026-11-09) Milestone 1: migration 121 adds triggers that make orphan mailbox and GitHub rows impossible; the five-second sweep and `prune_mailbox_events_for_missing_sessions` are removed. Test `rows_of_a_gone_session_cannot_outlive_it`; 629 + 913 controller tests and clippy pass. Historical fixtures drop the triggers (`drop_session_row_triggers`), and two fixtures now save the session their GitHub rows name.
- [x] (2026-11-09) Review of milestone 1: an enqueue for a gone target now returns `Enqueued::TargetGone`, which user and agent messages report as an error; migration 121 raises the compatibility floor to 121 (older builds' read-back after insert would fail). Pushed f4c920a50 and installed; the daemon handoff waits for an ordinary `mj` command.
- [x] (2026-11-09) Milestone 2: relay protocol 57. `Attach` names `known_state_digest`; the worker answers `AttachedWithKnownState` when it matches and otherwise `Attached` with `state_digest`. `RelayClient` keeps the last state with its digest. The worker's harness preparation now joins the state before the digest (it used to be overlaid on the reply afterwards). Tests `an_attach_skips_a_state_the_controller_already_holds` and `a_changed_preparation_is_never_taken_for_a_known_state`.
- [ ] Milestone 3: relay protocol 58 `WaitForChange` on a dedicated connection; session actors stop timer polling for workers that support it.
- [ ] Measure after installation and record the outcome.


## Surprises & Discoveries

- Observation: an earlier attempt (2026-11-08) replaced the sweep with an in-memory "did a session leave since the last sweep" check. Two reviews found holes: a GitHub classification finishing after its session was destroyed, and a session created and destroyed between two sweeps. Both are races between writers and the cleanup. Evidence: commits 41216e984 and 8249d6ede, reverted by 1ac078ddc and b04cab920.

- Observation: the worker builds `RelayOperationalState` on demand (`DurableRelay::operational_state` in `mj-worker/src/relay.rs`) from many fields: the durable snapshot, ACP activity clocks, foreground tools, terminals and more. There is no single mutation point, so a revision counter would be fragile; comparing a digest of the serialized state is robust.

- Observation: every change to the worker's relay state takes the lock on `Arc<Mutex<DurableRelay>>` (117 lock sites in 7 files). Notifying on every lock release is therefore a complete change signal without listing mutation points.

- Observation: a worker speaks only the current relay protocol (`envelope_rejection` negotiates exactly `RELAY_PROTOCOL_VERSION`), so a worker never serves an older controller. Only the controller needs version checks, because it talks to older workers until they are replaced.

- Observation: the worker's connection loop overlaid `harness_preparation` onto attach replies after the relay built them (`overlay_preparation_state`). With digests that would hide a preparation change; the preparation now joins the state before the digest (`DeferredRelayAttach::set_harness_preparation`). A session-manager test fake had the same pattern and now clears digests when it edits state.


## Decision Log

- Decision: orphan cleanup by SQLite triggers in migration 121, not by any daemon-side bookkeeping.
  Rationale: all writes go through SQLite, so a trigger runs inside the writing transaction and no ordering of writers can leave a row behind; the daemon-side attempt kept finding races (see Surprises).
  Date/Author: 2026-11-09, Claude.

- Decision: an insert into `mailbox_outbox` or the GitHub watch tables for a session that is missing or destroyed is ignored (`RAISE(IGNORE)`), and `enqueue_mailbox_event_with` reports such an insert as "not inserted" instead of failing.
  Rationale: the target is gone, so there is no one to deliver to; today the row is inserted and deleted by the next sweep, which is the same outcome later.
  Date/Author: 2026-11-09, Claude.

- Decision: "unchanged state" means equal SHA-256 digests of the state's JSON serialization, computed by the worker; the daemon sends back the digest of the state it holds.
  Rationale: robust against any field changing, costs the worker one hash of bytes it already produces, and needs no revision bookkeeping.
  Date/Author: 2026-11-09, Claude.

- Decision: milestones 2 and 3 ship separately, so milestone 2 is protocol 57 and milestone 3 protocol 58.
  Rationale: each protocol bump must match one released behavior.
  Date/Author: 2026-11-09, Claude.

- Decision: "push" is a long-held request, `WaitForChange`, on a separate relay connection per session, answered when the worker's latest event ordinal or state digest differs from what the daemon sent, or after a bounded wait so a lost connection is noticed. The worker wakes waiters whenever the relay lock is released.
  Rationale: the relay transport is request/response; a held request needs no new framing, and a separate connection keeps the session's main connection free for commands.
  Date/Author: 2026-11-09, Claude.


## Outcomes & Retrospective

(To be written as milestones complete.)


## Context and Orientation

The daemon owns a SQLite database (`~/.local/share/mjolnir/mj.sqlite3`). Schema changes are numbered migrations in `mj-controller/src/database/schema.rs`, applied in order at startup; `SCHEMA_VERSION` in `mj-controller/src/database.rs` names the newest. Each migration carries a comment saying whether it is compatible (older daemons can keep using the store) or breaking (it raises `minimum_compatible_version`). New stores start from `mj-controller/src/database/baseline.sql` at revision 33 and then run every migration, so a migration covers new and existing stores alike.

The mailbox outbox (`mailbox_outbox`, column `target_session_id`) holds messages to deliver to sessions. The GitHub watch tables (`github_watch_items.creator_session_id`, `github_watch_classifications.session_id`, `github_watch_classification_failures.session_id`) hold per-session GitHub tracking. A session is gone when its row in `sessions` is deleted or its `state` is `destroyed-with-data-loss`. Today `prune_mailbox_events_for_missing_sessions` in `mj-controller/src/database/mailbox.rs` deletes rows of gone sessions; `mj-controller/src/mailbox_outbox.rs` calls it from `reconcile_durable_state` on every five-second sweep.

Each session has a worker process (`mj-worker`) reachable over a relay connection, a newline-delimited JSON request/response protocol defined in `mj-core/src/relay/protocol.rs` (`RelayRequest`, `RelayResponsePayload`) with its version constants in `mj-core/src/relay.rs` (`RELAY_PROTOCOL_VERSION`, currently 56). Each request carries the protocol version; both sides keep older behavior for older peers. In the daemon, `mj-controller/src/session_manager/actor.rs` runs one actor per session; its loop syncs on a deadline from `sync_delay` (`SESSION_SYNC_INTERVAL` 150 ms, `QUIET_SESSION_SYNC_INTERVAL` 2 s, in `mj-controller/src/session_manager.rs`). A sync is `StandaloneSession::sync_in_place` in `mj-controller/src/session_manager/standalone.rs`, which sends `Attach { after_ordinal, after_digest }` and receives `Attached { state, events, through_ordinal, through_digest }` through `RelayClient::attach` in `mj-controller/src/worker_client/relay.rs`. In the worker, `mj-worker/src/worker_runtime/unix/requests.rs::handle_request` serves `Attach` via `DurableRelay::take_deferred_attach` (`mj-worker/src/relay/replay.rs`), and the relay state lives in `Arc<Mutex<DurableRelay>>`.


## Plan of Work

Milestone 1 adds migration 121. It creates triggers: after a `sessions` row is deleted, and after its `state` becomes `destroyed-with-data-loss`, delete that session's rows from the four tables; before an insert into any of the four tables, ignore it when the referenced session is missing or destroyed. It then deletes existing orphans once. `enqueue_mailbox_event_with` and `enqueue_github_event` treat an ignored insert (no row afterwards) as not inserted. The sweep's call to `prune_mailbox_events_for_missing_sessions` and the function itself are removed. Tests that enqueue for sessions they never saved must save those sessions. The migration is compatible: older daemons keep reading and writing the same columns; they only find orphans already gone.

Milestone 2 bumps the relay protocol to 57. `RelayRequest::Attach` gains an optional `known_state_digest`. For protocol 57 the worker computes the digest of the state it would send and, when it equals `known_state_digest`, replies with `Attached` whose `state` is omitted and which always carries `state_digest`. The daemon keeps the digest of the state it holds and reuses that state when a reply omits it. Older workers ignore the new field and keep sending state; older daemons never send it.

Milestone 3 adds `RelayRequest::WaitForChange { after_ordinal, state_digest }` for protocol 57. The worker wraps its relay mutex in a small type whose guard notifies waiters when released; the `WaitForChange` handler recomputes the latest ordinal and state digest after each notification and answers `Changed` when either differs, or `Unchanged` after a bounded wait. The daemon's actor opens a second relay connection for waits when the worker speaks protocol 57, issues a wait after each sync, and syncs when the wait answers or a command arrives, instead of on the 150 ms and 2 s timer. Workers below 57 keep the timer.


## Concrete Steps

Work in the repository root. Build and test with the dev profile; use targeted tests (`cargo test -p brokk-mj-controller --lib -- <filters>`, `cargo test -p brokk-mj-worker --lib -- <filters>`, `cargo test -p brokk-mj-core --lib -- relay`) and `cargo clippy -p <crates> --all-targets -- -D warnings`. Commit each milestone separately, request review, push to `origin/master`, then install with `RUSTFLAGS="-C force-frame-pointers=yes" scripts/install.sh` and trigger the handoff with an ordinary `mj` command such as `mj sessions`.


## Validation and Acceptance

Milestone 1: a test destroys a session that has outbox, watch and classification rows and finds them gone in the same transaction; an insert for a missing session leaves no row and the enqueue reports it as not inserted. After installation, `sqlite3 'file:...mj.sqlite3?mode=ro' "select count(*) from mailbox_outbox where target_session_id not in (select session_id from sessions where state <> 'destroyed-with-data-loss')"` prints 0, and `prune_mailbox_events_for_missing_sessions` no longer appears in a profile.

Milestone 2: a relay test attaches twice with no change and receives a reply without state the second time, with the same digest; a change in between brings the state back. An older-protocol attach still receives state.

Milestone 3: a relay test holds a `WaitForChange` and sees it answered when an event is recorded, and answered `Unchanged` after the bounded wait otherwise. Live: after installation, a profile shows far fewer `Attach` decodes, and daemon CPU at a similar session count is below the 43% baseline.


## Idempotence and Recovery

The migration uses `CREATE TRIGGER IF NOT EXISTS` and an idempotent orphan delete, inside one transaction with the revision bump, so a crash leaves either revision 120 or 121. Protocol changes are additive and version-gated; a mixed daemon/worker pair falls back to protocol 56 behavior.


## Artifacts and Notes

Baseline profile: `/mnt/optane/tmp/mj-cpu-20261106/after5.data` (43% steady, 50 running, 2026-11-09).


## Interfaces and Dependencies

`mj-core/src/relay/protocol.rs`: `RelayRequest::Attach { after_ordinal: u64, after_digest: String, known_state_digest: Option<String> }` (field omitted when `None`); `RelayResponsePayload::Attached { state: Option<RelayOperationalState>, state_digest: Option<String>, .. }` serialized so that protocol-56 peers see today's shape; `RelayRequest::WaitForChange { after_ordinal: u64, state_digest: String }`; `RelayResponsePayload::Changed` and `RelayResponsePayload::Unchanged`. `mj-core/src/relay.rs`: `RELAY_CHANGE_WAIT_PROTOCOL: u32 = 57` and `RELAY_PROTOCOL_VERSION` set to it.
```
