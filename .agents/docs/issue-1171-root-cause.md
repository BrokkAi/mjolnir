# Issue #1171: delayed delegation dispatch

Investigated 2026-09-27 against incident build
`389a166619e24aa561870bae00c2d2aa32b8e4b8` and checkout `abe49acb`.
Scope: diagnosis only; no runtime changes or live-session mutations.

## Finding and confidence

The session-update coalescer has a reproducible starvation defect. It stores
updates in a `BTreeMap` keyed by session ID and always removes `pop_first()`.
A busy smaller-ID session can repeatedly reinsert itself ahead of an already
waiting larger-ID session. Replacing the larger session's payload does not
improve its position. There is no bound on how long it can be bypassed.

This is the leading explanation for this incident, supported by continued
parent transcript replication, timely dispatch for smaller-ID parents, and
the delayed parent's accumulated requests completing together after the
benchmark was interrupted. The defect is proven; its exact contribution to
the historical 161.604-second delay is not directly measured. The incident
logs lack enqueue/dequeue timestamps and lease ownership spans, so they
cannot identify which coalescer held the parent or exclude all other delay.

## Code path

- `session_manager/standalone.rs::sync_in_place` polls durable subagent state
  and detects changes independently of the transcript.
- `session_manager/actor.rs::view_is_unchanged` compares subagent requests and
  results. Both checks were already present in the incident build.
- `session_manager/channels.rs::CoalescedUpdateSender::send` replaces the
  pending payload by session ID; `SessionManagerUpdates::pop_pending` removes
  the lexicographically smallest ID, irrespective of waiting time.
- This coalescer appears three times on the delegation observation path:
  actor to continuation service, continuation service to daemon process loop,
  and phone remote-session facade to server runtime.
- `daemon/process.rs` forwards the continuation service's updates through
  `RemoteSessionPublisher`; `session_manager/remote.rs` republishes those
  through the third coalescer.
- `server_runtime/run.rs` only calls `subagent_dispatch.observe` after
  receiving that last update. Independent dispatch jobs cannot help requests
  that have not reached this point.

The relevant coalescer, continuation, standalone polling, and dispatch files
have no differences between the incident commit and this checkout.

## Incident evidence

All times below are UTC, 2026-09-27. Parent:
`d8305df3f8f5e314f1c01de51778ee8b`. Request:
`55e7b8173881b8a42e905964e0488d30`.

| Observation | Time |
| --- | --- |
| Parent starts spawn tool | 20:02:36.250 |
| Monitor sees locally materialized parent activity from 20:02:36.252 | 20:02:39.047 |
| Worker answers overdue earlier wait locally | 20:02:52.953 |
| Monitor sees locally materialized parent activity from 20:03:15.888 | 20:03:20.338 |
| Spawn caller times out | 20:04:37.311 |
| Monitor sees locally materialized parent activity from 20:04:38.600 during its sample | sample began 20:04:38.417 |
| Supervisor interrupts parent turn | 20:04:52.309 |
| Daemon begins two accumulated waits | 20:05:14.145 / 20:05:14.167 |
| List result completion timestamp | 20:05:15.254 |
| Spawn result completion timestamp | 20:05:17.854 |
| Child first turn starts, per issue evidence | 20:05:35.997 |

The monitor reads `materialized_sessions` directly from SQLite (see
`brokkbench26/runbooks/judge/watch27.py`); these are evidence of relay
materialization continuing, not proof that dashboard updates were delivered.
The sample timestamp is recorded before its collection finishes.

Meanwhile, smaller-ID parents had waits start with 239/240 seconds remaining
(`223f...`, 20:03:10.451), 57/60 (`3b7d...`, 20:03:10.536), and 56/60
(`3b7d...`, 20:04:33.174). This was not a single global dispatch freeze.

The affected parent's daemon log has no slow-relay warnings or connection
failures during the delayed request interval. The checkpoint prestage took
17.721 seconds ending at 20:05:09.846, after the caller had already timed out;
its barrier was held about 9.691 seconds. Neither duration explains the
earlier request delay.

Sources inspected:

- `/home/jonathan/.local/share/mjolnir/logs/mj-daemon-20260927T170957.085Z-1550843.log`
- Parent native archive ending `419-archive-ee70f0a15dfcf4496a539be69aa73d84.hel.zip`
- `brokkbench26/evidence/eval27-20260927/astramedluna/monitoring/health-*.json`
- Preserved parent `worker.log` and `subagents.json`, read through Podman on
  Morannon, under `/var/lib/hel/workers/d8305df3f8f5e314f1c01de51778ee8b/`

## Reproduction and limits

An isolated Rust harness compiled with `rustc --edition=2024` uses the actual
repository bodies of `send` and `pop_pending`, with stub wake notifications
and upgrade guards. It queues the affected parent, then inserts a smaller-ID
update before each of 100,000 receives. Every receive selects the smaller
ID. Updating the parent's payload every tenth iteration does not help. The
parent is finally returned, with its latest payload, only after the smaller
publisher stops. Harness and binary are in
`/mnt/optane/mjolnir-1171-analysis/`. This proves the scheduling defect, not
the historical workload's exact timing; it is not a full daemon regression.

Intermittent network trouble could reduce throughput and aggravate pressure
on the update pipeline, but no direct network stall is demonstrated here.
The reproduction needs no network. The warning storm in #1172 is another
possible source of overhead; causation and magnitude remain unmeasured.

## Corrective direction

Preserve one latest payload per session while scheduling pending sessions
fairly. For example, a FIFO of distinct pending session IDs and their latest
payload map must share one owner/lock. Replacing a payload must retain the
session's existing queue position; publishing after delivery goes to the
tail. This removes the possibility of smaller IDs overtaking indefinitely.

Add a regression that keeps smaller-ID sessions active while asserting a
waiting parent's latest delegation state reaches dispatch within a bounded
number of dequeues. Cover all uses of the shared coalescer and retain
request identity/eventual completion after caller timeout. Add queue-age and
request-stage tracing to distinguish transport, publication, scheduling,
profile discovery, registration, and result delivery in future incidents.
Increasing the tool timeout does not repair starvation.
