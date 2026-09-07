# Reduce capacity polling CPU usage

This ExecPlan follows `.agents/PLANS.md` and remains a living record of implementation and validation.

## Purpose / Big Picture

The mj background process (daemon) should update capacity readings without repeatedly scanning unused CPU frequencies or launching overlapping probes. PID 2159028 used about 4.5 CPU cores during a 30-second measurement, with about 3.7 cores in 120 Rayon parallel workers. The user authorized optimizing this path. The running daemon will remain undisturbed while isolated measurements and tests validate the changes.

## Progress

- [x] (2026-09-06) Identify unnecessary CPU frequency scans and unchanged target publication in source; measure CPU by live thread pool.
- [x] (2026-09-06) Measure live capacity probe invocation rate and reproduce scanning cost independently.
- [x] (2026-09-06) Implement deduplicated publication, supervised per-target probes, and usage-only CPU sampling.
- [x] (2026-09-06) Validate behavior, measure the optimized sampling cost, run required checks, and review the changes for the current-branch commit.

## Surprises & Discoveries

The process has 120 Tokio asynchronous workers and 120 Rayon parallel workers. Six five-second CPU measurements attributed about 81% of CPU to Rayon. The nominal capacity interval is 30 seconds, but `watch` channel publications also start probes. `server.rs` republishes identical targets after controller reloads. `sysinfo` 0.32 scans CPU frequency in parallel when `refresh_cpu_all` is used; the reported capacity never uses frequency. On this host the per-core frequency files are absent, causing each worker to read `/proc/cpuinfo` instead.

A 15-second automatically continuing debugger breakpoint on `collect_local_capacity` confirmed repeated starts within each second, including starts separated by tens of milliseconds. The debugger detached and `/proc/2159028/status` confirmed `TracerPid: 0`.

An isolated unoptimized Rust executable linked to the existing sysinfo dependency collected 30 local samples per mode. The original path consumed 11.61 user plus 10.81 system CPU-seconds (22.42 total), versus 0.00 user plus 0.08 system CPU-seconds for usage-only sampling: about 99.6% less CPU for this operation. Wall times were 6.71 and 6.14 seconds because both retain the minimum utilization sampling delay. This does not measure a restarted live daemon.

## Decision Log

On 2026-09-06, use usage-only sysinfo refresh rather than changing global Rayon or Tokio pool sizes. This removes unused work without restricting unrelated parallel operations. Suppress unchanged target publication at the phone server and defensively ignore unchanged targets in the poller.

On 2026-09-06, keep one supervised probe per target. Periodic and manual requests while that target is busy share its current probe. Changed target configuration is sampled after the old probe finishes, and stale results are discarded. Different targets remain concurrent. Keep ownership of local blocking samples until they finish, including after a timeout, so timeouts cannot create overlapping detached samples. Tests drive the actual asynchronous poller using hand-written controlled futures.

On 2026-09-06, revalidate the latest target only after reserving output-channel space, with no await between validation and publication. Review identified that a result waiting on a full output channel could otherwise become stale. Phone refresh uses nonblocking `try_send`, treating a full trigger channel as coalesced success and a closed channel as failure; awaiting this send could form a backpressure cycle with result delivery.

On 2026-09-06, retain the explicit limitation that blocking OS reads cannot be forcibly cancelled. A timed-out local sample keeps its slot until it returns, while other targets remain independent. On shutdown its awaiting async task is cancelled and the read is allowed to finish; its errors are logged inside the blocking closure. Do not attempt unsafe thread termination or add unrelated global-runtime changes to this optimization.

## Outcomes & Retrospective

The implementation is complete and validated. Two full default-workspace `cargo test` runs passed. Following final backpressure refinements, `cargo test -q -p brokk-mjolnir` passed all 149 CLI unit tests and five enabled integration tests, with four opt-in import tests ignored by default. Final `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed. The source and this record are ready for the required current-branch commit; no push or live daemon restart is part of this change.

Behavior tests now cover periodic and manual refresh, unchanged publication, simultaneous independent targets, coalesced busy-target requests, changed/removed configurations, panic reporting and retry, cancellation when the consumer closes, retention of a timed-out blocking sample, and revalidation after a full result channel. The isolated operation benchmark showed about 99.6% less CPU per sample. The live daemon still runs the original executable, so a whole-process post-restart CPU reduction has not been measured.

## Context and Orientation

`mj-cli/src/pollers.rs` owns background capacity collection and returns watch, trigger, and update channels to UI consumers. A watch channel stores the latest list of capacity targets, each identified by its `id`. `mj-cli/src/server.rs` publishes that list for the phone/web surface. `DeploymentCapacityTarget` and `DeploymentCapacityUsage` live in `src/hel_targets.rs`. Local collection currently creates a sysinfo System for every sample and reads CPU counters twice, separated by the minimum sampling delay. Remote samples run subprocesses. No new dependencies or public interfaces are needed.

## Plan of Work

First record bounded live invocation evidence and benchmark old versus usage-only refresh with the same dependency/build mode. Next replace detached capacity jobs with a JoinSet (Tokio's owned collection of tasks), tracking the sampled target for each task ID. Reconcile target changes, suppress duplicates, handle task failures with target context, and stop tasks when channels close. A test-only-capable collector parameter allows tests to hold a probe open, change targets, and verify outputs without real remote work. Luna owns the independent server publication edit; the main agent owns poller concurrency and integration.

## Concrete Steps

Work from `/home/jonathan/Projects/hel`. Use bounded debugger capture for PID 2159028, with automatic detach, and store diagnostic artifacts under `/tmp`. Run focused `cargo test -p brokk-mjolnir capacity` outside the sandbox, followed by required `cargo test` and `cargo clippy --all-targets -- -D warnings`. Run `cargo fmt --all -- --check` and review `git diff --check` before staging only changed files and committing on the current branch. Do not push.

## Validation and Acceptance

Repeated identical target publications must not start new probes; the 30-second timer and explicit refresh still must. Requests arriving during a probe must not overlap it, while different targets can run together. A changed target must be sampled next and must never receive stale results from its previous configuration. Removed targets must not publish late results. A failed or panicked probe must report a contextual failure and permit subsequent retry. Dropping the output receiver must stop supervised async work. CPU utilization, memory, and core count must still be returned after removing frequency scans. An isolated before/after measurement should show lower CPU time for local collection; the live daemon remains the baseline until replacement is authorized as a separate operational step.

## Idempotence and Recovery

The change does not migrate data or alter configuration. Repeat tests freely using their temporary fixtures. Preserve unrelated changes, do not restart the live process, and do not change branches. Local blocking samples cannot be forcibly cancelled; retain their slot through completion after timeout and let normal bounded collection finish on shutdown.

## Artifacts and Notes

Existing captures are `/tmp/mj-2159028-perf.data`, `/tmp/mj-2159028-deep.data`, and `/tmp/mj-2159028-gdb.txt`. Perf caller unwinding was unreliable, but live debugger stacks identified the thread pools. The reproducible pool sampler is `/tmp/mj_cpu_pools.py`. Measurements and final validation outcomes will be added here.

The operation benchmark source is `/tmp/mj_capacity_bench.rs`, built with `rustc --edition=2024 /tmp/mj_capacity_bench.rs --extern sysinfo=target/debug/deps/libsysinfo-0f7a8b272850bdee.rlib -L dependency=target/debug/deps -o target/mj-capacity-bench`. Run `/usr/bin/time -f 'user=%U system=%S elapsed=%e' target/mj-capacity-bench old` and repeat with `new`. Its assertions check that CPU count and memory remain available; its black-box read consumes the utilization value. The executable uses normal build storage, not `/tmp`.

## Interfaces and Dependencies

Preserve `spawn_dashboard_capacity_poller` and `CapacityPollUpdate` for callers. Use existing Tokio task/channel/time primitives and `sysinfo::System::refresh_cpu_usage`. Keep production collection errors as `Result<Option<DeploymentCapacityUsage>>` and convert to the existing update error string at the scheduler boundary. No new crate or dependency is planned.

Revision note: Created on 2026-09-06 from the measured CPU investigation and accepted optimization sequence.

Revision note: Updated on 2026-09-06 with live probe-start evidence, independent before/after measurements, completed implementation, and validation progress.

Revision note: Updated on 2026-09-06 with review-driven backpressure handling, the blocking-read shutdown limitation, and successful full-suite validation before the last scoped refinements.

Revision note: Finalized on 2026-09-06 after all CLI tests, Clippy, formatting, and diff checks passed on the reviewed implementation.
